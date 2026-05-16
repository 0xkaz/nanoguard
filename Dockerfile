# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

# Stage an /app directory owned by the runtime UID so the distroless image
# inherits a writable workdir (default audit log and budget DB are relative
# paths created at runtime; the root-owned /app from WORKDIR would block them).
RUN install -d -m 0755 -o 65532 -g 65532 /staged

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder --chown=65532:65532 /staged /app
WORKDIR /app
COPY --from=builder --chown=65532:65532 /build/target/release/nanoguard /app/nanoguard
COPY --chown=65532:65532 dicts /app/dicts
COPY --chown=65532:65532 nanoguard.toml /app/nanoguard.toml

EXPOSE 8080

USER 65532:65532

ENTRYPOINT ["/app/nanoguard"]
