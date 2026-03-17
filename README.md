<p align="center">
  <img src="https://em-content.zobj.net/source/apple/391/poodle_1f429.png" width="96" />
</p>

<h1 align="center">toodles</h1>

<p align="center">
  <strong>Telegram × Gemini CLI — with streaming, voice & local transcription</strong>
</p>

<p align="center">
  <a href="#-quick-start">Quick Start</a> ·
  <a href="#-features">Features</a> ·
  <a href="#%EF%B8%8F-configuration">Config</a> ·
  <a href="#-architecture">Architecture</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/Telegram-2CA5E0?style=for-the-badge&logo=telegram&logoColor=white" alt="Telegram" />
  <img src="https://img.shields.io/badge/Gemini-886FBF?style=for-the-badge&logo=google-gemini&logoColor=white" alt="Gemini" />
  <img src="https://img.shields.io/badge/license-MIT-green?style=for-the-badge" alt="License" />
</p>

---

A Telegram bot written in Rust that wraps [`gemini-cli`](https://github.com/google-gemini/gemini-cli), letting you chat with Gemini AI directly from Telegram — with streaming responses, voice-message transcription (local or cloud), and per-topic session isolation.

## ✨ Features

| | Feature | Details |
|---|---|---|
| 💬 | **Streaming text** | Messages forwarded to gemini-cli; responses streamed back line-by-line |
| 🎙 | **Voice messages** | Transcribed locally via **Parakeet V3** or cloud via **OpenAI Whisper** |
| 🧠 | **Local transcription** | Offline, no API keys — NVIDIA Parakeet ONNX (int8, ~478 MB) |
| 📌 | **Forum topics** | Each Telegram topic gets an isolated gemini-cli session |
| 🔄 | **Session management** | `/new` resets, `/status` shows active count |
| 🔒 | **Access control** | Optional user allowlist via `ALLOWED_USER_IDS` |
| 🧙 | **Setup wizard** | Interactive `--setup` generates `.env` with guided prompts |

## 🚀 Quick Start

### Prerequisites

- **Rust** ≥ 1.70 — [rustup.rs](https://rustup.rs)
- **gemini-cli** — `npm install -g @google/gemini-cli && gemini`
- **Telegram bot token** — [@BotFather](https://t.me/BotFather)
- **ffmpeg** — `brew install ffmpeg` *(required for voice messages)*
- *(Optional)* **OpenAI API key** — for cloud Whisper fallback

### Install & Run

```sh
git clone https://github.com/sleep3r/toodles
cd toodles

# Option A: Interactive setup wizard (recommended)
make setup

# Option B: Manual config
cp .env.example .env
$EDITOR .env

# Run
make run            # debug
make release        # optimized build
make run-release    # run optimized
```

## 🎙 Voice Transcription

toodles supports two transcription backends:

```
┌────────────────────┐     ┌──────────────┐     ┌───────────┐
│   Telegram Voice   │────▶│    ffmpeg     │────▶│ Parakeet  │──── text
│    (OGG Opus)      │     │  (16kHz f32)  │     │   V3 🦜   │
└────────────────────┘     └──────────────┘     └─────┬─────┘
                                                      │ fallback
                                                ┌─────▼─────┐
                                                │  OpenAI    │
                                                │ Whisper 🌐 │
                                                └───────────┘
```

| Mode | Latency | Cost | Setup |
|---|---|---|---|
| **Local** (Parakeet V3) | ~2-5s | Free | `--setup` downloads 478 MB model |
| **Cloud** (Whisper API) | ~1-3s | ~$0.006/min | Requires `OPENAI_API_KEY` |

The setup wizard will guide you through downloading the model. If both are enabled, local transcription is tried first with automatic cloud fallback.

## ⚙️ Configuration

All configuration is managed through environment variables or `.env`:

```sh
# Required
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...

# Access control (leave empty for unrestricted)
ALLOWED_USER_IDS=123456789,987654321

# Gemini CLI
GEMINI_CLI_PATH=gemini                # path to binary
GEMINI_WORKING_DIR=/path/to/project   # optional cwd

# Voice — cloud (optional fallback)
OPENAI_API_KEY=sk-...

# Voice — local (recommended)
USE_LOCAL_TRANSCRIPTION=true
MODELS_DIR=~/.toodles/models

# Logging
RUST_LOG=info
```

> **💡 Tip:** Run `make setup` to generate this interactively!

## 🤖 Bot Commands

| Command | Description |
|---|---|
| `/start` | Welcome message and quick-start guide |
| `/new` | Reset the current gemini-cli session |
| `/status` | Show active session count |
| `/help` | List all commands |

## 📐 Architecture

```
src/
├── main.rs             — entry point, dispatcher, CLI args
├── config.rs           — Config from env vars
├── session.rs          — gemini-cli subprocess manager
├── setup.rs            — interactive setup wizard (--setup)
├── transcription.rs    — Parakeet V3 engine + model download
└── handlers/
    ├── mod.rs           — shared helpers
    ├── message.rs       — text handler (streaming)
    └── voice.rs         — voice handler (local → cloud fallback)
```

Each chat or forum topic maps to a long-lived `gemini-cli` subprocess. Queries are serialized per session via `tokio::sync::Mutex`. Responses stream back by editing a placeholder message every 500ms.

## 🛠 Makefile

```sh
make help          # show all targets
make build         # debug build
make release       # optimized build
make run           # run (debug)
make run-release   # run (release)
make setup         # interactive setup wizard
make test          # run tests
make lint          # clippy
make fmt           # format code
make clean         # clean artifacts
```

## 📄 License

MIT — see [LICENSE](LICENSE).
