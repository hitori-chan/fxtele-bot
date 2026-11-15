# syntax=docker/dockerfile:1
# Static musl binaries in scratch images. Target: bot.
#   docker build --target bot -t bot .

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
RUN cargo build --release --locked --bin bot \
 && mkdir -p /out /data \
 && cp target/release/bot /out/

FROM scratch AS bot
COPY --from=build /out/bot /bot
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
WORKDIR /data
ENTRYPOINT ["/bot"]
CMD ["/bot.toml"]
