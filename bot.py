import re
import logging
import json
import requests
import os
from uuid import uuid4
from telegram import Update, InlineQueryResultArticle, InputTextMessageContent
from telegram.ext import ApplicationBuilder, MessageHandler, InlineQueryHandler, filters, ContextTypes

logging.basicConfig(format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.INFO)
logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.)?facebook\.com/\S+)")
RE_X = re.compile(r"https?://x\.com")
RE_INSTAGRAM = re.compile(r"https?://(?:www\.)?instagram\.com")
RE_FB_HD_URL = re.compile(r'"browser_native_hd_url":"([^\"]*)"')
RE_FB_PHOTO_URL = re.compile(r'"photo_image":{"uri":"([^\"]*)"')
RE_FB_THUMBNAIL = re.compile(r'"preferred_thumbnail":{"image":{"uri":"([^\"]*)"')


async def handle_facebook(text: str):
    match = RE_FACEBOOK.search(text)
    if not match:
        return None

    url = match.group(1)
    try:
        headers = {
            "Host": "www.facebook.com",
            "User-Agent": "Mozilla/5.0 (X11; Linux x86_64; rv:146.0) Gecko/20100101 Firefox/146.0",
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Sec-Fetch-Site": "none",
        }
        response = requests.get(url, headers=headers, allow_redirects=True, timeout=10)
        response.raise_for_status()
        html = response.text

        thumbnail = None
        thumb_match = RE_FB_THUMBNAIL.search(html)
        if thumb_match:
            try:
                thumbnail = json.loads(f'"{thumb_match.group(1)}"')
            except json.JSONDecodeError:
                pass

        media_url = None
        for pattern in (RE_FB_HD_URL, RE_FB_PHOTO_URL):
            match = pattern.search(html)
            if match:
                try:
                    media_url = json.loads(f'"{match.group(1)}"')
                    break
                except json.JSONDecodeError:
                    pass

        if media_url:
            return {"type": "media", "url": media_url, "thumbnail": thumbnail}

    except Exception as e:
        logger.error(f"Error extracting Facebook media from {url}: {e}")

    return None


async def handle_fixup(text: str):
    fixed = text
    fixed = RE_X.sub("https://fixupx.com", fixed)
    fixed = RE_INSTAGRAM.sub("https://zzinstagram.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None


async def process_text(text: str):
    handlers = [
        handle_facebook,
        handle_fixup,
    ]

    for handler in handlers:
        result = await handler(text)
        if result:
            return result

    return None


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE):
    query = update.inline_query.query
    if not query:
        return None

    result = await process_text(query)
    results_list = []

    if result:
        if result["type"] == "media":
            media_url = result["url"]
            thumbnail = result["thumbnail"] or media_url
            results_list.append(
                InlineQueryResultArticle(
                    id=str(uuid4()),
                    title="Preview",
                    description="Click to send",
                    thumbnail_url=thumbnail,
                    input_message_content=InputTextMessageContent(
                        f'<a href="{media_url}">Source</a>', parse_mode="HTML"
                    ),
                )
            )
        elif result["type"] == "text":
            fixed_text = result["text"]
            results_list.append(
                InlineQueryResultArticle(
                    id=str(uuid4()),
                    title="Preview",
                    description="Click to send",
                    thumbnail_url=fixed_text,
                    input_message_content=InputTextMessageContent(fixed_text),
                )
            )

    if results_list:
        await context.bot.answer_inline_query(update.inline_query.id, results_list)


async def message_handler(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not update.message:
        return None
    text = update.message.text or ""

    result = await process_text(text)

    if result:
        if result["type"] == "media":
            media_url = result["url"]
            await update.message.reply_html(
                f'<a href="{media_url}">Source</a>', reply_to_message_id=update.message.message_id
            )
        elif result["type"] == "text":
            await update.message.reply_text(result["text"], reply_to_message_id=update.message.message_id)


if __name__ == "__main__":
    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")

    app = ApplicationBuilder().token(token).build()
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, message_handler))
    app.add_handler(InlineQueryHandler(inline_query))
    logger.info("Bot started and running...")
    app.run_polling()
