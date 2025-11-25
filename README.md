# tele-fixupx-bot

A Telegram bot that automatically replaces `x.com` links with `fixupx.com` in group chats.
The bot replies to messages containing `x.com` links.

## Setup

1. Clone the repo:

```bash
git clone https://github.com/hitori1403/tele-fixupx-bot.git
cd tele-fixupx-bot
```

2. Edit `compose.yml` and set your bot token:

```yaml
environment:
  TELEGRAM_BOT_TOKEN: "YOUR_TELEGRAM_BOT_TOKEN_HERE"
```

3. Start the bot:

```bash
docker-compose up --build -d
```

## Important

- Add the bot to your group.

- Disable privacy mode in BotFather — otherwise the bot will not see normal messages.
  - Talk to [@BotFather](https://telegram.me/BotFather)

  - `/mybots` → select your bot → Bot Settings → Group Privacy → Turn OFF
