# Configuration

## `.env`

The server reads it all; the bot only `TELEGRAM_BOT_TOKEN` and `RUST_LOG`, so the bucket's keys
stay with the server ([sample](../.env.example)). Only the token is required.

| Variable | Default | Meaning |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | | The bot polls with it; the server uses it only for alerts |
| `PUBLIC_ADDR` | `0.0.0.0:8080` | Public listener |
| `DASHBOARD_ADDR` | `0.0.0.0:8081` | Dashboard listener |
| `DATA_DIR` | `/data` | Where `server.db` lives |
| `CLIENT_IP_FROM` | `peer` | `peer`, `cf-connecting-ip`, `x-forwarded-for` or `x-real-ip` |
| `TRUSTED_PROXIES` | loopback | Who may set that header: add the front end's address (e.g. a container network) when it isn't on the same machine |
| `DASHBOARD_URL` | | The dashboard's public URL; needed for passkeys and full setup links |
| `INSTAGRAM_HOST` | | The preview hostname for Instagram links, e.g. `ig.example.com`; every other host serves Facebook's. Unset, Instagram links work only in the dashboard |
| `ACCESS_TEAM`, `ACCESS_AUD` | | Cloudflare Access team and audience tag, to sign in with Access |
| `VIDEO_BUCKET` | | A private bucket for 1080p videos, which need one: its S3 API address ending with its name, e.g. `https://<account>.r2.cloudflarestorage.com/videos`. Telegram downloads from it through signed links, so it must be public over HTTPS |
| `VIDEO_BUCKET_KEY_ID`, `VIDEO_BUCKET_SECRET` | | The bucket's access key, with read and write access |
| `VIDEO_BUCKET_REGION` | `auto` | R2 takes `auto`; Garage wants its `s3_region` |
| `RUST_LOG` | `info` | Log detail, e.g. `server=debug,info` |

## `bot.toml`

([sample](../bot/bot.example.toml)) The path is the bot's first argument.

| Key | Meaning |
|---|---|
| `owner_id` | Your user ID; your private chat is always allowed |
| `allowed_users` | People who may use the bot in a private chat |
| `allowed_chats` | Groups (negative IDs) |
| `state_path` | Where people and groups added from the menu are saved |
| `max_links_per_message` | Links per reply |
| `user_replies_per_minute`, `chat_replies_per_minute` | Reply limits; Telegram itself allows about 20 a minute per group |
| `[domains]` | Domain → replacement host. Subdomains match too, and the whole host is replaced |

The file is checked at startup; a mistake stops the bot with a message naming it.

## Dashboard settings

Stored in `server.db` and applied immediately.

| Setting | Default | Meaning |
|---|---|---|
| Proxy, per site | none | For the site's requests and its videos' 1080p downloads, e.g. `socks5://127.0.0.1:40000` |
| Cookie policy, per site | fallback | `fallback`: cookies only after a login wall; `always`; `never` |
| Site name, per site | the site's | `og:site_name` |
| About the author, per site | Facebook on, Instagram off | Fetch the author's profile for their bio (and on Facebook their full-size picture); one more request per author every 6 hours |
| Fetches, per site | Facebook 60/min, 4 at once; Instagram 20/min, 2 at once | Includes the playground and test links |
| Extraction timeout | 8 s | Keep it below how long Telegram waits for a page |
| Cache | 60 min, failures 5 min, 64 MB | |
| Description length | 300 | `og:description` |
| When extraction fails | card | A card saying why, a redirect to the post, or `503` |
| Media links | direct | The CDN's URLs, or `/_/m/` links that redirect to a fresh one |
| Instant View | on for all | Per post type: video, photo, album, text, profile |
| Counts in Instant View | on | Reactions or likes, comments, shares and views, as of the fetch |
| Several photos as one picture | on | The card shows the first four photos as one ([server](server.md#photo-mosaics)) |
| Videos | 720p | Or up to 1080p, joined by the server and kept in the bucket ([server](server.md#1080p-videos)) |
| Largest video | 20 MB | Telegram doesn't play bigger ones in previews: 1080p ones get 720p, and Facebook's 720p ones its smaller file. At most 100 MB, as joining holds it in memory |
| Space in the bucket | 1024 MB | Oldest first when full |
| Days in the bucket | 3 | 1 to 30; files go after this, checked every hour |
| Requests per IP | 120/min | Telegram is exempt |
| Test links, alert chat | none | See [server](server.md#test-links-and-alerts) |
