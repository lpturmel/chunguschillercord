FROM rust:1.95.0-slim-bullseye AS builder

WORKDIR /app
RUN apt-get update && \
    apt-get install --yes --no-install-recommends build-essential cmake perl pkg-config && \
    rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --locked --release -p chunguschillercord && \
    cp target/release/chunguschillercord /chunguschillercord

FROM debian:12.1-slim

RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* && \
    mkdir -p /data

WORKDIR /app
COPY --from=builder /chunguschillercord ./chunguschillercord
COPY --from=builder /app/config ./config

ENV CHUNGUSCHILLERCORD_DATABASE_PATH=/data/chunguschillercord.db
CMD ["./chunguschillercord"]
