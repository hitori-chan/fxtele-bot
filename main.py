#!/usr/bin/env python
"""
fxtele-bot - A Telegram bot that fixes social media links.

Entry point for the application.
"""

import logging
import os
import sys

from telegram.ext import ApplicationBuilder, InlineQueryHandler, MessageHandler, filters

from config.settings import (
    TELEGRAM_ACCESS_STATE_PATH,
    TELEGRAM_ALLOWED_CHAT_IDS,
    TELEGRAM_ALLOWED_USER_IDS,
    TELEGRAM_OWNER_ID,
)
from core.registry import build_handlers
from core.router import MessageRouter
from handlers.commands import load_commands, setup_bot_menu
from handlers.messages import handle_telegram_message, inline_query, leave_unapproved_group
from services.access_control import AccessControl
from services.http import init_http_client, shutdown_http_client

# Configure logging early
logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    level=logging.INFO,
    stream=sys.stdout,
)
logging.getLogger("httpx").setLevel(logging.WARNING)
logging.getLogger("httpcore").setLevel(logging.WARNING)

logger = logging.getLogger(__name__)


def main() -> None:
    """Initialize and run the bot."""
    logger.info("Starting fxtele-bot...")

    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        logger.error("TELEGRAM_BOT_TOKEN not set!")
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")
    if TELEGRAM_OWNER_ID <= 0:
        raise ValueError("Please set telegram.owner_id in config.toml to your numeric Telegram user ID")

    logger.info("Token loaded, building handlers...")

    handlers = build_handlers()

    if not handlers:
        logger.warning("No handlers discovered!")

    router = MessageRouter(handlers)
    access_control = AccessControl.load(
        owner_id=TELEGRAM_OWNER_ID,
        path=TELEGRAM_ACCESS_STATE_PATH,
        seed_user_ids=TELEGRAM_ALLOWED_USER_IDS,
        seed_chat_ids=TELEGRAM_ALLOWED_CHAT_IDS,
    )
    logger.info(f"Loaded {len(handlers)} handlers: {[h.name for h in handlers]}")

    async def post_init(app) -> None:
        await init_http_client(app)
        await setup_bot_menu(app, access_control)

    # Build application
    app = ApplicationBuilder().token(token).post_init(post_init).post_shutdown(shutdown_http_client).build()

    # Load commands
    load_commands(app, access_control)

    # Message handlers
    app.add_handler(MessageHandler(filters.ChatType.GROUPS, leave_unapproved_group(access_control)), group=-1)
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, handle_telegram_message(router, access_control)))
    app.add_handler(InlineQueryHandler(inline_query(router, access_control)))

    logger.info("Bot started and running...")
    app.run_polling()


if __name__ == "__main__":
    main()
