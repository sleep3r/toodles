# ──────────────────────────────────────────────────────────────────────────────
# Toodles — Makefile
# ──────────────────────────────────────────────────────────────────────────────

.PHONY: build release run setup test clean check fmt lint help

# Default target
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

# ──────────────────────────────────────────────────────────────────────────────
# Build
# ──────────────────────────────────────────────────────────────────────────────

build: ## Build debug binary
	cargo build

release: ## Build release binary
	cargo build --release

# ──────────────────────────────────────────────────────────────────────────────
# Run
# ──────────────────────────────────────────────────────────────────────────────

run: ## Run the bot (debug)
	cargo run

run-release: ## Run the bot (release)
	cargo run --release

setup: ## Run interactive setup wizard
	cargo run -- --setup

# ──────────────────────────────────────────────────────────────────────────────
# Quality
# ──────────────────────────────────────────────────────────────────────────────

check: ## Check compilation without building
	cargo check

test: ## Run tests
	cargo test

fmt: ## Format code
	cargo fmt

lint: ## Run clippy lints
	cargo clippy -- -W warnings

# ──────────────────────────────────────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────────────────────────────────────

clean: ## Clean build artifacts
	cargo clean
