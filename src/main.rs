mod acp;
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
    handle_stop_callback,
    message::{handle_text, send_reply},
    photo::handle_photo,
    session_key,
    voice::handle_voice,
    CancelRegistry, QueryRegistry,
};
use session::SessionManager;
use transcription::LocalTranscriber;

// ──────────────────────────────────────────────────────────────────────────────
// Bot commands
// ──────────────────────────────────────────────────────────────────────────────

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Here's what I can do:")]
enum Cmd {
    #[command(description = "Get started 👋")]
    Start,
    #[command(description = "Start fresh 🔄")]
    New,
    #[command(description = "Bot status 📊")]
    Status,
    #[command(description = "Create forum thread 🧵")]
    Thread,
    #[command(description = "Show commands 💡")]
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

    let key = session_key(&msg);

    match cmd {
        Cmd::Start => {
            send_reply(
                &bot,
                &msg,
                "🐩 Woof! I'm Toodles, your AI assistant.\n\n\
                 Just write me anything and I'll respond! \
                 Ask questions, get help with code, translations — anything.\n\n\
                 🎙 I understand voice messages — I'll transcribe and reply.\n\
                 📄 Send me files too — I'll figure them out!\n\n\
                 /thread — Create a new forum thread\n\
                 /new — Start fresh\n\
                 /help — All commands",
            )
            .await?;
        }
        Cmd::New => {
            sessions.reset(&key).await;
            send_reply(&bot, &msg, "Done! Starting fresh.").await?;
        }
        Cmd::Status => {
            let count = sessions.session_count();
            let thread = msg
                .thread_id
                .map(|t| t.0 .0.to_string())
                .unwrap_or_else(|| "none".to_string());
            let draft_mode = match config.draft_mode {
                crate::config::DraftMode::Compact => "compact",
                crate::config::DraftMode::Verbose => "verbose",
            };
            send_reply(
                &bot,
                &msg,
                &format!(
                    "📊 Active sessions: {count}\n🧵 Thread: {thread}\n🤖 Agent: gemini-cli\n📝 Draft mode: {draft_mode}\n🏷 Rename topic every: {} msgs",
                    config.thread_rename_every
                ),
            )
            .await?;
        }
        Cmd::Thread => {
            // Telegram currently allows only a fixed color set for forum topics.
            const TOPIC_ICON_COLOR: u32 = 0x6FB9F0;
            let icon_custom_emoji_id = bot
                .get_forum_topic_icon_stickers()
                .await
                .ok()
                .and_then(|stickers| {
                    stickers
                        .first()
                        .and_then(|s| s.custom_emoji_id().map(|id| id.to_string()))
                })
                .unwrap_or_default();

            match bot
                .create_forum_topic(
                    msg.chat.id,
                    "Новая сессия",
                    TOPIC_ICON_COLOR,
                    icon_custom_emoji_id,
                )
                .await
            {
                Ok(topic) => {
                    let topic_key = (msg.chat.id.0, Some(topic.thread_id.0 .0));
                    sessions.reset(&topic_key).await;

                    // Prewarm ACP session for this topic in background so the
                    // first real prompt starts faster.
                    let sessions_for_prewarm = sessions.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sessions_for_prewarm.get_or_create(topic_key).await {
                            warn!("Failed to prewarm topic session: {e}");
                        }
                    });

                    let _ = bot
                        .send_message(
                            msg.chat.id,
                            "🐩 Thread is ready. Send your prompt here and I'll keep context isolated for this topic.",
                        )
                        .message_thread_id(topic.thread_id)
                        .await;

                    send_reply(
                        &bot,
                        &msg,
                        &format!(
                            "🧵 Forum thread created: `{}`. I also sent a welcome message into that topic.",
                            topic.name
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    send_reply(
                        &bot,
                        &msg,
                        &format!(
                            "⚠️ Could not create thread: {e}. Make sure this is a forum-enabled supergroup and bot has admin rights for topics."
                        ),
                    )
                    .await?;
                }
            }
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
        gemini_cmd = %config.gemini_cli_command,
        gemini_yolo = config.gemini_yolo,
        draft_mode = ?config.draft_mode,
        thread_rename_every = config.thread_rename_every,
        allowed_users = config.allowed_user_ids.len(),
        local_transcription = config.use_local_transcription,
        "Starting Toodles bot"
    );

    // Load local transcription engine if configured.
    let local_transcriber: Option<Arc<Mutex<LocalTranscriber>>> = if config.use_local_transcription
    {
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
    let cancel_registry: CancelRegistry = Arc::new(dashmap::DashMap::new());
    let query_registry: QueryRegistry = Arc::new(dashmap::DashMap::new());

    let message_handler = Update::filter_message()
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
        // 4. Photo messages.
        .branch(Message::filter_photo().endpoint(handle_photo))
        // 5. Plain text messages forwarded to gemini-cli.
        .branch(Message::filter_text().endpoint(handle_text));

    let callback_handler = Update::filter_callback_query().endpoint(handle_stop_callback);

    let handler = dptree::entry()
        .branch(message_handler)
        .branch(callback_handler);

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            config,
            sessions,
            local_transcriber,
            aggregator,
            cancel_registry,
            query_registry
        ])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
