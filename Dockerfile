# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
# Use the `:nonroot` variant of distroless so the container runs as UID 65532
# (`nonroot`) by default. This removes the default-root posture; it does *not*
# by itself make the image read-only-rootfs-safe — the bundled `nanoguard.toml`
# writes `nanoguard-audit.jsonl` under `/app` and (if `[budget].enabled = true`)
# `nanoguard.db` as well. Operators who want `docker run --read-only` must
# either disable audit/budget or mount a writable volume over `/app` (or
# override the paths in their own config). See README for the runtime story.
FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app
COPY --from=builder /build/target/release/nanoguard /app/nanoguard
COPY dicts /app/dicts
COPY nanoguard.toml /app/nanoguard.toml

USER nonroot:nonroot

EXPOSE 8080

ENTRYPOINT ["/app/nanoguard"]
