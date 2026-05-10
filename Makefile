.PHONY: all build dev test e2e check lint fmt clean run run-openai ollama-start \
        docker docker-run release docker-release watch-docker watch watch-test watch-lint \
        bench coverage miri audit ci push release-patch release-minor release-major

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

# lint = clippy + fmt check (CI と同じ判定)
lint:
	cargo clippy -- -D warnings
	cargo fmt --check
	@echo "=== lint OK ==="

# check は lint の別名（後方互換）
check: lint

fmt:
	cargo fmt

# ── Watch (requires: cargo install cargo-watch) ───────────────────────────────

watch:
	cargo watch -x "build" -x "test"

watch-test:
	cargo watch -x "test"

watch-lint:
	cargo watch -w src -w benches -s "cargo clippy -- -D warnings && cargo fmt --check && echo '=== lint OK ==='"

watch-check: watch-lint

# ── Code coverage (requires: cargo install cargo-llvm-cov) ───────────────────
# Install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview

# ── Benchmarks (requires: cargo bench) ───────────────────────────────────────
# Results saved to target/criterion/  (HTML report auto-generated)

bench:
	cargo bench

coverage:
	cargo llvm-cov --html --open

coverage-summary:
	cargo llvm-cov --summary-only

watch-coverage:
	cargo watch -s "cargo llvm-cov --summary-only"

# ── Memory / undefined behavior (requires nightly) ───────────────────────────
# Install: rustup toolchain install nightly
#          rustup component add --toolchain nightly miri
# Note: tokio::test uses kqueue/epoll — not supported by Miri.
#       Run only on sync/pure-Rust tests via filter:
#       make miri TEST=matcher

miri:
	cargo +nightly miri test $(TEST)

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

# Multi-arch build & push to ghcr.io (requires: docker login ghcr.io + buildx)
release:
	docker buildx build \
	  --platform linux/arm64,linux/amd64 \
	  --tag ghcr.io/0xkaz/nanoguard:latest \
	  --push \
	  .

# Watch src/ changes and rebuild Docker image automatically
watch-docker:
	cargo watch -w src -w Cargo.toml -w Dockerfile \
	  -s "docker build -t nanoguard:latest . && echo '=== Docker build OK ==='"

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

# ── Release helpers (cargo-edit + tools/release.sh + tools/push.sh) ─────────

release-patch:
	./tools/release.sh patch

release-minor:
	./tools/release.sh minor

release-major:
	./tools/release.sh major

push:
	./tools/push.sh
