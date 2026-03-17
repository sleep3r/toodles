mod config;
mod handlers;
mod session;
mod setup;
mod transcription;

use std::sync::Arc;

use clap::Parser;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use config::Config;
use handlers::{
    message::{handle_text, send_reply},
    session_key,
    voice::handle_voice,
};
use session::SessionManager;
use transcription::LocalTranscriber;

// ──────────────────────────────────────────────────────────────────────────────
// Bot commands
// ──────────────────────────────────────────────────────────────────────────────

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
enum Cmd {
    #[command(description = "Introduce the bot.")]
    Start,
    #[command(description = "Reset the current gemini-cli session.")]
    New,
    #[command(description = "Show active session count.")]
    Status,
    #[command(description = "Show this help message.")]
    Help,
}

async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Cmd,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = match msg.from.as_ref() {
        Some(u) => u.id.0,
        None => return Ok(()),
    };

    if !config.is_user_allowed(user_id) {
        send_reply(&bot, &msg, "⛔ You are not authorised to use this bot.").await?;
        return Ok(());
    }

    match cmd {
        Cmd::Start => {
            send_reply(
                &bot,
                &msg,
                "👋 Welcome to Toodles!\n\n\
                 I'm a Telegram wrapper for gemini-cli. \
                 Send me any message and I'll forward it to Gemini AI and stream the response back.\n\n\
                 🎙 Voice messages are automatically transcribed via Whisper before being forwarded.\n\n\
                 📌 Each forum topic gets its own isolated session.\n\n\
                 /new — Start a fresh session\n\
                 /help — Show all commands",
            )
            .await?;
        }
        Cmd::New => {
            let key = session_key(&msg);
            sessions.reset(&key).await;
            send_reply(&bot, &msg, "🔄 Session reset. Starting fresh!").await?;
        }
        Cmd::Status => {
            let count = sessions.session_count();
            send_reply(
                &bot,
                &msg,
                &format!("📊 Active sessions: {count}"),
            )
            .await?;
        }
        Cmd::Help => {
            send_reply(&bot, &msg, &Cmd::descriptions().to_string()).await?;
        }
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────────────

/// Command-line arguments.
#[derive(Parser)]
#[command(name = "toodles", about = "Telegram bot wrapper for gemini-cli")]
struct Cli {
    /// Run interactive setup wizard to generate .env configuration.
    #[arg(long)]
    setup: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if cli.setup {
        if let Err(e) = setup::run_setup() {
            eprintln!("Setup failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // Load .env file if present (ignores missing file).
    dotenvy::dotenv().ok();

    // Initialise structured logging; respect RUST_LOG env var.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().expect("Failed to load configuration");
    let config = Arc::new(config);

    info!(
        gemini_cli = %config.gemini_cli_path,
        allowed_users = config.allowed_user_ids.len(),
        local_transcription = config.use_local_transcription,
        "Starting Toodles bot"
    );

    // Load local transcription engine if configured.
    let local_transcriber: Option<Arc<Mutex<LocalTranscriber>>> =
        if config.use_local_transcription {
            // Download model if not present yet.
            if !transcription::is_model_downloaded(&config.models_dir) {
                info!("Parakeet model not found — downloading…");
                if let Err(e) = transcription::download_model(&config.models_dir).await {
                    error!("Failed to download Parakeet model: {e:#}");
                    warn!("Local transcription disabled — falling back to Whisper API");
                    None
                } else {
                    match LocalTranscriber::load(&config.models_dir) {
                        Ok(t) => {
                            info!("Parakeet model loaded");
                            Some(Arc::new(Mutex::new(t)))
                        }
                        Err(e) => {
                            error!("Failed to load Parakeet model: {e:#}");
                            warn!("Local transcription disabled — falling back to Whisper API");
                            None
                        }
                    }
                }
            } else {
                match LocalTranscriber::load(&config.models_dir) {
                    Ok(t) => {
                        info!("Parakeet model loaded");
                        Some(Arc::new(Mutex::new(t)))
                    }
                    Err(e) => {
                        error!("Failed to load Parakeet model: {e:#}");
                        warn!("Local transcription disabled — falling back to Whisper API");
                        None
                    }
                }
            }
        } else {
            None
        };

    let bot = Bot::new(&config.telegram_bot_token);
    let sessions = Arc::new(SessionManager::new(config.clone()));

    let handler = Update::filter_message()
        // 1. Commands (must be checked before the plain-text handler).
        .branch(
            Message::filter_text()
                .filter_command::<Cmd>()
                .endpoint(command_handler),
        )
        // 2. Voice messages.
        .branch(Message::filter_voice().endpoint(handle_voice))
        // 3. Plain text messages forwarded to gemini-cli.
        .branch(Message::filter_text().endpoint(handle_text));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, sessions, local_transcriber])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
