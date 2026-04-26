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
from telegram.error import BadRequest, TelegramError
from telegram.ext import ContextTypes

from core.router import MessageRouter
from core.types import LinkFixResult, MediaResult
from services.access_control import AccessControl
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
        if not urls and not original_url:
            return []

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


def handle_telegram_message(router: MessageRouter, access_control: AccessControl):
    """Build a Telegram message callback bound to a router."""

    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not update.message:
            return
        if not await _message_access_allowed(update, context, access_control):
            return

        text = update.message.text or ""
        result = await router.handle(text)
        if not result:
            return

        reply_to = update.message.message_id
        if isinstance(result, LinkFixResult):
            await _reply_text_safely(
                update,
                result.content,
                reply_to=reply_to,
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
            await _reply_text_safely(
                update,
                media_caption or clean_url,
                reply_to=reply_to,
                disable_web_page_preview=True,
                parse_mode="HTML" if media_caption else None,
            )

    return callback


def leave_unapproved_group(access_control: AccessControl):
    """Build a guard callback that silently leaves unapproved groups."""

    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        chat = update.effective_chat
        user = update.effective_user
        if (
            chat
            and chat.type in {"group", "supergroup"}
            and (not user or user.id != access_control.owner_id)
            and not access_control.is_chat_allowed(chat.id)
        ):
            await _leave_chat_safely(context, chat.id)

    return callback


def inline_query(router: MessageRouter, access_control: AccessControl):
    """Build an inline query callback bound to a router and access control."""

    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        query_update = update.inline_query
        if not query_update:
            return

        if not access_control.is_user_allowed(query_update.from_user.id):
            await context.bot.answer_inline_query(query_update.id, [], cache_time=0)
            return

        query = query_update.query
        if not query:
            await context.bot.answer_inline_query(query_update.id, [])
            return

        result = await router.handle(query)
        results = _build_inline_results(result)

        try:
            from config import INLINE_CACHE_TIME

            await context.bot.answer_inline_query(query_update.id, results, cache_time=INLINE_CACHE_TIME)
        except Exception as e:
            logger.error(f"Error answering inline query: {e}")

    return callback


async def _message_access_allowed(
    update: Update,
    context: ContextTypes.DEFAULT_TYPE,
    access_control: AccessControl,
) -> bool:
    chat = update.effective_chat
    user = update.effective_user
    message = update.message
    if not chat or not message:
        return False

    if chat.type == "private":
        if access_control.is_user_allowed(user.id if user else None):
            return True
        return False

    if chat.type in {"group", "supergroup"}:
        if access_control.is_user_denied(user.id if user else None):
            return False
        if access_control.is_chat_allowed(chat.id):
            return True
        await _leave_chat_safely(context, chat.id)
        return False

    return False


async def _leave_chat_safely(context: ContextTypes.DEFAULT_TYPE, chat_id: int) -> None:
    """Leave an unapproved group without surfacing already-left errors."""
    try:
        await context.bot.leave_chat(chat_id)
    except TelegramError as e:
        logger.info("Could not leave unapproved chat %s: %s", chat_id, e)


async def _reply_text_safely(update: Update, text: str | None, reply_to: int | None, **kwargs) -> None:
    if not update.message or not text:
        return
    try:
        await update.message.reply_text(text, reply_to_message_id=reply_to, **kwargs)
    except BadRequest as e:
        if reply_to is not None and _reply_target_missing(e):
            logger.info("Reply target disappeared; sending text without reply target")
            await update.message.reply_text(text, **kwargs)
            return
        raise


def _reply_target_missing(error: BadRequest) -> bool:
    return "message to be replied not found" in str(error).lower()
