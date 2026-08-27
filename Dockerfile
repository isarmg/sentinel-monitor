FROM node:22-alpine AS web-builder
WORKDIR /build/web
COPY web/package.json web/vite.config.js ./
RUN npm install
COPY web/index.html ./index.html
COPY web/src ./src
RUN npm run build

FROM rust:1.88-bookworm AS rust-builder
WORKDIR /build
COPY Cargo.toml ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=rust-builder /build/target/release/sentinel-monitor /usr/local/bin/sentinel-monitor
COPY --from=web-builder /build/web/dist /app/web
ENV STATIC_DIR=/app/web
EXPOSE 8080
ENTRYPOINT ["sentinel-monitor"]

