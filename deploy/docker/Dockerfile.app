FROM rust:1-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# 1) Cache dependencies for ai-service + telegram-bot only.
COPY Cargo.toml Cargo.lock ./
COPY funnyprint-proto/Cargo.toml funnyprint-proto/Cargo.toml
COPY funnyprint-render/Cargo.toml funnyprint-render/Cargo.toml
COPY funnyprint-cli/Cargo.toml funnyprint-cli/Cargo.toml
COPY printerd/Cargo.toml printerd/Cargo.toml
COPY ai-service/Cargo.toml ai-service/Cargo.toml
COPY telegram-bot/Cargo.toml telegram-bot/Cargo.toml

RUN mkdir -p \
      funnyprint-proto/src \
      funnyprint-render/src \
      funnyprint-cli/src \
      printerd/src \
      ai-service/src \
      telegram-bot/src \
    && printf 'pub fn placeholder() {}\n' > funnyprint-proto/src/lib.rs \
    && printf 'pub fn placeholder() {}\n' > funnyprint-render/src/lib.rs \
    && printf 'fn main() {}\n' > funnyprint-cli/src/main.rs \
    && printf 'fn main() {}\n' > printerd/src/main.rs \
    && printf 'fn main() {}\n' > ai-service/src/main.rs \
    && printf 'fn main() {}\n' > telegram-bot/src/main.rs \
    && cargo build --release -p ai-service -p telegram-bot \
    && rm -rf \
      funnyprint-proto/src \
      funnyprint-render/src \
      funnyprint-cli/src \
      printerd/src \
      ai-service/src \
      telegram-bot/src

# 2) Build real binaries.
COPY . .
RUN find ai-service/src telegram-bot/src -type f -exec touch {} + \
    && rm -f /app/target/release/ai-service /app/target/release/telegram-bot \
    && cargo build --release -p ai-service -p telegram-bot

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
        libssl3 \
        fontconfig \
        fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/ai-service /usr/local/bin/ai-service
COPY --from=builder /app/target/release/telegram-bot /usr/local/bin/telegram-bot

EXPOSE 8090
