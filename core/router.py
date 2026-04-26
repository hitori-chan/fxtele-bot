"""Message routing."""

import asyncio
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
        logger.debug("Router initialized with %d handlers.", len(handlers))

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Process text through all handlers concurrently until one matches.

        Args:
            text: Message text to process

        Returns:
            First matching result in configured handler order, or None if no handler matched
        """
        tasks = [asyncio.create_task(self._handle_one(handler, text)) for handler in self.handlers]
        try:
            for handler, task in zip(self.handlers, tasks, strict=True):
                result = await task
                if result is not None:
                    logger.debug("Matched handler %s.", handler.name)
                    self._cancel_pending(tasks)
                    return result
            return None
        except asyncio.CancelledError:
            self._cancel_pending(tasks)
            raise
        finally:
            await self._finish_tasks(tasks)

    async def _handle_one(self, handler: MessageHandler, text: str) -> HandlerResult | None:
        try:
            return await handler.handle(text)
        except Exception as e:
            logger.error("Handler %s failed with %s.", handler.name, type(e).__name__)
            return None

    def _cancel_pending(self, tasks: Sequence[asyncio.Task[HandlerResult | None]]) -> None:
        for task in tasks:
            if not task.done():
                task.cancel()

    async def _finish_tasks(self, tasks: Sequence[asyncio.Task[HandlerResult | None]]) -> None:
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
