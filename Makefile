# ──────────────────────────────────────────────────────────────────────────────
# Toodles — Makefile
# ──────────────────────────────────────────────────────────────────────────────

.PHONY: build release run setup test clean check fmt lint help

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
