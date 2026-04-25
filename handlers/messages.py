"""Message and inline query handlers."""

from html import escape
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

from core.registry import build_handlers
from core.router import MessageRouter
from core.types import LinkFixResult, MediaResult
from services.media_delivery import deliver_media
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)

MEDIA_CAPTION_LIMIT = 1024


def _build_inline_results(result) -> list:
    """Build inline query results from processed result."""
    if result is None:
        return []

    if isinstance(result, MediaResult):
        original_url = result.metadata.original_url
        thumbnail = result.metadata.thumbnail
        urls = list(result.urls)

        results = [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title=f"Source ({len(urls)} media)",
                description="Send the original post link",
                thumbnail_url=thumbnail or (urls[0] if urls else None),
                input_message_content=InputTextMessageContent(
                    _format_source_message(original_url, urls),
                    parse_mode="HTML" if original_url else None,
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
                        parse_mode="HTML",
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
                        thumbnail_url=media_url,
                        title=title,
                        description="Send this photo",
                        caption=caption,
                        parse_mode="HTML",
                    )
                )
        return results

    if isinstance(result, LinkFixResult):
        return [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title="Fixed Link",
                description="Click to send",
                input_message_content=InputTextMessageContent(result.content),
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
    return _source_link(strip_url_tracking(original_url))


def _is_video_url(url: str) -> bool:
    """Detect videos from the direct URL or encoded upstream URI."""
    lowered = unquote(url).lower()
    return ".mp4" in lowered or "video" in lowered


def _video_thumbnail_url(thumbnail: str | None) -> str | None:
    """Return a thumbnail URL only when it is not itself a video."""
    if thumbnail and not _is_video_url(thumbnail):
        return thumbnail
    return None


def _format_media_caption(caption_text: str | None, clean_url: str | None) -> str | None:
    """Build a Telegram media caption within the Bot API limit."""
    caption = escape(caption_text) if caption_text else None
    source = _source_link(clean_url)
    if not caption and not source:
        return None
    if not caption:
        return source
    if not source:
        return _truncate_caption(caption)

    separator = "\n\n"
    max_caption_len = MEDIA_CAPTION_LIMIT - len(separator) - len(source)
    if max_caption_len <= 0:
        return source
    if len(caption) > max_caption_len:
        caption = caption[: max_caption_len - 3].rstrip() + "..."
    return f"{caption}{separator}{source}"


def _source_link(url: str | None) -> str | None:
    """Build a Telegram HTML link to the source URL."""
    if not url:
        return None
    return f'<a href="{escape(url, quote=True)}">Source</a>'


def _truncate_caption(caption: str) -> str:
    """Truncate plain escaped caption text to Telegram's caption limit."""
    if len(caption) <= MEDIA_CAPTION_LIMIT:
        return caption
    return caption[: MEDIA_CAPTION_LIMIT - 3].rstrip() + "..."


def _safe_source_log_url(url: str | None) -> str:
    """Log source URLs only after tracking query cleanup."""
    return strip_url_tracking(url) if url else "<missing-source>"


def handle_telegram_message(router: MessageRouter):
    """Build a Telegram message callback bound to a router."""

    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _ = context
        if not update.message:
            return

        text = update.message.text or ""
        result = await router.handle(text)
        if not result:
            return

        reply_to = update.message.message_id
        if isinstance(result, LinkFixResult):
            await update.message.reply_text(
                result.content,
                reply_to_message_id=reply_to,
            )
            return

        if isinstance(result, MediaResult):
            original_url = result.metadata.original_url
            clean_url = strip_url_tracking(original_url)
            media_caption = _format_media_caption(result.metadata.caption, clean_url)
            norm_original = original_url.rstrip("/")
            media_urls = [url for url in result.urls if url.rstrip("/") != norm_original]
            logger.info(
                "Replying with media result from %s (%d direct media URL(s))",
                _safe_source_log_url(original_url),
                len(media_urls),
            )

            try:
                delivered = await deliver_media(
                    update.message,
                    media_urls,
                    media_caption,
                    reply_to,
                    parse_mode="HTML",
                )
                if delivered:
                    return
            except Exception as e:
                logger.error("Failed to send media group: %r", e)

            logger.info("Falling back to source text reply for %s", _safe_source_log_url(original_url))
            await update.message.reply_text(
                media_caption or clean_url,
                reply_to_message_id=reply_to,
                disable_web_page_preview=True,
                parse_mode="HTML" if media_caption else None,
            )

    return callback


# Create a singleton router for inline queries
# We need to discover handlers here too since inline queries use the same logic
_handlers = None
_router = None


def _get_router():
    """Get or create the message router (lazy initialization)."""
    global _handlers, _router
    if _router is None:
        _handlers = build_handlers()
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
