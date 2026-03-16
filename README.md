# toodles

A Telegram bot written in Rust that wraps [`gemini-cli`](https://github.com/google-gemini/gemini-cli), letting you chat with Gemini AI directly from Telegram — with streaming responses, voice-message transcription, and per-topic session isolation.

---

## Features

| Feature | Details |
|---|---|
| 💬 **Text messages** | Forwarded to your gemini-cli session; streamed back line-by-line |
| 🎙 **Voice messages** | Transcribed via OpenAI Whisper, then forwarded to Gemini |
| 📌 **Forum topics** | Each Telegram topic gets an isolated gemini-cli session |
| 🔄 **Session reset** | `/new` kills the current session and starts a fresh one |
| 📊 **Status** | `/status` shows the number of active sessions |
| 🔒 **User allowlist** | Optional `ALLOWED_USER_IDS` restricts access to specific users |

---

## Prerequisites

1. **Rust** ≥ 1.70 (install via [rustup](https://rustup.rs))
2. **gemini-cli** — install the Google Gemini CLI:
   ```sh
   npm install -g @google/gemini-cli
   gemini  # log in and authenticate on first run
   ```
3. A **Telegram bot token** — create one via [@BotFather](https://t.me/BotFather)
4. *(Optional)* An **OpenAI API key** for Whisper voice transcription

---

## Quick start

```sh
# 1. Clone and enter the repo
git clone https://github.com/sleep3r/toodles
cd toodles

# 2. Create .env from the example
cp .env.example .env
$EDITOR .env          # fill in TELEGRAM_BOT_TOKEN at minimum

# 3. Build and run
cargo run --release
```

---

## Configuration

All configuration is done through environment variables (or a `.env` file):

| Variable | Required | Default | Description |
|---|---|---|---|
| `TELEGRAM_BOT_TOKEN` | ✅ | — | Bot token from @BotFather |
| `ALLOWED_USER_IDS` | ❌ | *(all)* | Comma-separated Telegram user IDs |
| `GEMINI_CLI_PATH` | ❌ | `gemini` | Path to the gemini-cli binary |
| `GEMINI_WORKING_DIR` | ❌ | *(cwd)* | Working directory for gemini-cli |
| `OPENAI_API_KEY` | ❌ | — | OpenAI key for Whisper transcription |
| `RUST_LOG` | ❌ | `info` | Log level (`trace`, `debug`, `info`, …) |

---

## Bot commands

| Command | Description |
|---|---|
| `/start` | Introduction and quick-start guide |
| `/new` | Reset the current gemini-cli session |
| `/status` | Show the number of active sessions |
| `/help` | List all available commands |

---

## Architecture

```
src/
├── main.rs          — entry point; Telegram dispatcher and command routing
├── config.rs        — Config struct, loaded from environment variables
├── session.rs       — Session (gemini-cli subprocess) and SessionManager
└── handlers/
    ├── mod.rs       — shared helpers (session_key, truncate_for_telegram)
    ├── message.rs   — text-message handler (streaming output)
    └── voice.rs     — voice-message handler (Whisper → gemini-cli)
```

Each chat/topic maps to a single long-lived `gemini-cli` subprocess.
Queries are serialised per session via a `tokio::sync::Mutex`.
Responses are streamed back to Telegram by editing a placeholder message every 500 ms.

---

## License

MIT — see [LICENSE](LICENSE).
