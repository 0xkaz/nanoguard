.PHONY: all build dev test e2e check clean run run-openai ollama-start

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

clean:
	cargo clean

# Start Ollama if not running, pull model, then run nanoguard
run: build ollama-start
	RUST_LOG=info ./target/release/nanoguard

# Run with OpenAI backend
run-openai: build
	RUST_LOG=info BACKEND_PROVIDER=openai BACKEND_ENDPOINT=https://api.openai.com \
	  BACKEND_API_KEY=$(OPENAI_API_KEY) BACKEND_MODEL=gpt-4o-mini \
	  ./target/release/nanoguard

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
