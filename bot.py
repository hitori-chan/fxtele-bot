import os
import re
import logging
from telegram import Update
from telegram.ext import ApplicationBuilder, MessageHandler, filters, ContextTypes

logging.basicConfig(format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.INFO)
logger = logging.getLogger(__name__)


async def rewrite_links(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if update.message is None:
        return

    text = update.message.text or ""
    logger.debug(f"Received message from {update.message.from_user.username}: {text}")

    fixed = re.sub(r"https?://x\.com", "https://fixupx.com", text)

    if fixed != text:
        await update.message.reply_text(fixed)
        logger.debug(f"Replied with: {fixed}")


if __name__ == "__main__":
    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")

    app = ApplicationBuilder().token(token).build()
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, rewrite_links))
    logger.info("Bot started and running...")
    app.run_polling()
