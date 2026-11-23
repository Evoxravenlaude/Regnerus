# Regnerus. Railway builds this, as railway.json says.

FROM rust:1-bookworm AS build
WORKDIR /src

# Dependencies first, in a layer of their own, so a code change rebuilds
# only the bot itself.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# Fewer malloc arenas: less resident memory on Railway.
ENV MALLOC_ARENA_MAX=2
COPY --from=build /src/target/release/regnerus /usr/local/bin/regnerus
RUN useradd --system --no-create-home bot
WORKDIR /app
# Labels for exchanges, bridges and app wallets; edit labels.json and redeploy.
COPY labels.json ./labels.json
USER bot

# Settings come from Railway's Variables; see .env.example.
CMD ["regnerus"]
