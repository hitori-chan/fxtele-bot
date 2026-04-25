"""Message routing and dispatch."""

import logging
import os
import uuid
import httpx
from typing import Sequence

from telegram import Update
from telegram.ext import ContextTypes
from telegram.constants import ParseMode

from .types import HandlerResult, HandlerType, MessageHandler
from services.http import get_client
from config import HTTP_TIMEOUT
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)

TEMP_DIR = "/tmp/fx-telebot/"


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

    async def handle_telegram_message(
        self, update: Update, context: ContextTypes.DEFAULT_TYPE
    ) -> None:
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
            original_url = (
                result.metadata.get("original_url") if result.metadata else None
            )
            caption_text = (
                result.metadata.get("caption") if result.metadata else None
            )
            urls = result.content if isinstance(result.content, list) else [result.content]

            clean_url = strip_url_tracking(original_url) if original_url else None
            
            # 1. Send the fixed source link first (as a 'Source' HTML link)
            if clean_url:
                msg_html = f'<a href="{clean_url}">Source</a>'
                if caption_text:
                    msg_html = f"{caption_text}\n\n{msg_html}"
                
                await update.message.reply_html(
                    msg_html,
                    reply_to_message_id=reply_to,
                    disable_web_page_preview=True # Disable preview for the text link as we send native media
                )

            # 2. Download to local disk and Push each media natively
            norm_original = original_url.rstrip("/") if original_url else ""
            
            for media_url in urls:
                norm_media = media_url.rstrip("/")
                if norm_media == norm_original:
                    continue

                # Generate unique filename
                is_video = ".mp4" in media_url or "rapidcdn.app/v2" in media_url
                ext = ".mp4" if is_video else ".jpg"
                file_path = os.path.join(TEMP_DIR, f"{uuid.uuid4()}{ext}")

                try:
                    # Download the media file
                    if not client:
                        async with httpx.AsyncClient(timeout=HTTP_TIMEOUT) as temp_client:
                            resp = await temp_client.get(media_url, follow_redirects=True)
                    else:
                        resp = await client.get(media_url, follow_redirects=True)
                    
                    resp.raise_for_status()
                    
                    with open(file_path, "wb") as f:
                        f.write(resp.content)
                    
                    # Upload to Telegram using file path
                    with open(file_path, "rb") as media_file:
                        if is_video:
                            await update.message.reply_video(
                                video=media_file,
                                reply_to_message_id=reply_to
                            )
                        else:
                            await update.message.reply_photo(
                                photo=media_file,
                                reply_to_message_id=reply_to
                            )
                except Exception as e:
                    logger.error(f"Failed to download/push media {media_url}: {e}")
                    # Fallback to plain link if file operation fails
                    await update.message.reply_text(media_url, reply_to_message_id=reply_to)
                finally:
                    # Always clean up the file
                    if os.path.exists(file_path):
                        try:
                            os.remove(file_path)
                        except Exception as e:
                            logger.warning(f"Failed to delete temp file {file_path}: {e}")
