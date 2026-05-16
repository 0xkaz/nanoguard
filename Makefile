.PHONY: all build dev test e2e e2e-live check lint fmt clean run run-openai ollama-start \
        docker docker-run release docker-release watch-docker watch watch-test watch-lint \
        bench coverage miri audit geiger ci push release-patch release-minor release-major \
        release-tag pr pr-web preflight

MODEL ?= qwen3:0.6b
OLLAMA_BASE_URL ?= http://localhost:11434

all: build

build:
	cargo build --release

dev:
	RUST_LOG=debug cargo run

test:
	cargo test

# E2E test — boots a mock backend + a release nanoguard binary, then runs
# the buffered SSE / Vault round-trip / shadow-mode / per-entity-action
# scenarios. Self-contained: no servers need to be running first.
e2e:
	./tools/e2e.sh

# Live e2e against an already-running nanoguard on :8080. Used for ad-hoc
# checks against `make run` and for the budget/admin API tests that require
# an ADMIN_API_KEY to be set on both sides.
e2e-live:
	./e2e_test.sh

# lint = clippy + fmt check
lint:
	cargo clippy -- -D warnings
	cargo fmt --check
	@echo "=== lint OK ==="

# check = test + lint
check: test lint

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

# ── Geiger security audit (requires: cargo install cargo-geiger) ──────────
# Checks for unsafe code usage in dependencies

geiger:
	cargo geiger --update-advisories

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

# Tag the latest main as the current Cargo.toml version. Run AFTER a
# release PR (created by `make release-{patch,minor,major}`) has merged.
release-tag:
	./tools/release-tag.sh

# ── PR helpers (require `gh` CLI) ────────────────────────────────────────────
# `make pr` opens a PR for the current branch, prefilling title / body from
# the most recent commit. Use `make pr-web` to also open the GitHub UI in a
# browser for final review.

pr:
	@command -v gh >/dev/null 2>&1 || { echo "error: \`gh\` CLI not found"; exit 1; }
	@branch=$$(git rev-parse --abbrev-ref HEAD); \
	if [ "$$branch" = "main" ]; then \
	    echo "error: refusing to open a PR from main; check out a feature branch first"; \
	    exit 1; \
	fi; \
	if [ -n "$$(git status --porcelain)" ]; then \
	    echo "error: working tree has uncommitted changes"; git status --short; exit 1; \
	fi; \
	if ! git ls-remote --exit-code origin "$$branch" >/dev/null 2>&1; then \
	    echo "pushing branch $$branch to origin first..."; \
	    git push -u origin "$$branch"; \
	fi; \
	gh pr create --base main --fill

pr-web: pr
	@gh pr view --web

# ── Preflight (run before make pr) ──────────────────────────────────────────
# Mirrors the CI lint, test, audit, and e2e jobs so a feature branch fails
# locally instead of red-statusing a PR. Required by CLAUDE.md > Branch Policy.

preflight:
	@echo "→ cargo fmt --check"
	cargo fmt --check
	@echo "→ cargo clippy --all-targets -- -D warnings"
	cargo clippy --all-targets -- -D warnings
	@echo "→ cargo test"
	cargo test --quiet
	@echo "→ cargo audit"
	@command -v cargo-audit >/dev/null 2>&1 || { \
	    echo ""; \
	    echo "error: cargo-audit not installed."; \
	    echo "       install once with:"; \
	    echo "           cargo install cargo-audit"; \
	    echo ""; \
	    echo "       cargo-audit is required by preflight because the CI"; \
	    echo "       'security audit' job runs it; failures here surface"; \
	    echo "       advisories before they break a PR."; \
	    exit 1; \
	}
	cargo audit
	@echo "→ tools/e2e.sh"
	./tools/e2e.sh
	@echo "✓ preflight passed"
