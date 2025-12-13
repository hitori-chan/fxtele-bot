# fxtele-bot

A Telegram bot that automatically fixes links from social media platforms.

## Features

-   **X (formerly Twitter) Link Fixing:** Automatically replaces `x.com` links with `fixupx.com` to improve media viewing.
-   **Instagram Link Fixing:** Automatically replaces `instagram.com` links with `zzinstagram.com`.
-   **Facebook Media Extraction:** Extracts direct media URLs (HD videos or photos) from Facebook links.
-   **Inline Bot Support:** Use the bot in any chat by typing `@your_bot_username <link>` to get a preview of the fixed link or extracted media.
-   **Reply Functionality:** The bot replies directly to the original message.

## Setup

1.  **Clone the repo:**

    ```bash
    git clone https://github.com/hitori1403/fxtele-bot.git
    cd fxtele-bot
    ```

2.  **Edit `compose.yml` and set your bot token:**

    ```yaml
    environment:
      TELEGRAM_BOT_TOKEN: "YOUR_TELEGRAM_BOT_TOKEN_HERE"
    ```

3.  **Start the bot:**

    ```bash
    docker-compose up --build -d
    ```

## Important Telegram Bot Settings

To ensure full functionality, configure your bot via [@BotFather](https://telegram.me/BotFather):

-   **Disable privacy mode for group chats:**
    -   Go to `/mybots` → select your bot → Bot Settings → Group Privacy → **Turn OFF**.
-   **Enable Inline Mode:**
    -   Go to `/mybots` → select your bot → Bot Settings → Inline Mode → **Turn ON**.
