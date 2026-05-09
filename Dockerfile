# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1.78-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

WORKDIR /app
COPY --from=builder /build/target/release/nanoguard /app/nanoguard
COPY dicts /app/dicts
COPY nanoguard.toml /app/nanoguard.toml

EXPOSE 8080

ENTRYPOINT ["/app/nanoguard"]
