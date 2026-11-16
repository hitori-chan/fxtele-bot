# Deployment

## Services

`compose.yml` runs two containers, built from the one `Dockerfile` (static binaries in `scratch`
images). Both read `.env`, but the bot's container gets only `TELEGRAM_BOT_TOKEN` and `RUST_LOG`,
so the bucket's keys stay with the server.

| Service | Network | Data |
|---|---|---|
| `bot` | default, outbound only | `bot.toml` (read-only), volume `bot-data` (`state.json`) |
| `homura` | host, so it can reach a local proxy such as WARP | volume `homura-data` (`server.db`) |

The sample `.env` keeps the listeners on `127.0.0.1:8080` (public) and `127.0.0.1:8081`
(dashboard), as `compose.yml` uses the host's network. The server doesn't do TLS; put a front end
in front of both. On a container network, drop those two lines and add the front end's address to
`TRUSTED_PROXIES`.

Podman works too, rootless included: use `podman compose`, or install `podman-docker`.

## Front end

- **Preview host → public listener**, over HTTPS. Telegram's fetchers must get through without
  challenges or JavaScript checks. Cloudflare's Bot Fight Mode blocks them, and on the free plan
  it can't be turned off for one hostname.
- **Instagram's preview host → public listener** too, with `INSTAGRAM_HOST` set to its name
  (e.g. `ig.example.com`). Facebook and Instagram share paths (`/reel/`, `/stories/`), so each
  needs its own host.
- **Dashboard host → dashboard listener**, over HTTPS (or `localhost`), preferably not the preview
  host.
- **Client IP:** the front end sends the client's address in a header it sets itself, replacing
  any the client sent, and `CLIENT_IP_FROM` names that header (`x-real-ip`, say). Without it,
  Telegram's requests look like the front end's, get redirected instead of previewed, and the
  dashboard shows a warning. Behind Cloudflare's proxy, the front end takes the address from
  `CF-Connecting-IP`, believing it only from [Cloudflare's addresses](https://www.cloudflare.com/ips/):
  one that passes the header on as it came lets anyone who reaches it directly claim a Telegram
  address, and so make fetches with the saved cookies and get the bucket's files. Only loopback
  may set the header by default, so nobody else on the network can claim to be Telegram; add the
  front end's address to `TRUSTED_PROXIES` if it runs elsewhere.
- **Caching:** don't cache at the edge. Telegram and people get different answers for the same
  URL; the server marks everything `no-store, no-transform`, which also keeps Cloudflare's email
  obfuscation out of the pages.
- **Certificates:** Cloudflare's free certificate covers `example.com` and one level below,
  such as `fb.example.com` and `ig.example.com`, but not deeper names. The bot replaces the
  whole host, so one name per site is enough.

## 1080p videos in a bucket

Optional, and needed for 1080p videos. Any bucket with S3's API works; tested with R2 and Garage.
The bucket stays private: Telegram downloads through links signed for an hour, from the same S3
API address the server uses, so that address must be reachable from the internet over HTTPS.

**Cloudflare R2**

1. Create a bucket, e.g. `videos`. Leave public access off: no custom domain or `r2.dev` address.
2. Create an R2 API token with "Object Read & Write" for that bucket.
3. In `.env`: `VIDEO_BUCKET` is the bucket's S3 API address (shown in its settings, ending in
   `/videos`), `VIDEO_BUCKET_KEY_ID` and `VIDEO_BUCKET_SECRET` the token's keys.

**Garage**

1. `garage bucket create videos`, `garage key create preview-server`, then
   `garage bucket allow --read --write videos --key preview-server`. No website access.
2. Serve Garage's S3 API publicly over HTTPS, e.g. at `s3.example.com`. Behind Cloudflare, make
   that record DNS only: its free plan doesn't allow video through its proxy.
3. In `.env`: `VIDEO_BUCKET` is that address ending in `/videos`, the key's ID and secret, and
   `VIDEO_BUCKET_REGION` Garage's `s3_region`.

Restart the server, then choose 1080p on the dashboard's Videos page. The page lists the bucket
the first time it's opened, so it shows right away whether the keys work; after that, each new
video does.

## First run

1. In BotFather, turn off group privacy, so the bot sees ordinary group messages.
2. Fill in `.env` and `bot.toml` ([configuration](configuration.md)).
3. `docker compose up -d --build`
4. Open the setup link from `docker compose logs homura` within 30 minutes, and set a password or
   add a passkey. `server setup-link` prints a new one.

## Upgrades and backups

- Upgrade: pull, then run step 3 again. Builds keep compiled crates in a cache, so only what
  changed compiles again; `docker builder prune` (or `podman image prune --build-cache`) clears it.
- Back up the `homura-data` volume (settings, cookies, sign-in, statistics) and `bot-data` (people
  and groups added from the menu).
- Locked out: `docker compose exec homura /server reset-login` removes the password,
  authenticator app, passkeys and sessions, and prints a setup link.
