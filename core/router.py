"""Message routing and dispatch."""

import logging
from typing import Sequence

from telegram import Update
from telegram.ext import ContextTypes

from .types import HandlerResult, HandlerType, MessageHandler
from utils.text import strip_url_tracking

logger = logging.getLogger(__name__)


class MessageRouter:
    """Routes messages to appropriate handlers."""

    def __init__(self, handlers: Sequence[MessageHandler]):
        """
        Initialize router with handlers.

        Args:
            handlers: List of handler instances
        """
        self.handlers = handlers
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

    def _format_text_result(self, result: HandlerResult) -> str:
        """Format a link fixer result."""
        return str(result.content)

    def _format_media_result(
        self, result: HandlerResult, original_url: str | None
    ) -> str:
        """Format a media extractor result as HTML."""
        urls = result.content if isinstance(result.content, list) else [result.content]
        if not urls:
            return ""

        # Use first URL for preview
        media_url = urls[0]
        clean_url = strip_url_tracking(original_url) if original_url else media_url

        return f'<a href="{media_url}">\u200b</a><a href="{clean_url}">Source</a>'

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

        if result.type == HandlerType.LINK_FIXER:
            await update.message.reply_text(
                self._format_text_result(result),
                reply_to_message_id=reply_to,
            )

        elif result.type == HandlerType.MEDIA_EXTRACTOR:
            original_url = (
                result.metadata.get("original_url") if result.metadata else None
            )
            urls = (
                result.content if isinstance(result.content, list) else [result.content]
            )

            for media_url in urls:
                content = self._format_media_result(
                    HandlerResult(
                        type=HandlerType.MEDIA_EXTRACTOR,
                        content=media_url,
                        metadata=result.metadata,
                    ),
                    original_url,
                )
                await update.message.reply_html(
                    content,
                    reply_to_message_id=reply_to,
                )
