# syntax=docker/dockerfile:1
# Static musl binaries in scratch images. Targets: bot, server.
#   docker build --target bot -t bot .
#   docker build --target server -t server .

# Debian's compiler, not Alpine's, whose musl allocator makes it several times slower. The
# programs are still built for musl, so they're static.
FROM rust:1.100-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# The toolchain rust-toolchain.toml pins, which CI checks with, even once the image's tag moves on.
COPY rust-toolchain.toml .
RUN rustup toolchain install --no-self-update && rustup target add "$(uname -m)-unknown-linux-musl"
COPY . .
# Downloaded and compiled crates stay in caches between builds, so only what changed compiles
# again, even after Cargo.lock changes. The sources are touched because cargo goes by file
# times, and copied files can look older than the cached build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find bot/src server/src server/templates server/static -type f -exec touch {} + \
 && target="$(uname -m)-unknown-linux-musl" \
 && cargo build --release --locked --target "$target" --bin bot --bin server \
 && mkdir -p /out /data \
 && cp "target/$target/release/bot" "target/$target/release/server" /out/

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
