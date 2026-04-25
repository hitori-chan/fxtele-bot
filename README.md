# fxtele-bot

A Telegram bot that fixes social media links and extracts direct media from Facebook and Instagram posts.

## Features

-   **X (Twitter):** Replaces `x.com` and `twitter.com` links with `fixupx.com` for better media embedding.
-   **Instagram:** Extracts direct media URLs from public posts.
-   **TikTok:** Replaces `tiktok.com` and `vt.tiktok.com` links with `tfxktok.com`.
-   **YouTube:** Replaces `youtube.com` and `youtu.be` links with `koutube.com`.
-   **Pixiv:** Replaces `pixiv.net` links with `phixiv.net` for better image embedding.
-   **Facebook:** Extracts direct media URLs from Facebook posts, videos, photos, stories, profiles, and Reels. When login credentials are configured, the bot maintains a persisted browser session and falls back to public extraction if auth fails.
-   **Inline Mode:** Use `@your_bot_username <link>` in any chat (works in DMs, groups, channels).
-   **Auto-Reply:** Bot automatically replies to social media links posted in group chats.

## Project Structure

-   `main.py`: Entry point for the application.
-   `config/`: Configuration and constants.
-   `handlers/`: Telegram callbacks plus platform extraction and link fixing handlers.
-   `services/`: HTTP client lifecycle, media delivery, and Facebook Playwright auth.
-   `utils/`: Shared text and URL helpers.

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

4.  **Optional Facebook login:**

    ```env
    FACEBOOK_EMAIL=your-facebook-email@example.com
    FACEBOOK_PASSWORD=your-facebook-password
    FACEBOOK_TOTP_SECRET=BASE32_TOTP_SECRET
    FACEBOOK_AUTH_STATE_PATH=/app/data/facebook_state.json
    ```

    `FACEBOOK_TOTP_SECRET` is the base32 secret from your two-factor authenticator setup, not a one-time six-digit code. The bot stores the Playwright session state at `FACEBOOK_AUTH_STATE_PATH`; Docker Compose mounts the `facebook-data` named volume at `/app/data` so the session survives restarts without host bind-mount permissions. After 3 consecutive login failures, Playwright login is disabled and Facebook extraction uses the public no-cookie path until you remove the login failure marker or restore a valid session state. If these variables are missing, login fails, the session expires, or an authenticated fetch fails, Facebook extraction retries through the public no-cookie path.

5.  **Start the bot:**

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
