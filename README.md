# fxtele-bot

Telegram bot for previewing Facebook and Instagram media links.

It also hardcodes a few embed link fixers:

- X/Twitter -> `fixupx.com`
- TikTok -> `tfxktok.com`
- YouTube -> `koutube.com`
- Pixiv -> `phixiv.net`

## Config

Secrets go in `.env`:

```env
TELEGRAM_BOT_TOKEN=YOUR_TELEGRAM_BOT_TOKEN
FACEBOOK_EMAIL=
FACEBOOK_PASSWORD=
FACEBOOK_TOTP_SECRET=
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

[facebook]
auth_state_path = "/app/data/facebook_state.json"
```

`owner_id` is required. Group chat IDs must be negative, usually `-100...`.

## Access

Only the owner can manage access. Telegram group admins do not matter.

User states:

- `allowed`: private, inline, and allowed groups.
- `neutral`: allowed groups only.
- `denied`: blocked everywhere, including allowed groups.

Group states:

- `allowed`: all non-denied members can use the bot.
- `neutral`: bot leaves when it sees the group.

Owner commands:

```text
/allow <user_id|negative_group_chat_id>
/deny <user_id|negative_group_chat_id>
/reset <user_id|negative_group_chat_id>
/status
```

Without an argument:

- reply to a user to target that user.
- run in a group to target the current group.

`/reset` makes a user neutral. For groups, `/deny` and `/reset` both remove approval; if run inside that group, the bot leaves.

## State

Access state is JSON at `telegram.access_state_path`:

```json
{
  "allowed_chat_ids": [],
  "allowed_user_ids": [],
  "denied_user_ids": []
}
```

No version field. Invalid JSON or invalid ID types fail startup.

If the file is deleted, the bot recreates it from `config.toml` seeds. Old groups must be seeded in `allowed_chat_ids` or re-approved by the owner.

## Run

```bash
podman compose -f compose.yml up --build -d
podman compose -f compose.yml logs -f telegram-bot
```

Docker Compose also works:

```bash
docker compose -f compose.yml up --build -d
```

The named volume `facebook-data` stores `/app/data`, including access state and Facebook auth state.

## Telegram Setup

In BotFather:

- Disable group privacy mode.
- Enable inline mode if needed.

If the owner adds the bot to a group, it is auto-approved. If anyone else adds it, the bot leaves.

## Dev

Use `uv`:

```bash
uv run ruff check .
uv run ruff format --check .
uv run python -m compileall .
```
