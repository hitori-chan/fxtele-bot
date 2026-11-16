# AGENTS.md

Notes for anyone, coding agents included, changing this repo. `docs/` covers how it works and
how to deploy it.

## What this is

Homura: Telegram link previews and Instant View for social posts, from Facebook and Instagram so
far; others (Reddit...) are meant to follow. Each site is an `extract::Source`, with its own
extractor, settings, cookies, budget, statistics and prefix in the video bucket. Two independent programs in one Cargo workspace (Rust 1.100,
edition 2024):

- **`bot/`** replies in allowed Telegram chats with the same links on another host
  (`facebook.com` → the preview host, `x.com` → `fixupx.com`...). Configured by `bot.toml`.
- **`server/`** answers on the preview hosts, one per site (`INSTAGRAM_HOST` picks Instagram's).
  Telegram's link-preview fetcher, recognized by its user agent *and* Telegram's IP ranges, gets
  preview tags and an Instant View article built from the post; everyone else is redirected to
  the original. A dashboard on a second
  listener holds the settings, statistics and a playground.

## Server layout

| Path | What |
|---|---|
| `src/app.rs` | Shared state; getting a post through the cache and its site's budget |
| `src/public/` | Public listener: previews, `/_/m/` media redirects, `/_/v/` videos (1080p or the site's), `/_/g/` photo mosaics, `/_/l/` signed text links |
| `src/mosaic.rs` | Several photos as one picture for the card: layout and drawing |
| `src/extract/` | The `Post` model, `Source`, `Target` and `Sites`, which keeps media on Meta's CDNs; `page.rs` reads HTML; `facebook/` reads Facebook's pages (`dash.rs` finds 1080p streams); `instagram/` Instagram's queries |
| `src/video/` | Joins DASH video and audio into one MP4 (`mp4.rs`) and keeps it in a bucket with S3's API (`bucket.rs`) |
| `src/render.rs`, `templates/preview.html` | Preview tags and the Instant View article |
| `src/dashboard/`, `templates/dashboard/`, `static/` | Login (password, TOTP, passkeys, Cloudflare Access), pages (Askama and htmx, no build step), SVG charts, live updates |
| `src/canary.rs`, `src/alerts.rs` | Test links; alerts about test links, live failures and videos |
| `src/settings.rs` | Live settings, stored in SQLite and edited in the dashboard |
| `src/db.rs`, `migrations/`, `cache.rs`, `stats.rs`, `limits.rs` | Storage, post cache, statistics, rate limits |
| `tests/fixtures/` | Saved logged-out Facebook pages and Instagram answers (snapshotted by `tests/fixtures.rs`), test video files |

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo deny check
```

All four must pass; CI runs exactly these. Running the server, extracting a link and recording
a fixture are in `docs/development.md`. `cargo insta review` accepts changed snapshots after
you've read the diff.

## Conventions

- Lints are set for the workspace in `Cargo.toml`; `unsafe` is forbidden.
- Each file starts with a `//!` comment saying what it's for. Comments explain why, not what.
- Messages in errors, logs and the dashboard are plain English for whoever runs the server,
  and say what to do about the problem.
- New settings get a default (old saved settings must still load), a check in
  `Settings::check`, a field in the dashboard form, and a line in its test.
- Keep `docs/` current with the code, and short. The README stays a quick start.
- Changing extraction: update `docs/facebook.md` or `docs/instagram.md`, run the fixture tests,
  and read every snapshot diff before accepting it. Neither site documents any of this, so the
  notes and snapshots are the only record of why the code looks the way it does.
- A new site: a `Source` variant, an extractor under `src/extract/`, its cookies' names in
  `cookies.rs`, and a nav entry; settings, budgets, statistics, the bucket and the dashboard pages
  follow `Source::ALL`.
- New dependencies must pass `cargo deny check` (allowed licenses are in `deny.toml`).
- No task runner (justfile, Makefile); plain cargo commands.
- Until the first release, schema changes go into `migrations/0001_init.sql`, not new migrations.
- Commit messages are one short line, "Area: what changed". No body unless something isn't
  obvious from the diff.

## Safety rules

- Never log or print cookie values, tokens, the bucket's secret, signed bucket links or message
  text.
- Never commit a page saved with `--cookies` (it holds the account's name) or a cookie export.
  Keep cookie exports outside the repo.
- Fixtures are logged-out pages saved with `server record`, checked so that only the posting
  page or person is named. Test media is generated (see `docs/development.md`), never a site's.
- Instagram cookies go only to www.instagram.com, with the client's one browser user agent: a
  session that shows up as another device gets logged out.
- No real hostnames in code, tests or samples: use `fb.example.com`, `ig.example.com` and
  `homura.example.com`. Whoever deploys picks theirs in `bot.toml` and `.env`.
- Strangers must not be able to make the server fetch things: media routes serve only media of
  posts the server extracted itself, and only Telegram's addresses trigger fetches or get the
  bucket's files.
- The server never sends video bytes: videos redirect to the site's CDN, and opt-in 1080p videos
  to the bucket, which stays private and is reached through links signed for an hour. Cloudflare's
  free plan forbids video through its proxy, so keep the Videos page's note for buckets that
  aren't R2.
- In the bucket, the server only deletes files it named itself (`<source>/<16 hex>-<n>.mp4`).
