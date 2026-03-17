mod aggregator;
mod config;
mod handlers;
mod session;
mod setup;
mod telegram_api;
mod transcription;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use aggregator::MessageAggregator;
use config::Config;
use handlers::{
    document::handle_document,
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
#[command(rename_rule = "lowercase", description = "Вот что я умею:")]
enum Cmd {
    #[command(description = "Познакомиться 👋")]
    Start,
    #[command(description = "Начать с чистого листа 🔄")]
    New,
    #[command(description = "Статус бота 📊")]
    Status,
    #[command(description = "Показать команды 💡")]
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
                "🐩 Гав! Я — Toodles, твой AI-ассистент.\n\n\
                 Просто напиши мне что угодно, и я отвечу! \
                 Можешь задавать вопросы, просить помощь с кодом, переводами — чем угодно.\n\n\
                 🎙 Голосовые сообщения тоже понимаю — расшифрую и отвечу.\n\
                 📄 Файлы тоже принимаю — пришли документ, и я разберусь!\n\n\
                 /new — Начать заново\n\
                 /help — Все команды",
            )
            .await?;
        }
        Cmd::New => {
            let key = session_key(&msg);
            sessions.reset(&key).await;
            send_reply(&bot, &msg, "Готово! Начинаем с чистого листа.").await?;
        }
        Cmd::Status => {
            let count = sessions.session_count();
            send_reply(
                &bot,
                &msg,
                &format!("📊 Активных диалогов: {count}"),
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

    // Register commands with Telegram so they show in the command menu.
    if let Err(e) = bot.set_my_commands(Cmd::bot_commands()).await {
        error!("Failed to register bot commands: {e}");
    } else {
        info!("Bot commands registered with Telegram");
    }

    let sessions = Arc::new(SessionManager::new(config.clone()));
    let aggregator = Arc::new(MessageAggregator::new(Duration::from_millis(1500)));

    let handler = Update::filter_message()
        // 1. Commands (must be checked before the plain-text handler).
        .branch(
            Message::filter_text()
                .filter_command::<Cmd>()
                .endpoint(command_handler),
        )
        // 2. Voice messages.
        .branch(Message::filter_voice().endpoint(handle_voice))
        // 3. Document messages (files with optional caption).
        .branch(Message::filter_document().endpoint(handle_document))
        // 4. Plain text messages forwarded to gemini-cli.
        .branch(Message::filter_text().endpoint(handle_text));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, sessions, local_transcriber, aggregator])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
