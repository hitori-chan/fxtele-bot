# Development

Rust 1.100 (pinned in `rust-toolchain.toml`), [cargo-nextest](https://nexte.st),
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny) and [cargo-insta](https://insta.rs).

## Checks

CI runs these; all must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo deny check
```

## Running locally

```sh
mkdir -p data
DATA_DIR=data RUST_LOG=server=debug,info cargo run -p server
cargo run -p bot -- bot.toml          # needs TELEGRAM_BOT_TOKEN
```

Open the dashboard at `http://localhost:8081` with the setup link from the log. Browsers treat
`localhost` as secure, so sign-in works without HTTPS; passkeys also need
`DASHBOARD_URL=http://localhost:8081`.

```sh
DATA_DIR=data cargo run -p server -- extract <url> [--cookies] [--format json|preview|iv]
DATA_DIR=data cargo run -p server -- extract --page <file>    # offline, from a saved page
cargo bench -p server                                          # extraction benchmarks
```

## Saved pages

`server/tests/fixtures/` holds logged-out copies of real public posts, saved with
`server record`, and `server/tests/fixtures.rs` snapshots what extraction makes of each:

- `facebook-*.html`: Facebook's pages, keeping only what extraction reads (meta tags and JSON
  blocks, minus comments and the `viewer` block), starting with a `<!-- url: ... -->` line.
- `reddit-*.json`: Reddit's tags for crawlers (`crawler`) and the `url`; logged-in answers aren't
  recorded.
- `threads-*.json`: the post (and the one it answers) from a post's page, or the account from a
  profile's, keeping only the fields extraction reads, under `scripts`.
- `instagram-*.json`: Instagram's answers (`query`, `embed`, `crawler`) and the `url`, keeping
  only the fields extraction reads. Instagram's are of NASA's public posts.

Rules:

- Add one: `cargo run -p server -- record <url> --out server/tests/fixtures/<site>-<what>.<ext>`,
  then `cargo insta test --review` to accept its snapshot.
- When a site changes a page type, re-record it and read the snapshot diff.
- Never add anything saved with `--cookies`: it holds the account's name and more. Instagram's are
  only ever recorded logged out.
- Check that only the posting page or person is named before committing.

## Test video files

`server/tests/fixtures/video/` has two small DASH-style files for the MP4 joiner, made by ffmpeg
from a test pattern and a tone, so nothing there comes from Facebook. The video has B-frames
and a keyframe every 15 frames. To remake them:

```sh
FLAGS="+frag_keyframe+empty_moov+default_base_moof+global_sidx"
ffmpeg -f lavfi -i testsrc2=size=96x64:rate=30 -t 2 -c:v libx264 -preset veryfast -bf 2 -g 15 \
  -pix_fmt yuv420p -movflags $FLAGS -f mp4 video.mp4
ffmpeg -f lavfi -i sine=frequency=440:sample_rate=44100 -t 2 -c:a aac -b:a 32k \
  -movflags $FLAGS -f mp4 audio.mp4
```
