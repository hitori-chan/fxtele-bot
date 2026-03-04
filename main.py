#!/usr/bin/env python
"""
fxtele-bot - A Telegram bot that fixes social media links.

Entry point for the application.
"""

import logging
import os
import sys

from telegram.ext import ApplicationBuilder, InlineQueryHandler, MessageHandler, filters

from core.router import MessageRouter
from core.registry import discover_handlers
from handlers.commands import load_commands
from handlers.messages import inline_query
from services.http import init_http_client, shutdown_http_client

# Configure logging early
logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    level=logging.INFO,
    stream=sys.stdout,
)

logger = logging.getLogger(__name__)


def main() -> None:
    """Initialize and run the bot."""
    logger.info("Starting fxtele-bot...")

    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        logger.error("TELEGRAM_BOT_TOKEN not set!")
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")

    logger.info("Token loaded, discovering handlers...")

    # Discover and instantiate all handlers
    # Order matters: media extractors first, then link fixers
    handlers = discover_handlers(
        "handlers.media_extractors",
        "handlers.link_fixers",
    )

    if not handlers:
        logger.warning("No handlers discovered!")

    router = MessageRouter(handlers)
    logger.info(f"Loaded {len(handlers)} handlers: {[h.name for h in handlers]}")

    # Build application
    app = (
        ApplicationBuilder()
        .token(token)
        .post_init(init_http_client)
        .post_shutdown(shutdown_http_client)
        .build()
    )

    # Load commands
    load_commands(app)

    # Message handlers
    app.add_handler(
        MessageHandler(filters.TEXT & ~filters.COMMAND, router.handle_telegram_message)
    )
    app.add_handler(InlineQueryHandler(inline_query))

    logger.info("Bot started and running...")
    app.run_polling()


if __name__ == "__main__":
    main()
