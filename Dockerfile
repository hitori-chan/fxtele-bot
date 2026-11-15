# syntax=docker/dockerfile:1
# Static musl binaries in scratch images. Targets: bot, server.
#   docker build --target bot -t bot .
#   docker build --target server -t server .

FROM rust:1.100-alpine AS chef
RUN apk add --no-cache musl-dev cmake make perl g++ \
 && cargo install --locked cargo-chef
WORKDIR /src

FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin bot --bin server \
 && mkdir -p /out /data \
 && cp target/release/bot target/release/server /out/

FROM scratch AS bot
COPY --from=build /out/bot /bot
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
WORKDIR /data
ENTRYPOINT ["/bot"]
CMD ["/bot.toml"]

FROM scratch AS server
COPY --from=build /out/server /server
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
WORKDIR /data
ENV DATA_DIR=/data
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/server", "healthcheck"]
ENTRYPOINT ["/server"]
