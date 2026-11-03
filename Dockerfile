# syntax=docker/dockerfile:1

######################################################################################
# Build the Rust bot
######################################################################################
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY db ./db
COPY shared ./shared
COPY vendor ./vendor
RUN cargo build --release --locked

######################################################################################
# Runtime: the Rust binary plus the Ruby operations tooling (migrate, commands:*)
######################################################################################
FROM ruby:3.4-slim-bookworm AS runtime
LABEL org.opencontainers.image.title="Gaia"       org.opencontainers.image.description="Multi-server Discord moderation and utility bot"       org.opencontainers.image.source="https://github.com/darkzmaj/Gaia-Discord-Bot"       org.opencontainers.image.licenses="MIT"       org.opencontainers.image.authors="DarkZmaj"       org.opencontainers.image.version="1.0.0"
WORKDIR /app
ENV BUNDLE_WITHOUT=test BUNDLE_DEPLOYMENT=1 BUNDLE_USER_HOME=/tmp/.bundle

RUN groupadd --system gaia && useradd --system --gid gaia --home /app gaia

COPY Gemfile Gemfile.lock Rakefile ./
RUN bundle install && rm -rf /usr/local/bundle/cache
COPY ruby ./ruby
COPY db ./db
COPY shared ./shared
COPY --from=build /app/target/release/gaia /usr/local/bin/gaia

RUN mkdir -p /app/data && chown -R gaia:gaia /app

USER gaia

VOLUME ["/app/data"]

# Discord bots are outbound-gateway only; no port is exposed.
CMD ["gaia"]
