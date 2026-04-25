"""Message routing and dispatch."""

import logging
import os
import uuid
import httpx
from urllib.parse import unquote
from typing import Sequence

from telegram import InputMediaPhoto, InputMediaVideo, Message, Update
from telegram.ext import ContextTypes

from .types import HandlerResult, HandlerType, MessageHandler
from services.http import get_client
from config import HTTP_TIMEOUT
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)

TEMP_DIR = "/tmp/fx-telebot/"
MEDIA_GROUP_LIMIT = 10
MEDIA_CAPTION_LIMIT = 1024


class MessageRouter:
    """Routes messages to appropriate handlers."""

    def __init__(self, handlers: Sequence[MessageHandler]):
        """
        Initialize router with handlers.

        Args:
            handlers: List of handler instances
        """
        self.handlers = handlers
        # Ensure temp directory exists
        os.makedirs(TEMP_DIR, exist_ok=True)
        logger.info(f"Initialized router with {len(handlers)} handlers")

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Process text through all handlers until one matches.

        Args:
            text: Message text to process

        Returns:
            First matching result, or None if no handler matched
        """
        for handler in self.handlers:
            try:
                result = await handler.handle(text)
                if result is not None:
                    logger.debug(f"Handler {handler.name} matched")
                    return result
            except Exception as e:
                logger.error(f"Handler {handler.name} failed: {e}")

        return None

    async def handle_telegram_message(self, update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        """
        Handle a Telegram message update.

        Args:
            update: Telegram update
            context: Callback context
        """
        if not update.message:
            return

        text = update.message.text or ""
        result = await self.handle(text)

        if not result:
            return

        reply_to = update.message.message_id
        client = get_client()

        if result.type == HandlerType.LINK_FIXER:
            await update.message.reply_text(
                str(result.content),
                reply_to_message_id=reply_to,
            )

        elif result.type == HandlerType.MEDIA_EXTRACTOR:
            original_url = result.metadata.get("original_url") if result.metadata else None
            caption_text = result.metadata.get("caption") if result.metadata else None
            urls = result.content if isinstance(result.content, list) else [result.content]

            clean_url = strip_url_tracking(original_url) if original_url else None
            media_caption = _format_media_caption(caption_text, clean_url)
            norm_original = original_url.rstrip("/") if original_url else ""
            media_files: list[tuple[str, bool]] = []

            try:
                for media_url in urls:
                    norm_media = media_url.rstrip("/")
                    if norm_media == norm_original:
                        continue
                    try:
                        media_files.append(await _download_media(client, media_url))
                    except Exception as e:
                        logger.error("Failed to download media %s: %r", media_url, e)

                if media_files:
                    await _reply_with_media(
                        update.message,
                        media_files,
                        media_caption,
                        reply_to,
                    )
                elif clean_url:
                    await update.message.reply_text(
                        media_caption or clean_url,
                        reply_to_message_id=reply_to,
                        disable_web_page_preview=True,
                    )
            except Exception as e:
                logger.error("Failed to send media group: %r", e)
                if clean_url:
                    await update.message.reply_text(
                        media_caption or clean_url,
                        reply_to_message_id=reply_to,
                        disable_web_page_preview=True,
                    )
            finally:
                for file_path, _ in media_files:
                    if os.path.exists(file_path):
                        try:
                            os.remove(file_path)
                        except Exception as e:
                            logger.warning(f"Failed to delete temp file {file_path}: {e}")


async def _download_media(client, media_url: str) -> tuple[str, bool]:
    """Download a media URL to a temp file and return its path and type."""
    if not client:
        async with httpx.AsyncClient(timeout=HTTP_TIMEOUT) as temp_client:
            resp = await temp_client.get(media_url, follow_redirects=True)
    else:
        resp = await client.get(media_url, follow_redirects=True)

    resp.raise_for_status()
    content_type = resp.headers.get("Content-Type", "")
    is_video = _is_video_url(media_url, content_type)
    ext = ".mp4" if is_video else ".jpg"
    os.makedirs(TEMP_DIR, exist_ok=True)
    file_path = os.path.join(TEMP_DIR, f"{uuid.uuid4()}{ext}")

    with open(file_path, "wb") as f:
        f.write(resp.content)

    return file_path, is_video


async def _reply_with_media(
    message: Message,
    media_files: list[tuple[str, bool]],
    caption: str | None,
    reply_to: int,
) -> None:
    """Reply with uploaded media, grouping files into albums when possible."""
    if len(media_files) == 1:
        file_path, is_video = media_files[0]
        with open(file_path, "rb") as media_file:
            if is_video:
                await message.reply_video(
                    video=media_file,
                    caption=caption,
                    supports_streaming=True,
                    reply_to_message_id=reply_to,
                )
            else:
                await message.reply_photo(
                    photo=media_file,
                    caption=caption,
                    reply_to_message_id=reply_to,
                )
        return

    first_chunk = True
    for chunk_start in range(0, len(media_files), MEDIA_GROUP_LIMIT):
        chunk = media_files[chunk_start : chunk_start + MEDIA_GROUP_LIMIT]
        if len(chunk) == 1:
            await _reply_with_media(
                message,
                chunk,
                caption if first_chunk else None,
                reply_to,
            )
            first_chunk = False
            continue

        handles = []
        try:
            media_group = []
            for index, (file_path, is_video) in enumerate(chunk):
                media_file = open(file_path, "rb")
                handles.append(media_file)
                item_caption = caption if first_chunk and index == 0 else None
                if is_video:
                    media_group.append(
                        InputMediaVideo(
                            media=media_file,
                            caption=item_caption,
                            supports_streaming=True,
                        )
                    )
                else:
                    media_group.append(InputMediaPhoto(media=media_file, caption=item_caption))

            await message.reply_media_group(
                media=media_group,
                reply_to_message_id=reply_to,
            )
        finally:
            for media_file in handles:
                try:
                    media_file.close()
                except Exception as e:
                    logger.warning("Failed to close media file: %r", e)
        first_chunk = False


def _format_media_caption(caption_text: str | None, clean_url: str | None) -> str | None:
    """Build a Telegram media caption within the Bot API limit."""
    parts = [part for part in (caption_text, clean_url) if part]
    if not parts:
        return None
    caption = "\n\n".join(parts)
    if len(caption) <= MEDIA_CAPTION_LIMIT:
        return caption
    return caption[: MEDIA_CAPTION_LIMIT - 3].rstrip() + "..."


def _is_video_url(url: str, content_type: str = "") -> bool:
    """Detect videos from response metadata or the direct URL."""
    lowered_type = content_type.lower()
    if lowered_type.startswith("video/"):
        return True
    lowered_url = unquote(url).lower()
    return ".mp4" in lowered_url or "video" in lowered_url
