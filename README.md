# fxtele-bot

A Telegram bot that fixes social media links and extracts direct media from public Facebook posts.

## Features

-   **X (Twitter):** Replaces `x.com` and `twitter.com` links with `fixupx.com` for better media embedding.
-   **Instagram:** Extracts direct media URLs from public posts.
-   **TikTok:** Replaces `tiktok.com` and `vt.tiktok.com` links with `tfxktok.com`.
-   **YouTube:** Replaces `youtube.com` and `youtu.be` links with `koutube.com`.
-   **Pixiv:** Replaces `pixiv.net` links with `phixiv.net` for better image embedding.
-   **Facebook:** Extracts direct media URLs from public Facebook posts, videos, photos, and Reels.
-   **Inline Mode:** Use `@your_bot_username <link>` in any chat (works in DMs, groups, channels).
-   **Auto-Reply:** Bot automatically replies to social media links posted in group chats.

## Project Structure

-   `main.py`: Entry point for the application.
-   `config/`: Configuration and constants.
-   `handlers/`: Platform-specific extraction logic (Facebook, X, Instagram, TikTok).
-   `utils/`: Shared utilities for HTTP and text processing.

## Setup

1.  **Clone the repo:**

    ```bash
    git clone https://github.com/hitori1403/fxtele-bot.git
    cd fxtele-bot
    ```

2.  **Create a `.env` file from the example:**

    ```bash
    cp .env.example .env
    ```

3.  **Edit `.env` and set your bot token:**

    ```env
    TELEGRAM_BOT_TOKEN=YOUR_TELEGRAM_BOT_TOKEN_HERE
    ```

4.  **Start the bot:**

    ```bash
    docker-compose up --build -d
    ```

## Usage

### In Group Chats
Simply send a social media link and the bot will automatically reply:
```
https://www.facebook.com/share/p/abc123/
https://x.com/user/status/123456789
https://twitter.com/user/status/123456789
https://www.instagram.com/p/abc123/
https://vt.tiktok.com/ZS123456/
https://www.pixiv.net/en/artworks/12345678
```

### In Private Chats (Inline Mode)
Use the bot in any chat without adding it:
```
@your_bot_username https://www.facebook.com/share/p/abc123/
```
Then click the result to send the media.

## Important Telegram Bot Settings

To ensure full functionality, configure your bot via [@BotFather](https://telegram.me/BotFather):

-   **Disable privacy mode for group chats:**
    -   `/mybots` → select your bot → Bot Settings → Group Privacy → **Turn OFF**
-   **Enable Inline Mode:**
    -   `/mybots` → select your bot → Bot Settings → Inline Mode → **Turn ON**
