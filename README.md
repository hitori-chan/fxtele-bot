# fxtele-bot

Telegram bot for previewing Facebook, Instagram, and Reddit media links.

It also hardcodes a few embed link fixers:

- X/Twitter -> `fixupx.com`
- TikTok -> `tfxktok.com`
- YouTube -> `koutube.com`
- Pixiv -> `phixiv.net`

## Config

Secrets go in `.env`:

```env
TELEGRAM_BOT_TOKEN=YOUR_TELEGRAM_BOT_TOKEN
```

Non-secrets go in `config.toml`:

```bash
cp config.example.toml config.toml
```

```toml
[telegram]
owner_id = 123456789
access_state_path = "/app/data/access_control.json"
allowed_user_ids = []
allowed_chat_ids = []
inline_cache_time = 300
max_media_bytes = 52428800
max_source_video_bytes = 268435456
```

`owner_id` is required. Group chat IDs must be negative, usually `-100...`.

`max_media_bytes` is Telegram's bot upload limit. Videos above it are downloaded up to `max_source_video_bytes` and re-encoded with `ffmpeg` to fit; the Docker image includes it. The encode targets constant quality (x264 CRF 23) and caps the bitrate so the result fits, so easy footage ends up well under the limit. When the cap is too low for full resolution the picture is scaled down by its shorter side, so portrait reels stay portrait. Without `ffmpeg`, or if a video is too long to compress to a watchable quality, the bot replies with the source link instead.

## Access

Only the owner can manage access. Telegram group admins do not matter.

Owner commands:

```text
/allow <user_id|negative_group_chat_id>
/deny <user_id|negative_group_chat_id>
/reset <user_id|negative_group_chat_id>
/status
```

Without an argument:

- reply to a user to target that user.
- otherwise the bot shows usage help.

`/allow <group_id>` records owner intent even if the bot is not currently in that group.
`/deny <group_id>` and `/reset <group_id>` remove group approval.
`/status` is private-only to avoid leaking access lists in groups.

See [docs/access-control.md](docs/access-control.md) for the technical access rules, group admission flow, stale group cleanup, and command menu behavior.

## State

Access state is JSON at `telegram.access_state_path`:

```json
{
  "allowed_chats": [],
  "allowed_users": [],
  "denied_users": []
}
```

No version field. Invalid JSON or invalid ID types fail startup.

If the file is deleted, the bot recreates it from `config.toml` seeds. Old groups must be seeded in `allowed_chat_ids` in `config.toml` or re-approved by the owner.

## Run

```bash
podman compose -f compose.yml up --build -d
podman compose -f compose.yml logs -f telegram-bot
```

The Dockerfile uses short image names, so podman needs Docker Hub as a search registry. Without a `registries.conf` the build fails with `short-name ... did not resolve`:

```bash
mkdir -p ~/.config/containers
echo 'unqualified-search-registries = ["docker.io"]' >> ~/.config/containers/registries.conf
```

Docker Compose also works:

```bash
docker compose -f compose.yml up --build -d
```

The named volume `data` stores `/app/data`, including access state and browser cookie exports.

Cookie files are conventional paths, not TOML config:

- Facebook: `/app/data/cookies/facebook.json`
- Reddit: `/app/data/cookies/reddit.json`

The files must be raw browser-exported cookie JSON lists. Facebook and Reddit use those files directly. If a file is missing, invalid, expired, or unusable, the extractor falls back to an unauthenticated request.

## Telegram Setup

In BotFather:

- Disable group privacy mode.
- Enable inline mode if needed.

If the owner adds the bot to a group, it is auto-approved. Allowed users can add the bot back to groups that are already allowed.

## Dev

Use `uv`:

```bash
uv run ruff check .
uv run ruff format --check .
uv run python -m compileall .
```
