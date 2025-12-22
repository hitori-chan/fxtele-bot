import os
import logging
from telegram.ext import ApplicationBuilder, MessageHandler, InlineQueryHandler, filters

from handlers.messages import message_handler, inline_query
from utils.http_client import init_http_client, shutdown_http_client

logger = logging.getLogger(__name__)


def main() -> None:
    """Initialize and run the bot."""
    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")

    app = ApplicationBuilder().token(token).post_init(init_http_client).post_shutdown(shutdown_http_client).build()
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, message_handler))
    app.add_handler(InlineQueryHandler(inline_query))

    logger.info("Bot started and running...")
    app.run_polling()


if __name__ == "__main__":
    main()
