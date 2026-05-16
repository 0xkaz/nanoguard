# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
# Use the `:nonroot` variant of distroless so the container runs as UID 65532
# (`nonroot`) by default. Combined with a read-only root filesystem at deploy
# time this gives "process inside cannot mutate the image"-grade isolation.
FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app
COPY --from=builder /build/target/release/nanoguard /app/nanoguard
COPY dicts /app/dicts
COPY nanoguard.toml /app/nanoguard.toml

USER nonroot:nonroot

EXPOSE 8080

ENTRYPOINT ["/app/nanoguard"]
