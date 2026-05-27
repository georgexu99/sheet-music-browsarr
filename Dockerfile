# syntax=docker/dockerfile:1.7

# ---------- builder ----------
FROM rust:1-bookworm AS builder
WORKDIR /app

# Pre-fetch deps for better layer caching.
COPY Cargo.toml ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && \
    cargo fetch && \
    rm -rf src

# Real build.
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY assets ./assets
COPY build.rs ./

# Tailwind CSS — pull the standalone binary (no Node toolchain needed),
# compile assets/tailwind.css to dist/styles.css. The Rust binary embeds
# the result via include_str! at compile time, so this MUST run before
# `cargo build`.
ADD --chmod=755 https://github.com/tailwindlabs/tailwindcss/releases/latest/download/tailwindcss-linux-x64 /usr/local/bin/tailwindcss
RUN mkdir -p dist && \
    /usr/local/bin/tailwindcss -i ./assets/tailwind.css -o ./dist/styles.css --minify

RUN cargo build --release && \
    strip target/release/sheet-music-browsarr || true

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      rsync \
      openssh-client \
      tini \
    && rm -rf /var/lib/apt/lists/*

# Non-root user (PUID/PGID conventions match the rest of the NAS stacks).
RUN groupadd --gid 1000 browsarr && \
    useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash browsarr

COPY --from=builder /app/target/release/sheet-music-browsarr /usr/local/bin/sheet-music-browsarr

USER browsarr
WORKDIR /home/browsarr
EXPOSE 8686

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/sheet-music-browsarr"]
