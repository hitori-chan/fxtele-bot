# fxtele-bot

Facebook link previews for Telegram, with Instant View. There are two independent programs:

- **bot** watches allowed chats for links and replies with the same links on another host:
  `facebook.com` → your preview hostname, `x.com` → `fixupx.com`, and so on. That's all it does.
- **server** answers on the preview hostname. When Telegram's link-preview fetcher asks, it
  extracts the Facebook post and returns preview tags plus an Instant View article. Everyone else
  is redirected to the post on facebook.com. A dashboard on a separate listener shows statistics
  and holds the settings.

[PLAN.md](PLAN.md) has the full design.

## How a link travels

1. Someone posts `https://www.facebook.com/reel/123` in an allowed chat.
2. The bot replies with `https://facebook.example.com/reel/123`.
3. Telegram fetches that URL to build the preview. The server recognizes Telegram by its user
   agent and IP ranges, extracts the post (cached, within a Facebook budget), and returns the
   preview and Instant View page.
4. A person who opens the link gets a redirect to `https://www.facebook.com/reel/123`.

## Setup

You need Docker with Compose, a Telegram bot token, and a front end that gives the server's two
listeners HTTPS hostnames (cloudflared, Caddy, nginx...). The server doesn't do TLS itself.

1. In BotFather, turn the bot's group privacy mode off.
2. In the repo root, copy the samples and fill them in:

   ```sh
   cp .env.example .env               # TELEGRAM_BOT_TOKEN, CLIENT_IP_FROM, DASHBOARD_URL...
   cp bot/bot.example.toml bot.toml   # owner_id, allowed chats, the domain mappings
   ```

   Your preview hostname goes only in `bot.toml`, as the value for `"facebook.com"`.
3. Start both: `just deploy` (or `docker compose -f deploy/compose.yml up -d --build`).
4. Point your front end at the listeners:
   - **public** (`PUBLIC_ADDR`, default `127.0.0.1:8080`) at the preview hostname
   - **admin** (`DASHBOARD_ADDR`, default `127.0.0.1:8081`) at the dashboard hostname
5. Open the one-time setup link from the server's log, then set a password or add a passkey.
   The link is valid for 30 minutes.

   ```sh
   docker compose -f deploy/compose.yml logs homura | grep setup
   docker compose -f deploy/compose.yml exec homura /server setup-link   # a new one
   ```

6. In the dashboard:
   - set the proxy if you use one (for example WARP at `socks5://127.0.0.1:40000`)
   - paste your Facebook cookies if you have them: Cookie-Editor's Export → JSON, or the `Cookie`
     header of any facebook.com request in the developer tools
   - try a link in the playground
   - add test links, plus an alert chat for when they break

### What the front end must do

The full checklist is in [PLAN.md, section 8](PLAN.md#8-deployment). In short:

- **Telegram's fetchers** must reach the public hostname without challenges or JavaScript
  checks. For example, Cloudflare's Bot Fight Mode can block them.
- **Client IP:** set `CLIENT_IP_FROM` to the header your front end adds (`cf-connecting-ip`
  behind Cloudflare). Without it, Telegram's requests look like they come from the proxy, so
  they're redirected instead of previewed. The dashboard warns you when this happens.
- **Caching:** don't cache responses at the edge. The server marks them `no-store`.

## Dashboard login

Any of these can be used, alone or together:

- **Password**, optionally with a code from an authenticator app (TOTP). Set it up from a QR
  code under Login; after that, password sign-in also asks for the 6-digit code.
- **Passkeys:** need `DASHBOARD_URL`, since they're tied to its hostname.
- **Cloudflare Access:** set `ACCESS_TEAM` and `ACCESS_AUD`. A request with a valid Access token
  is signed in.

Repeated failed attempts lock the IP out for a while, and every change must come from the
dashboard's own pages. If you're locked out, or lose the phone with the authenticator app, this
removes the password, authenticator app, passkeys and sessions, and prints a new setup link:

```sh
docker compose -f deploy/compose.yml exec homura /server reset-login
```

## Configuration

| Where | What |
|---|---|
| `bot.toml` ([sample](bot/bot.example.toml)) | owner, allowed people and groups, link cap, reply limits, domain mappings |
| `.env` ([sample](.env.example)) | bot token, listener addresses, client IP source, dashboard URL, Cloudflare Access |
| the dashboard | everything else: proxy, cookie policy, cache, rate limits, Instant View per post type, failure response, test links, alert chat. Changes apply right away. |

As the owner, send `/menu` to the bot in a private chat to add or remove people and groups.

## Server commands

| Command | What it does |
|---|---|
| `server` | Run the server |
| `server extract <url> [--cookies] [--format json\|preview\|iv]` | Extract one post and print it |
| `server extract --page <file>` | The same from a saved page, offline |
| `server record <url> [--cookies] [--out file]` | Save a cleaned-up Facebook page as a test fixture (never commit one saved with `--cookies`) |
| `server setup-link` | Print a one-time dashboard sign-in link |
| `server reset-login` | Remove every way to sign in, then print a setup link |
| `server healthcheck` | Exit 0 if the server answers (used by the container) |

## Development

Rust 1.100 (pinned in `rust-toolchain.toml`), [just](https://github.com/casey/just),
[cargo-nextest](https://nexte.st) and [cargo-deny](https://github.com/EmbarkStudios/cargo-deny).

```sh
just check                 # fmt, clippy, tests, cargo-deny: what CI runs
just dev                   # the server on 127.0.0.1:8080 and :8081, data in ./data
just bot                   # the bot with ./bot.toml (needs TELEGRAM_BOT_TOKEN)
just extract <url>         # extract one post
just record <url>          # save a page under server/tests/fixtures/
just snapshots             # review changed snapshot tests
just bench                 # extraction benchmarks
```

Locally, open the dashboard at `http://localhost:8081`: browsers treat `localhost` as secure,
so the session cookie and passkeys work without HTTPS (passkeys also need
`DASHBOARD_URL=http://localhost:8081`). Sign in with the setup link from the log.

```
bot/       the bot: config, link swapping, access, limits, owner menu, Telegram client
server/    the server: public previews, Facebook extractor, dashboard, SQLite storage
deploy/    compose file
```
