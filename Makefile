.PHONY: all build dev test e2e check clean run run-openai ollama-start \
        docker docker-run watch watch-test watch-check \
        coverage miri audit

MODEL ?= qwen3:0.6b
OLLAMA_BASE_URL ?= http://localhost:11434

all: build

build:
	cargo build --release

dev:
	RUST_LOG=debug cargo run

test:
	cargo test

# E2E test — requires nanoguard running on localhost:8080
e2e:
	./e2e_test.sh

check:
	cargo clippy -- -D warnings
	cargo fmt --check

fmt:
	cargo fmt

# ── Watch (requires: cargo install cargo-watch) ───────────────────────────────

watch:
	cargo watch -x "build" -x "test"

watch-test:
	cargo watch -x "test"

watch-check:
	cargo watch -x "clippy -- -D warnings"

# ── Code coverage (requires: cargo install cargo-llvm-cov) ───────────────────
# Install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview

coverage:
	cargo llvm-cov --html --open

coverage-summary:
	cargo llvm-cov --summary-only

watch-coverage:
	cargo watch -s "cargo llvm-cov --summary-only"

# ── Memory / undefined behavior (requires nightly) ───────────────────────────
# Install: rustup toolchain install nightly && cargo +nightly install cargo-miri
# Usage: runs unit tests under Miri interpreter (catches memory bugs, UB)

miri:
	cargo +nightly miri test

# ── Security audit (requires: cargo install cargo-audit) ─────────────────────
# Checks dependencies against RustSec advisory database

audit:
	cargo audit

# ── Full CI-equivalent check (build + test + clippy + fmt + audit) ───────────

ci: check test audit
	@echo "=== All CI checks passed ==="

clean:
	cargo clean

# ── Run ───────────────────────────────────────────────────────────────────────

# Start Ollama if not running, pull model, then run nanoguard
run: build ollama-start
	RUST_LOG=info ./target/release/nanoguard

# Run with OpenAI backend
run-openai: build
	RUST_LOG=info BACKEND_PROVIDER=openai BACKEND_ENDPOINT=https://api.openai.com \
	  BACKEND_API_KEY=$(OPENAI_API_KEY) BACKEND_MODEL=gpt-4o-mini \
	  ./target/release/nanoguard

docker:
	docker build -t nanoguard:latest .

docker-run:
	docker run --rm -p 8080:8080 \
	  -e BACKEND_ENDPOINT=http://host.docker.internal:11434 \
	  nanoguard:latest

ollama-start:
	@if ! curl -sf $(OLLAMA_BASE_URL)/api/tags > /dev/null 2>&1; then \
	  echo "Starting Ollama..."; \
	  ollama serve & \
	  sleep 2; \
	fi
	@if ! ollama list | grep -q "^$(MODEL)"; then \
	  echo "Pulling $(MODEL)..."; \
	  ollama pull $(MODEL); \
	fi
	@echo "Ollama ready (model=$(MODEL))"
