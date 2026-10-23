"""Message and inline query handlers."""

import functools
from html import escape
import logging
import time
from urllib.parse import unquote, urlparse
from uuid import uuid4

from telegram import (
    InlineQueryResultArticle,
    InlineQueryResultPhoto,
    InlineQueryResultVideo,
    InputTextMessageContent,
    LinkPreviewOptions,
    Update,
)
from telegram.error import BadRequest, TelegramError
from telegram.ext import ApplicationHandlerStop, ContextTypes

from core.router import MessageRouter
from core.types import LinkFixResult, MediaResult, TextResult
from services.access_control import AccessControl
from services.media_delivery import deliver_media
from utils.error_text import describe_error
from utils.logging import request_logging
from utils.telegram_errors import bot_absent_from_chat
from utils.telegram_log import chat_label, chat_state_label, chat_username, user_label, user_state_label, user_username
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)

MEDIA_CAPTION_LIMIT = 1024
TEXT_MESSAGE_LIMIT = 4096
_NO_LINK_PREVIEW = LinkPreviewOptions(is_disabled=True)
_SOURCE_LABEL = "Source"
_TEXT_SEPARATOR = "\n\n"
_ELLIPSIS = "..."


def _build_inline_results(result) -> list:
    """Build inline query results from processed result."""
    if result is None:
        return []

    if isinstance(result, MediaResult):
        original_url = result.metadata.original_url
        clean_url = strip_url_tracking(original_url)
        media_caption = _format_media_caption(result.metadata.caption, clean_url)
        thumbnail = result.metadata.thumbnail
        urls = list(result.urls)
        if not urls:
            return []

        results = []
        for index, media_url in enumerate(urls):
            if inline_result := _build_inline_media_result(
                media_url,
                _inline_media_title(index, len(urls)),
                media_caption,
                thumbnail,
            ):
                results.append(inline_result)
        return results

    if isinstance(result, TextResult):
        clean_url = strip_url_tracking(result.metadata.original_url)
        text = _format_media_caption(result.metadata.caption, clean_url, TEXT_MESSAGE_LIMIT)
        if not text:
            return []
        return [
            InlineQueryResultArticle(
                id=str(uuid4()),
                title=result.metadata.title or "Post",
                description=(result.metadata.caption or "")[:100],
                input_message_content=InputTextMessageContent(
                    text, parse_mode="HTML", link_preview_options=_NO_LINK_PREVIEW
                ),
            )
        ]

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


def _is_video_url(url: str) -> bool:
    """Detect videos from the direct URL or encoded upstream URI."""
    lowered = unquote(url).lower()
    return ".mp4" in lowered or "video" in lowered


def _video_thumbnail_url(thumbnail: str | None) -> str | None:
    """Return a thumbnail URL only when it is not itself a video."""
    if thumbnail and not _is_video_url(thumbnail):
        return thumbnail
    return None


def _build_inline_media_result(media_url: str, title: str, caption: str | None, thumbnail: str | None):
    """Build one inline result for one extracted media URL."""
    if _is_video_url(media_url):
        video_thumbnail = _video_thumbnail_url(thumbnail)
        if not video_thumbnail:
            return None
        return InlineQueryResultVideo(
            id=str(uuid4()),
            video_url=media_url,
            mime_type="video/mp4",
            thumbnail_url=video_thumbnail,
            title=title,
            description="Send this video",
            caption=caption,
            parse_mode="HTML",
        )

    return InlineQueryResultPhoto(
        id=str(uuid4()),
        photo_url=media_url,
        thumbnail_url=media_url,
        caption=caption,
        parse_mode="HTML",
    )


def _inline_media_title(index: int, total: int) -> str:
    """Return a compact inline media title for non-grid result types."""
    if total == 1:
        return "Media"
    return f"Media {index + 1}/{total}"


def _format_media_caption(
    caption_text: str | None, clean_url: str | None, limit: int = MEDIA_CAPTION_LIMIT
) -> str | None:
    """Build Telegram HTML text for a post that renders within the given Bot API length limit.

    Telegram applies the limit to the text as displayed, in UTF-16 code units,
    so HTML markup, the link URL and entity escapes (&amp;) cost nothing and
    the budget is spent on the post itself. The text is cut before it is
    escaped, so a cut can never land inside an HTML entity.
    """
    source = _source_link(clean_url)
    if not caption_text and not source:
        return None
    if not caption_text:
        return source
    if not source:
        return escape(_fit_text(caption_text, limit))

    max_caption_units = limit - _utf16_len(_TEXT_SEPARATOR) - _utf16_len(_SOURCE_LABEL)
    if max_caption_units <= 0:
        return source
    return f"{escape(_fit_text(caption_text, max_caption_units))}{_TEXT_SEPARATOR}{source}"


def _utf16_len(text: str) -> int:
    """Length in UTF-16 code units, which is how Telegram counts message text (an emoji is 2)."""
    return len(text.encode("utf-16-le", "surrogatepass")) // 2


def _fit_text(text: str, max_units: int) -> str:
    """Return text unchanged if it fits max_units UTF-16 units, else the longest prefix plus an ellipsis that does."""
    if _utf16_len(text) <= max_units:
        return text

    budget = max_units - _utf16_len(_ELLIPSIS)
    units = 0
    for index, char in enumerate(text):
        units += 2 if ord(char) > 0xFFFF else 1
        if units > budget:
            return text[:index].rstrip() + _ELLIPSIS
    return text


def _source_link(url: str | None) -> str | None:
    """Build a Telegram HTML link to the source URL."""
    if not url:
        return None
    return f'<a href="{escape(url, quote=True)}">{_SOURCE_LABEL}</a>'


def _safe_source_log_url(url: str | None) -> str:
    """Log source URLs only after tracking query cleanup."""
    return strip_url_tracking(url) if url else "<missing-source>"


def _message_label(update: Update) -> str | None:
    """Label a message's log lines with its chat and message id."""
    if not update.message or not update.effective_chat:
        return None
    return f"{update.effective_chat.id}:{update.message.message_id}"


def _inline_label(update: Update) -> str | None:
    """Label an inline query's log lines with the tail of its id."""
    return f"inline:{update.inline_query.id[-6:]}" if update.inline_query else None


def _with_request_label(label_for):
    """Run a handler callback inside request_logging so all its log lines carry the request's label."""

    def decorator(callback):
        @functools.wraps(callback)
        async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
            with request_logging(label_for(update)):
                await callback(update, context)

        return wrapper

    return decorator


def handle_telegram_message(router: MessageRouter, access_control: AccessControl):
    """Build a Telegram message callback bound to a router."""

    @_with_request_label(_message_label)
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not update.message:
            return
        _remember_update(access_control, update)
        allowed = await _message_access_allowed(update, context, access_control)
        if not allowed:
            return

        text = update.message.text or ""
        started = time.monotonic()
        result = await router.handle(text)
        if not result:
            return
        extracted_in = time.monotonic() - started

        sender = f"{user_label(update.effective_user)} in {chat_label(update.effective_chat)}"
        reply_to = update.message.message_id
        if isinstance(result, LinkFixResult):
            logger.info("Message from %s: fixed link in %.1fs.", sender, extracted_in)
            await _reply_text_safely(
                update,
                result.content,
                reply_to=reply_to,
            )
            return

        if isinstance(result, TextResult):
            original_url = result.metadata.original_url
            clean_url = strip_url_tracking(original_url)
            logger.info(
                "Message from %s: text-only %s post from %s in %.1fs.",
                sender,
                _platform_name(original_url),
                _safe_source_log_url(original_url),
                extracted_in,
            )
            text = _format_media_caption(result.metadata.caption, clean_url, TEXT_MESSAGE_LIMIT)
            await _reply_with_source_text(update, text, clean_url, reply_to)
            return

        if isinstance(result, MediaResult):
            original_url = result.metadata.original_url
            clean_url = strip_url_tracking(original_url)
            media_caption = _format_media_caption(result.metadata.caption, clean_url)
            norm_original = original_url.rstrip("/")
            media_urls = [url for url in result.urls if url.rstrip("/") != norm_original]
            logger.info(
                "Message from %s: %d %s media from %s in %.1fs.",
                sender,
                len(media_urls),
                _platform_name(original_url),
                _safe_source_log_url(original_url),
                extracted_in,
            )

            delivery_started = time.monotonic()
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
                logger.error(
                    "Failed to deliver media from %s after %.1fs: %s.",
                    _safe_source_log_url(original_url),
                    time.monotonic() - delivery_started,
                    describe_error(e),
                )

            logger.warning("Falling back to a source-link reply for %s.", _safe_source_log_url(original_url))
            await _reply_with_source_text(update, media_caption, clean_url, reply_to)

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
            and not getattr(user, "is_bot", False)
            and not access_control.is_chat_allowed(chat.id)
        ):
            logger.info(
                "Unapproved group %s received interaction from %s: leaving.",
                chat_label(chat),
                user_label(user),
            )
            await _leave_chat_safely(context, chat.id)
            raise ApplicationHandlerStop

    return callback


def inline_query(router: MessageRouter, access_control: AccessControl):
    """Build an inline query callback bound to a router and access control."""

    @_with_request_label(_inline_label)
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        query_update = update.inline_query
        if not query_update:
            return
        access_control.remember_user(
            query_update.from_user.id,
            user_state_label(query_update.from_user),
            user_username(query_update.from_user),
        )
        allowed = access_control.is_user_allowed(query_update.from_user.id)
        logger.info(
            "Inline query from %s with %d characters: %s.",
            user_label(query_update.from_user),
            len(query_update.query or ""),
            "allowed" if allowed else "blocked",
        )

        if not allowed:
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

            is_media_result = isinstance(result, MediaResult)
            await context.bot.answer_inline_query(
                query_update.id,
                results,
                cache_time=0 if is_media_result else INLINE_CACHE_TIME,
                is_personal=is_media_result,
            )
            logger.info(
                "Answered inline query for %s with %d result(s).", user_label(query_update.from_user), len(results)
            )
        except Exception as e:
            logger.error("Failed to answer inline query: %s.", describe_error(e))

    return callback


def _remember_update(access_control: AccessControl, update: Update) -> None:
    user = update.effective_user
    chat = update.effective_chat
    access_control.remember_user(user.id if user else None, user_state_label(user), user_username(user))
    access_control.remember_chat(chat.id if chat else None, chat_state_label(chat), chat_username(chat))


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
        if getattr(user, "is_bot", False):
            return False
        if access_control.is_user_denied(user.id if user else None):
            return False
        if access_control.is_chat_allowed(chat.id):
            return True
        return False

    return False


async def _leave_chat_safely(context: ContextTypes.DEFAULT_TYPE, chat_id: int) -> None:
    """Leave an unapproved group without surfacing already-left errors."""
    try:
        await context.bot.leave_chat(chat_id)
    except TelegramError as e:
        if bot_absent_from_chat(e):
            logger.info("Leave already complete for unapproved chat %s; bot is absent.", chat_id)
            return
        logger.warning("Unexpected Telegram error while leaving unapproved chat %s: %s.", chat_id, e)


async def _reply_with_source_text(
    update: Update, text: str | None, clean_url: str | None, reply_to: int | None
) -> None:
    """Reply with formatted post text, or just the source URL when there is none."""
    await _reply_text_safely(
        update,
        text or clean_url,
        reply_to=reply_to,
        link_preview_options=_NO_LINK_PREVIEW,
        parse_mode="HTML" if text else None,
    )


async def _reply_text_safely(update: Update, text: str | None, reply_to: int | None, **kwargs) -> None:
    if not update.message or not text:
        return
    try:
        await update.message.reply_text(text, reply_to_message_id=reply_to, **kwargs)
    except BadRequest as e:
        if reply_to is not None and _reply_target_missing(e):
            logger.warning("Reply target disappeared; sending text without a reply target.")
            await update.message.reply_text(text, **kwargs)
            return
        raise


def _reply_target_missing(error: BadRequest) -> bool:
    return "message to be replied not found" in str(error).lower()


def _platform_name(url: str | None) -> str:
    if not url:
        return "unknown"
    hostname = urlparse(url).hostname or ""
    if "facebook." in hostname or hostname == "fb.watch":
        return "Facebook"
    if "instagram." in hostname:
        return "Instagram"
    if "reddit." in hostname or hostname == "redd.it":
        return "Reddit"
    return "unknown"
