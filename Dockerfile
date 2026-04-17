# syntax=docker/dockerfile:1.7
#
# Multi-stage build for bot-template-rs. Uses cargo-chef so dependency
# compilation is cached independently from application code.
#
# Feature selection is controlled by the FEATURES build arg. The default
# covers the full template surface (lavalink + native music backends,
# playlists, recording, TTS). Override with --build-arg FEATURES="..." to
# produce a leaner image, e.g. `--build-arg FEATURES="music"` for a
# lavalink-only build.

FROM lukemathwalker/cargo-chef:latest-rust-latest AS chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG FEATURES="music music-native playlists record tts stt"
# git is needed by build.rs (commit-hash stamp); cmake/pkg-config/libssl-dev
# satisfy transitive C dependencies pulled in by a few crates.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        cmake pkg-config libssl-dev git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --no-default-features --features "${FEATURES}"
COPY . .
RUN cargo build --release --no-default-features --features "${FEATURES}"

FROM debian:trixie-slim AS runtime
# yt-dlp + ffmpeg are required at runtime by the `native` music backend
# (songbird's YoutubeDl input shells out to yt-dlp and pipes through ffmpeg).
# tini gives us a proper PID 1 so SIGTERM reaches the bot cleanly.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates tini ffmpeg python3 curl \
    && curl -fsSL -o /usr/local/bin/yt-dlp \
        https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp \
    && apt-get purge -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/bot-template-rs /usr/local/bin/bot-template-rs

# Mount a host dir at /app/config to persist guild_configs, lavalink.yaml,
# tts.yaml, and the playlists/ subdirectory across container restarts.
VOLUME ["/app/config", "/app/logs"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/bot-template-rs"]
