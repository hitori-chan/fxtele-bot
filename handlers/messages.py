"""Message and inline query handlers."""

import logging
from uuid import uuid4

from telegram import InlineQueryResultArticle, InputTextMessageContent, Update
from telegram.ext import ContextTypes

from core.registry import discover_handlers
from core.router import MessageRouter
from core.types import HandlerType
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)


def _build_inline_results(result) -> list[InlineQueryResultArticle]:
    """Build inline query results from processed result."""
    if result is None:
        return []

    if result.type == HandlerType.MEDIA_EXTRACTOR:
        original_url = result.metadata.get("original_url") if result.metadata else None
        thumbnail = result.metadata.get("thumbnail") if result.metadata else None
        urls = result.content if isinstance(result.content, list) else [result.content]

        # Result 1: All media in one message
        results = [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title=f"All Media ({len(urls)})",
                description="Send all media in one message",
                thumbnail_url=thumbnail or (urls[0] if urls else None),
                input_message_content=InputTextMessageContent(
                    _format_media_message(urls, original_url),
                    parse_mode="HTML",
                ),
            )
        ]

        # Add individual results if there's more than one
        if len(urls) > 1:
            for i, media_url in enumerate(urls):
                results.append(
                    InlineQueryResultArticle(
                        id=str(uuid4()),
                        title=f"Media {i + 1}/{len(urls)}",
                        description="Send only this media",
                        thumbnail_url=thumbnail or media_url,
                        input_message_content=InputTextMessageContent(
                            _format_media_message([media_url], original_url),
                            parse_mode="HTML",
                        ),
                    )
                )

        return results

    elif result.type == HandlerType.LINK_FIXER:
        return [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title="Fixed Link",
                description="Click to send",
                input_message_content=InputTextMessageContent(str(result.content)),
            )
        ]

    return []


def _format_media_message(urls: list[str], original_url: str | None) -> str:
    """Format media as HTML with embedded preview and source link."""
    if not urls:
        return ""
    clean_url = strip_url_tracking(original_url) if original_url else urls[0]
    previews = "".join(f'<a href="{url}">\u200b</a>' for url in urls)
    return f"{previews}{clean_url}"


# Create a singleton router for inline queries
# We need to discover handlers here too since inline queries use the same logic
_handlers = None
_router = None


def _get_router():
    """Get or create the message router (lazy initialization)."""
    global _handlers, _router
    if _router is None:
        _handlers = discover_handlers(
            "handlers.media_extractors",
            "handlers.link_fixers",
        )
        _router = MessageRouter(_handlers)
    return _router


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle inline queries for link fixing and media extraction."""
    query = update.inline_query.query
    if not query:
        await context.bot.answer_inline_query(update.inline_query.id, [])
        return

    router = _get_router()
    result = await router.handle(query)
    results = _build_inline_results(result)

    try:
        from config import INLINE_CACHE_TIME

        await context.bot.answer_inline_query(
            update.inline_query.id, results, cache_time=INLINE_CACHE_TIME
        )
    except Exception as e:
        logger.error(f"Error answering inline query: {e}")
        await context.bot.answer_inline_query(update.inline_query.id, [])
