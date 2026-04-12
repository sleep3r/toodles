# ──────────────────────────────────────────────────────────────────────────────
# Toodles — Makefile
# ──────────────────────────────────────────────────────────────────────────────

.PHONY: build release run run-release setup test clean check fmt lint help \
	service-plist service-sync-env service-install service-uninstall service-start \
	service-stop service-restart service-status service-logs service-update

# Some macOS setups with Command Line Tools only miss Rust's default
# clang runtime search path. Detect clang's resource dir and inject
# LIBRARY_PATH so linker can find libclang_rt.osx.
CLANG_RESOURCE_DIR := $(shell xcrun --toolchain default clang --print-resource-dir 2>/dev/null)
CLANG_RT_DARWIN := $(CLANG_RESOURCE_DIR)/lib/darwin

ifneq (,$(wildcard $(CLANG_RT_DARWIN)))
  CARGO_ENV = LIBRARY_PATH="$(CLANG_RT_DARWIN)"
else
  CARGO_ENV =
endif

# macOS launchd service defaults
PROJECT_DIR := $(CURDIR)
TOODLES_ENV_FILE ?= $(PROJECT_DIR)/.env
TOODLES_RELEASE_BIN := $(PROJECT_DIR)/target/release/toodles
SERVICE_CONFIG_DIR := $(HOME)/.config/toodles
TOODLES_SERVICE_ENV := $(SERVICE_CONFIG_DIR)/service.env
LAUNCHD_WORKDIR ?= $(HOME)

LAUNCHD_LABEL ?= com.toodles.bot
LAUNCHD_DOMAIN := gui/$(shell id -u)
LAUNCHD_PLIST := $(HOME)/Library/LaunchAgents/$(LAUNCHD_LABEL).plist
LAUNCHD_OUT_LOG := $(PROJECT_DIR)/logs/$(LAUNCHD_LABEL).out.log
LAUNCHD_ERR_LOG := $(PROJECT_DIR)/logs/$(LAUNCHD_LABEL).err.log

# Default target
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

# ──────────────────────────────────────────────────────────────────────────────
# Build
# ──────────────────────────────────────────────────────────────────────────────

build: ## Build debug binary
	$(CARGO_ENV) cargo build

release: ## Build release binary
	$(CARGO_ENV) cargo build --release

# ──────────────────────────────────────────────────────────────────────────────
# Run
# ──────────────────────────────────────────────────────────────────────────────

run: ## Run the bot (debug)
	$(CARGO_ENV) cargo run

run-release: ## Run the bot (release)
	$(CARGO_ENV) cargo run --release

setup: ## Run interactive setup wizard
	$(CARGO_ENV) cargo run -- --setup

# ──────────────────────────────────────────────────────────────────────────────
# Quality
# ──────────────────────────────────────────────────────────────────────────────

check: ## Check compilation without building
	cargo check

test: ## Run tests
	$(CARGO_ENV) cargo test

fmt: ## Format code
	cargo fmt

lint: ## Run clippy lints
	cargo clippy -- -W warnings

# ──────────────────────────────────────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────────────────────────────────────

clean: ## Clean build artifacts
	cargo clean

# ──────────────────────────────────────────────────────────────────────────────
# macOS launchd service
# ──────────────────────────────────────────────────────────────────────────────

service-plist: ## Generate launchd plist in ~/Library/LaunchAgents
	@mkdir -p "$(HOME)/Library/LaunchAgents" "$(PROJECT_DIR)/logs"
	@printf '%s\n' \
		'<?xml version="1.0" encoding="UTF-8"?>' \
		'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
		'<plist version="1.0">' \
		'<dict>' \
		'  <key>Label</key>' \
		'  <string>$(LAUNCHD_LABEL)</string>' \
		'  <key>ProgramArguments</key>' \
		'  <array>' \
		'    <string>/bin/zsh</string>' \
		'    <string>-lc</string>' \
		'    <string>if [ -f "$(TOODLES_SERVICE_ENV)" ]; then source "$(TOODLES_SERVICE_ENV)"; fi; exec "$(TOODLES_RELEASE_BIN)"</string>' \
		'  </array>' \
		'  <key>WorkingDirectory</key>' \
		'  <string>$(LAUNCHD_WORKDIR)</string>' \
		'  <key>EnvironmentVariables</key>' \
		'  <dict>' \
		'    <key>PATH</key>' \
		'    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$(HOME)/.cargo/bin</string>' \
		'    <key>HOME</key>' \
		'    <string>$(HOME)</string>' \
		'  </dict>' \
		'  <key>RunAtLoad</key>' \
		'  <true/>' \
		'  <key>KeepAlive</key>' \
		'  <true/>' \
		'  <key>ThrottleInterval</key>' \
		'  <integer>5</integer>' \
		'  <key>StandardOutPath</key>' \
		'  <string>$(LAUNCHD_OUT_LOG)</string>' \
		'  <key>StandardErrorPath</key>' \
		'  <string>$(LAUNCHD_ERR_LOG)</string>' \
		'</dict>' \
		'</plist>' \
		> "$(LAUNCHD_PLIST)"
	@echo "Generated $(LAUNCHD_PLIST)"

service-sync-env: ## Sync .env into launchd-readable env file
	@if [ ! -f "$(TOODLES_ENV_FILE)" ]; then echo "Missing $(TOODLES_ENV_FILE). Run 'make setup' first."; exit 1; fi
	@mkdir -p "$(SERVICE_CONFIG_DIR)"
	@python3 "$(PROJECT_DIR)/scripts/make_service_env.py" "$(TOODLES_ENV_FILE)" "$(TOODLES_SERVICE_ENV)"
	@echo "Synced env -> $(TOODLES_SERVICE_ENV)"

service-install: release service-sync-env service-plist ## Build release and install launchd service
	@launchctl bootout $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) >/dev/null 2>&1 || true
	@sleep 1
	@launchctl bootstrap $(LAUNCHD_DOMAIN) "$(LAUNCHD_PLIST)"
	@launchctl enable $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) >/dev/null 2>&1 || true
	@launchctl kickstart -k $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL)
	@echo "Installed and started: $(LAUNCHD_LABEL)"

service-uninstall: ## Stop and remove launchd service
	@launchctl bootout $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) >/dev/null 2>&1 || true
	@rm -f "$(LAUNCHD_PLIST)"
	@echo "Removed: $(LAUNCHD_LABEL)"

service-start: service-plist ## Start launchd service
	@launchctl bootstrap $(LAUNCHD_DOMAIN) "$(LAUNCHD_PLIST)" >/dev/null 2>&1 || true
	@launchctl kickstart -k $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL)
	@echo "Started: $(LAUNCHD_LABEL)"

service-stop: ## Stop launchd service
	@launchctl bootout $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) >/dev/null 2>&1 || true
	@echo "Stopped: $(LAUNCHD_LABEL)"

service-restart: ## Restart launchd service
	@launchctl kickstart -k $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) >/dev/null 2>&1 || \
		(launchctl bootstrap $(LAUNCHD_DOMAIN) "$(LAUNCHD_PLIST)" >/dev/null 2>&1 || true; \
		 launchctl kickstart -k $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL))
	@echo "Restarted: $(LAUNCHD_LABEL)"

service-status: ## Show launchd service status
	@launchctl print $(LAUNCHD_DOMAIN)/$(LAUNCHD_LABEL) | sed -n '1,120p'

service-logs: ## Tail service stdout/stderr logs
	@mkdir -p "$(PROJECT_DIR)/logs"
	@touch "$(LAUNCHD_OUT_LOG)" "$(LAUNCHD_ERR_LOG)"
	@echo "Tailing logs: $(LAUNCHD_OUT_LOG) and $(LAUNCHD_ERR_LOG)"
	@tail -n 120 -f "$(LAUNCHD_OUT_LOG)" "$(LAUNCHD_ERR_LOG)"

service-update: service-install ## Rebuild release binary, sync env, and restart service
	@echo "Updated binary/env and restarted $(LAUNCHD_LABEL)"
