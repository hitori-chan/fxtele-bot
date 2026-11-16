<img src="docs/homura.webp" alt="" width="112" align="right">

# Homura

Rich link previews with Instant View for Telegram: Facebook posts today, and other sites
(Instagram, Reddit...) to come. Named after [Homura Akemi](https://madoka.fandom.com/wiki/Homura_Akemi),
of *Puella Magi Madoka Magica*.

- **bot**: replies in allowed chats with the same links on another host (`facebook.com` → your
  preview host, `x.com` → `fixupx.com`, ...).
- **server**: serves the preview host. Telegram gets preview tags and an Instant View page for
  the post; everyone else is redirected to the original. A dashboard on a second port holds the
  settings and statistics.

## Setup

Needs Docker Compose (or `podman compose`), a Telegram bot token, and an HTTPS front end
(Caddy, nginx...).

1. In BotFather, turn off group privacy for the bot.
2. Copy and fill in the samples:

   ```sh
   cp .env.example .env
   cp bot/bot.example.toml bot.toml   # your preview host is the value for "facebook.com"
   ```

3. Start: `docker compose up -d --build`
4. Route your front end:
   - preview host → `127.0.0.1:8080` (`PUBLIC_ADDR`)
   - dashboard host → `127.0.0.1:8081` (`DASHBOARD_ADDR`)
5. Open the setup link from the log (valid for 30 minutes) and set a password or add a passkey:

   ```sh
   docker compose logs homura | grep setup
   ```

6. In the dashboard: set a proxy if needed, add Facebook cookies from a separate account (a
   JSON export, a `cookies.txt` or a `Cookie` header), try a link in the playground, and add
   test links and an alert chat.

### Front end

- Telegram must reach the preview host without challenges (turn off Cloudflare's Bot Fight Mode).
- Have it send the client's address in a header it sets itself (behind Cloudflare's proxy, from
  `CF-Connecting-IP`, believed only from Cloudflare's addresses), name that header in
  `CLIENT_IP_FROM` (e.g. `x-real-ip`), and set `TRUSTED_PROXIES` if the front end isn't on the
  same machine.
- Don't cache responses at the edge.
- Serve the dashboard over HTTPS (or `localhost`).

## Dashboard login

A password (an authenticator app is optional), a passkey (needs `DASHBOARD_URL`), or Cloudflare
Access (`ACCESS_TEAM`, `ACCESS_AUD`). If you're locked out:

```sh
docker compose exec homura /server reset-login
```

## Configuration

| Where | What |
|---|---|
| `bot.toml` | owner, allowed users and groups, limits, domain mappings |
| `.env` | bot token, listen addresses, client IP, dashboard URL, Cloudflare Access, video bucket |
| dashboard | everything else; changes apply immediately |

As the owner, send `/menu` to the bot in a private chat to manage allowed users and groups.

## Server commands

| Command | |
|---|---|
| `server extract <url> [--cookies] [--format json\|preview\|iv]` | extract one post |
| `server extract --page <file>` | extract from a saved page |
| `server record <url> [--out <file>]` | save a page as a test fixture |
| `server setup-link` | print a one-time link to the dashboard's Security page |
| `server reset-login` | remove every sign-in method |
| `server healthcheck` | used by the container |

## Development

Rust 1.100. Checks, running it locally and the test pages are in
[development](docs/development.md); everything else is in [docs](docs/README.md).

## License

[MIT](LICENSE), except the logo (`docs/homura.webp`, and `favicon.png`, `homura.webp` and
`mascot.webp` in `server/static/`): it's a sticker from the
[Akemi_Homura](https://t.me/addstickers/Akemi_Homura) Telegram pack, and its rights stay with
its artist.
