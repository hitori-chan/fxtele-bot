import logging
from uuid import uuid4
from telegram import Update, InlineQueryResultArticle, InputTextMessageContent
from telegram.ext import ContextTypes

from handlers.facebook import handle_facebook
from handlers.x import handle_x
from handlers.instagram import handle_instagram
from handlers.tiktok import handle_tiktok
from utils.text import strip_url_tracking
from config import INLINE_CACHE_TIME

logger = logging.getLogger(__name__)


async def process_text(text: str) -> dict[str, str | list[str]] | None:
    """Process text through all available handlers."""
    handlers = [handle_facebook, handle_x, handle_instagram, handle_tiktok]
    for handler in handlers:
        result = await handler(text)
        if result:
            return result
    return None


def _format_media_message(media_url: str, original_url: str | None) -> str:
    """Format Facebook media as HTML with embedded preview and source link."""
    clean_url = strip_url_tracking(original_url) if original_url else media_url
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
