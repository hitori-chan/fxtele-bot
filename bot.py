import re
import logging
import json
import requests
import os
from uuid import uuid4
from telegram import Update, InlineQueryResultArticle, InputTextMessageContent
from telegram.ext import ApplicationBuilder, MessageHandler, InlineQueryHandler, filters, ContextTypes
from urllib.parse import urlparse, urlunparse, parse_qs, urlencode

logging.basicConfig(format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.DEBUG)
logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.)?facebook\.com/\S+)")
RE_X = re.compile(r"https?://x\.com")
RE_INSTAGRAM = re.compile(r"https?://(?:www\.)?instagram\.com")
RE_FB_HD_URL = re.compile(r'"browser_native_hd_url":"([^"]*)"')
RE_FB_PHOTO_URL = re.compile(r'"(?:viewer_image|photo_image)"\s*:\s*\{[^}]*?"uri"\s*:\s*"([^"]*)"')
RE_FB_THUMBNAIL = re.compile(r'"preferred_thumbnail":{"image":{"uri":"([^"]*)"')


def _strip_url_tracking(url: str) -> str:
    parsed_url = urlparse(url)
    query_params = parse_qs(parsed_url.query)
    keep_params = {k: query_params[k] for k in ["story_fbid", "id", "fbid"] if k in query_params}
    new_query = urlencode(keep_params, doseq=True)
    return urlunparse(parsed_url._replace(query=new_query))


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
                logger.debug(f"JSON decode error for thumbnail URL in {url}")
        else:
            logger.debug(f"No thumbnail regex match found for {url}")

        # 1. Try HD video first
        hd_match = RE_FB_HD_URL.search(html)
        if hd_match:
            try:
                media_url = json.loads(f'"{hd_match.group(1)}"')
                return {"type": "media", "urls": [media_url], "thumbnail": thumbnail, "original_url": response.url}
            except json.JSONDecodeError:
                logger.debug(f"JSON decode error for HD video URL in {url}")

        # 2. If no HD video, find all unique photo URLs
        photo_uris = []
        all_photo_matches = RE_FB_PHOTO_URL.findall(html)
        for raw_uri in all_photo_matches:
            try:
                decoded_uri = json.loads(f'"{raw_uri}"')
                photo_uris.append(decoded_uri)
            except json.JSONDecodeError:
                logger.debug(f"JSON decode error for photo URL '{raw_uri}' in {url}")

        unique_photo_uris = list(set(photo_uris))

        if unique_photo_uris:
            return {"type": "media", "urls": unique_photo_uris, "thumbnail": thumbnail, "original_url": response.url}
        else:
            logger.debug(f"No media (HD or Photo) regex match found for {url}")

    except requests.exceptions.RequestException as e:
        logger.error(f"Network or HTTP error accessing {url}: {e}")
    except Exception as e:
        logger.error(f"Unexpected error extracting Facebook media from {url}: {e}")

    return None


async def handle_fixup(text: str):
    fixed = text
    fixed = RE_X.sub("https://fixupx.com", fixed)
    fixed = RE_INSTAGRAM.sub("https://zzinstagram.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None


async def process_text(text: str):
    handlers = [handle_facebook, handle_fixup]
    for handler in handlers:
        result = await handler(text)
        if result:
            return result
    return None


def _get_fb_content(media_url, original_url_full, thumbnail_from_result):
    original_url = _strip_url_tracking(original_url_full) if original_url_full else media_url
    return f'<a href="{media_url}">\u200b</a><a href="{original_url}">Source</a>', thumbnail_from_result or media_url


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE):
    query = update.inline_query.query
    if not query:
        return None

    result = await process_text(query)
    results_list = []

    if result:
        if result["type"] == "media":
            original_url_full = result.get("original_url")
            thumbnail_from_result = result.get("thumbnail")
            for media_url in result["urls"]:
                content_html, thumb = _get_fb_content(media_url, original_url_full, thumbnail_from_result)
                results_list.append(
                    InlineQueryResultArticle(
                        id=str(uuid4()),
                        title="Preview",
                        description="Click to send",
                        thumbnail_url=thumb,
                        input_message_content=InputTextMessageContent(content_html, parse_mode="HTML"),
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
            original_url_full = result.get("original_url")
            thumbnail_from_result = result.get("thumbnail")
            for media_url in result["urls"]:
                content_html, _ = _get_fb_content(media_url, original_url_full, thumbnail_from_result)
                logger.debug(f"Replying with HTML: {content_html}")
                await update.message.reply_html(content_html, reply_to_message_id=update.message.message_id)
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
