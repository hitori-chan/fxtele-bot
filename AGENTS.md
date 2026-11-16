# AGENTS.md

Notes for anyone, coding agents included, changing this repo. `docs/` covers how it works and
how to deploy it.

## What this is

Two independent programs in one Cargo workspace (Rust 1.100, edition 2024):

- **`bot/`** replies in allowed Telegram chats with the same links on another host
  (`facebook.com` → the preview hostname, `x.com` → `fixupx.com`...). Configured by `bot.toml`.
- **`server/`** answers on the preview hostname. Telegram's link-preview fetcher, recognized by
  its user agent *and* Telegram's IP ranges, gets preview tags and an Instant View article built
  from the Facebook post; everyone else is redirected to facebook.com. A dashboard on a second
  listener holds the settings, statistics and a playground.

## Server layout

| Path | What |
|---|---|
| `src/app.rs` | Shared state; getting a post through the cache and the Facebook budget |
| `src/public/` | The public listener: previews, and `/_/m/` media redirects |
| `src/extract/` | The `Post` model; `facebook/` fetches and reads pages |
| `src/render.rs`, `templates/preview.html` | Preview tags and the Instant View article |
| `src/dashboard/` | Login (password, TOTP, passkeys, Cloudflare Access), pages, live updates |
| `src/canary.rs` | Scheduled test links and Telegram alerts |
| `src/settings.rs` | Live settings, stored in SQLite and edited in the dashboard |
| `src/db.rs`, `cache.rs`, `stats.rs`, `limits.rs` | Storage, post cache, statistics, rate limits |
| `tests/fixtures/` | Saved logged-out Facebook pages; `tests/fixtures.rs` snapshots them |

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo deny check
```

All four must pass; CI runs exactly these. Running the server, extracting a link and recording
a fixture are in `docs/development.md`. `cargo insta review` accepts
changed snapshots after you've read the diff.

## Conventions

- Lints are set for the workspace in `Cargo.toml`; `unsafe` is forbidden.
- Each file starts with a `//!` comment saying what it's for. Comments explain why, not what.
- Messages in errors, logs and the dashboard are plain English for whoever runs the server,
  and say what to do about the problem.
- New settings get a default (old saved settings must still load), a check in
  `Settings::check`, a field in the dashboard form, and a line in its test.
- Keep `docs/` current with the code, as short as it can be.
- Changing extraction: update `docs/facebook.md`, run the fixture tests, and read
  every snapshot diff before accepting it. Facebook documents none of this, so the notes and
  snapshots are the only record of why the code looks the way it does.
- New dependencies must pass `cargo deny check` (allowed licenses are in `deny.toml`).
- Commit subjects read "Area: what changed"; the body says why, and how it was checked.

## Safety rules

- Never log or print cookie values, tokens or message text.
- Never commit a page saved with `--cookies` (it holds the account's name) or a cookie export.
  Keep cookie exports outside the repo.
- Fixtures are logged-out pages saved with `server record`, checked so that only the posting
  page or person is named.
- No real hostnames in code, tests or samples: use `fb.example.com` and
  `homura.example.com`. Whoever deploys picks theirs in `bot.toml` and `.env`.
- Strangers must not be able to make the server fetch things: media routes serve only media of
  posts the server extracted itself, and only Telegram's verified requests trigger fetches.
