# fxtele-bot

A Telegram bot that fixes social media links and extracts direct media from Facebook and Instagram posts.

## Features

-   **X (Twitter):** Replaces `x.com` and `twitter.com` links with `fixupx.com` for better media embedding.
-   **Instagram:** Extracts direct media URLs from public posts.
-   **TikTok:** Replaces `tiktok.com` and `vt.tiktok.com` links with `tfxktok.com`.
-   **YouTube:** Replaces `youtube.com` and `youtu.be` links with `koutube.com`.
-   **Pixiv:** Replaces `pixiv.net` links with `phixiv.net` for better image embedding.
-   **Facebook:** Extracts direct media URLs from Facebook posts, videos, photos, stories, profiles, and Reels. When login credentials are configured, the bot maintains a persisted browser session and falls back to public extraction if auth fails.
-   **Inline Mode:** Allowed users can use `@your_bot_username <link>` in chats where inline mode is available.
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

2.  **Create a `.env` file from the example and set secrets:**

    ```bash
    cp .env.example .env
    ```

    ```env
    TELEGRAM_BOT_TOKEN=YOUR_TELEGRAM_BOT_TOKEN_HERE
    ```

3.  **Edit `config.toml` and set non-secret runtime config:**

    ```toml
    [telegram]
    owner_id = 123456789
    access_state_path = "/app/data/access_control.json"
    ```

    `telegram.owner_id` is required. The owner is always allowed and cannot be revoked. Access state is persisted as JSON at `telegram.access_state_path`; Docker Compose mounts the `facebook-data` named volume at `/app/data`, so Telegram access state and Facebook auth state survive restarts.

    Optional startup seeds can be used during rollout. Seeds are additive: they approve missing IDs at startup and do not remove users or groups already stored in `telegram.access_state_path`.

    ```toml
    allowed_user_ids = [111111111, 222222222]
    allowed_chat_ids = [-1001234567890]
    ```

4.  **Optional Facebook login:**

    ```env
    FACEBOOK_EMAIL=your-facebook-email@example.com
    FACEBOOK_PASSWORD=your-facebook-password
    FACEBOOK_TOTP_SECRET=BASE32_TOTP_SECRET
    ```

    `FACEBOOK_TOTP_SECRET` is the base32 secret from your two-factor authenticator setup, not a one-time six-digit code. The bot stores the Playwright session state at `facebook.auth_state_path` from `config.toml`; Docker Compose mounts the `facebook-data` named volume at `/app/data` so the session survives restarts without host bind-mount permissions. After 3 consecutive login failures, Playwright login is disabled and Facebook extraction uses the public no-cookie path until you remove the login failure marker or restore a valid session state. If these variables are missing, login fails, the session expires, or an authenticated fetch fails, Facebook extraction retries through the public no-cookie path.

5.  **Start the bot:**

    ```bash
    docker-compose up --build -d
    ```

## Usage

### Access Control

Private messages and inline queries are limited to the owner and allowed users. Group usage is limited to allowed groups. Neutral users can use the bot inside allowed groups, but denied users are blocked everywhere. Unallowed private users and inline users receive no response; if the bot sees an unapproved group, it leaves.

Owner commands:

```
/allow <user_id>
/allow <negative_group_chat_id>
/allow   # as a reply to a user, or in the current group
/deny <user_id>
/deny <negative_group_chat_id>
/deny    # as a reply to a user, or in the current group
/reset <user_id>
/reset <negative_group_chat_id>
/reset   # as a reply to a user, or in the current group
/status
```

`/reset` returns a user to neutral: not allowed in private or inline mode, but usable in allowed groups. For groups, `/deny` and `/reset` both remove the group approval and the bot leaves when run in that group.

If the owner adds the bot to a group, that group is approved and persisted automatically. Previously approved groups stay approved across restarts from the JSON state. Group chat IDs must be negative. Groups that existed before this access-control state was created must be seeded once with `telegram.allowed_chat_ids` or re-approved by the owner, because Telegram does not provide a startup API to enumerate every group the bot is already in.

### In Group Chats
In an approved group, send a social media link and the bot will automatically reply:
```
https://www.facebook.com/share/p/abc123/
https://x.com/user/status/123456789
https://twitter.com/user/status/123456789
https://www.instagram.com/p/abc123/
https://vt.tiktok.com/ZS123456/
https://www.pixiv.net/en/artworks/12345678
```

### Inline Mode
Allowed users can use the bot inline:
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
