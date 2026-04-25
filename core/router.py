"""Message routing."""

import logging
from typing import Sequence

from .types import HandlerResult, MessageHandler

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
