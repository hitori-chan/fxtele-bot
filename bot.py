import json
import logging
import os
import re
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse
from uuid import uuid4

import httpx
from telegram import InlineQueryResultArticle, InputTextMessageContent, Update
from telegram.ext import (
    ApplicationBuilder,
    ContextTypes,
    InlineQueryHandler,
    MessageHandler,
    filters,
)

logging.basicConfig(format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.DEBUG)
logger = logging.getLogger(__name__)

# HTTP Configuration
HTTP_TIMEOUT = 10.0
USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:146.0) Gecko/20100101 Firefox/146.0"
INLINE_CACHE_TIME = 300  # 5 minutes

# URL Processing
FACEBOOK_PARAMS_TO_KEEP = {"story_fbid", "id", "fbid"}

# Regex patterns
RE_FACEBOOK = re.compile(r"(https?://(?:www\.)?facebook\.com/\S+)")
RE_X = re.compile(r"https?://(?:www\.)?(?:x|twitter)\.com")
RE_INSTAGRAM = re.compile(r"https?://(?:www\.)?instagram\.com")
RE_TIKTOK = re.compile(r"https?://(?:www\.|vt\.)?tiktok\.com")
RE_FB_HD_URL = re.compile(r'"browser_native_hd_url":"([^"]*)"')
RE_FB_SD_URL = re.compile(r'"browser_native_sd_url":"([^"]*)"')
RE_FB_PHOTO_URL = re.compile(r'"(?:viewer_image|photo_image)"\s*:\s*\{[^}]*?"uri"\s*:\s*"([^"]*)"')
RE_FB_PHOTO_FALLBACK = re.compile(r'"created_time":\d+,"image":{"uri":"([^"]*)"')
RE_FB_THUMBNAIL = re.compile(r'"preferred_thumbnail":{"image":{"uri":"([^"]*)"')


def _decode_json_string(escaped_str: str) -> str | None:
    """Decode a JSON-escaped string."""
    try:
        return json.loads(f'"{escaped_str}"')
    except json.JSONDecodeError:
        return None


def _strip_url_tracking(url: str) -> str:
    """Remove tracking parameters from URL, keeping only essential Facebook params."""
    parsed_url = urlparse(url)
    query_params = parse_qs(parsed_url.query)
    keep_params = {k: query_params[k] for k in FACEBOOK_PARAMS_TO_KEEP if k in query_params}
    new_query = urlencode(keep_params, doseq=True)
    return urlunparse(parsed_url._replace(query=new_query))


async def handle_facebook(text: str) -> dict[str, str | list[str]] | None:
    """Extract direct media URLs from Facebook links."""
    match = RE_FACEBOOK.search(text)
    if not match:
        return None

    url = match.group(1)
    try:
        headers = {
            "Host": "www.facebook.com",
            "User-Agent": USER_AGENT,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Sec-Fetch-Site": "none",
        }
        async with httpx.AsyncClient(follow_redirects=True, timeout=HTTP_TIMEOUT) as client:
            response = await client.get(url, headers=headers)
            response.raise_for_status()
            html = response.text
            final_url = str(response.url)

        # Extract thumbnail
        thumbnail = None
        thumb_match = RE_FB_THUMBNAIL.search(html)
        if thumb_match:
            thumbnail = _decode_json_string(thumb_match.group(1))

        # Try extracting video (HD > SD)
        for pattern in [RE_FB_HD_URL, RE_FB_SD_URL]:
            match = pattern.search(html)
            if match and (media_url := _decode_json_string(match.group(1))):
                return {"type": "media", "urls": [media_url], "thumbnail": thumbnail, "original_url": final_url}

        # If no video, extract all unique photo URLs
        photo_uris = [decoded for raw_uri in RE_FB_PHOTO_URL.findall(html) if (decoded := _decode_json_string(raw_uri))]

        # Fallback for direct photo links (e.g., facebook.com/photo/?fbid=...)
        if not photo_uris:
            photo_uris = [
                decoded for raw_uri in RE_FB_PHOTO_FALLBACK.findall(html) if (decoded := _decode_json_string(raw_uri))
            ]

        if photo_uris:
            unique_photos = list(set(photo_uris))
            return {"type": "media", "urls": unique_photos, "thumbnail": thumbnail, "original_url": final_url}

    except httpx.HTTPError as e:
        logger.error(f"HTTP error accessing {url}: {e}")
    except Exception as e:
        logger.error(f"Unexpected error extracting Facebook media from {url}: {e}")

    return None


async def handle_fixup(text: str) -> dict[str, str] | None:
    """Replace social media URLs with privacy-friendly alternatives."""
    fixed = text
    fixed = RE_X.sub("https://fixupx.com", fixed)
    fixed = RE_INSTAGRAM.sub("https://zzinstagram.com", fixed)
    fixed = RE_TIKTOK.sub("https://www.tfxktok.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None


async def process_text(text: str) -> dict[str, str | list[str]] | None:
    """Process text through all available handlers."""
    handlers = [handle_facebook, handle_fixup]
    for handler in handlers:
        result = await handler(text)
        if result:
            return result
    return None


def _format_media_message(media_url: str, original_url: str | None) -> str:
    """Format Facebook media as HTML with embedded preview and source link."""
    clean_url = _strip_url_tracking(original_url) if original_url else media_url
    return f'<a href="{media_url}">\u200b</a><a href="{clean_url}">Source</a>'


def _build_inline_results(result: dict[str, str | list[str]]) -> list[InlineQueryResultArticle]:
    """Build inline query results from processed text result."""
    match result.get("type"):
        case "media":
            original_url = result.get("original_url")
            thumbnail = result.get("thumbnail")
            urls = result.get("urls", [])

            return [
                InlineQueryResultArticle(
                    id=str(uuid4()),
                    title=f"Media {i + 1}/{len(urls)}",
                    description="Click to send",
                    thumbnail_url=thumbnail or media_url,
                    input_message_content=InputTextMessageContent(
                        _format_media_message(media_url, original_url), parse_mode="HTML"
                    ),
                )
                for i, media_url in enumerate(urls)
            ]

        case "text":
            return [
                InlineQueryResultArticle(
                    id=str(uuid4()),
                    title="Fixed Link",
                    description="Click to send",
                    input_message_content=InputTextMessageContent(result["text"]),
                )
            ]

        case _:
            return []


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle inline queries for link fixing and media extraction."""
    query = update.inline_query.query
    if not query:
        await context.bot.answer_inline_query(update.inline_query.id, [])
        return

    result = await process_text(query)
    results = _build_inline_results(result) if result else []

    try:
        await context.bot.answer_inline_query(update.inline_query.id, results, cache_time=INLINE_CACHE_TIME)
    except Exception as e:
        logger.error(f"Error answering inline query: {e}")
        await context.bot.answer_inline_query(update.inline_query.id, [])


async def message_handler(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle text messages for link fixing and media extraction."""
    if not update.message:
        return

    result = await process_text(update.message.text or "")
    if not result:
        return

    reply_to = update.message.message_id

    match result.get("type"):
        case "media":
            original_url = result.get("original_url")
            for media_url in result.get("urls", []):
                content = _format_media_message(media_url, original_url)
                await update.message.reply_html(content, reply_to_message_id=reply_to)

        case "text":
            await update.message.reply_text(result["text"], reply_to_message_id=reply_to)


def main() -> None:
    """Initialize and run the bot."""
    token = os.getenv("TELEGRAM_BOT_TOKEN")
    if not token:
        raise ValueError("Please set TELEGRAM_BOT_TOKEN environment variable")

    app = ApplicationBuilder().token(token).build()
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, message_handler))
    app.add_handler(InlineQueryHandler(inline_query))

    logger.info("Bot started and running...")
    app.run_polling()


if __name__ == "__main__":
    main()
