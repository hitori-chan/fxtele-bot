"""Message and inline query handlers."""

import logging
from urllib.parse import unquote
from uuid import uuid4

from telegram import (
    InlineQueryResultArticle,
    InlineQueryResultPhoto,
    InlineQueryResultVideo,
    InputTextMessageContent,
    Update,
)
from telegram.ext import ContextTypes

from core.registry import discover_handlers
from core.router import MessageRouter
from core.types import HandlerType
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)


def _build_inline_results(result) -> list:
    """Build inline query results from processed result."""
    if result is None:
        return []

    if result.type == HandlerType.MEDIA_EXTRACTOR:
        original_url = result.metadata.get("original_url") if result.metadata else None
        thumbnail = result.metadata.get("thumbnail") if result.metadata else None
        urls = result.content if isinstance(result.content, list) else [result.content]

        results = [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title=f"Source ({len(urls)} media)",
                description="Send the original post link",
                thumbnail_url=thumbnail or (urls[0] if urls else None),
                input_message_content=InputTextMessageContent(
                    _format_source_message(original_url, urls),
                    disable_web_page_preview=True,
                ),
            )
        ]

        for i, media_url in enumerate(urls):
            title = f"Media {i + 1}/{len(urls)}"
            caption = _format_source_caption(original_url)
            video_thumbnail = _video_thumbnail_url(thumbnail)
            if _is_video_url(media_url) and video_thumbnail:
                results.append(
                    InlineQueryResultVideo(
                        id=str(uuid4()),
                        video_url=media_url,
                        mime_type="video/mp4",
                        thumbnail_url=video_thumbnail,
                        title=title,
                        description="Send this video",
                        caption=caption,
                    )
                )
            elif _is_video_url(media_url):
                results.append(
                    InlineQueryResultArticle(
                        id=str(uuid4()),
                        title=title,
                        description="Send this video link",
                        thumbnail_url=thumbnail,
                        input_message_content=InputTextMessageContent(media_url),
                    )
                )
            else:
                results.append(
                    InlineQueryResultPhoto(
                        id=str(uuid4()),
                        photo_url=media_url,
                        thumbnail_url=thumbnail or media_url,
                        title=title,
                        description="Send this photo",
                        caption=caption,
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


def _format_source_message(original_url: str | None, urls: list[str]) -> str:
    """Format a safe inline article message without media URL entities."""
    if not original_url:
        return urls[0]
    return _format_source_caption(original_url)


def _format_source_caption(original_url: str | None) -> str | None:
    """Format the original post URL for inline media captions."""
    if not original_url:
        return None
    return strip_url_tracking(original_url)


def _is_video_url(url: str) -> bool:
    """Detect videos from the direct URL or encoded upstream URI."""
    lowered = unquote(url).lower()
    return ".mp4" in lowered or "video" in lowered


def _video_thumbnail_url(thumbnail: str | None) -> str | None:
    """Return a thumbnail URL only when it is not itself a video."""
    if thumbnail and not _is_video_url(thumbnail):
        return thumbnail
    return None


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

        await context.bot.answer_inline_query(update.inline_query.id, results, cache_time=INLINE_CACHE_TIME)
    except Exception as e:
        logger.error(f"Error answering inline query: {e}")
        await context.bot.answer_inline_query(update.inline_query.id, [])
