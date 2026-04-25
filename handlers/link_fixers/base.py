"""Base class for simple URL replacement handlers."""

import re

from config import LinkFixerConfig
from core.types import HandlerResult, LinkFixResult, MessageHandler


class LinkFixer(MessageHandler):
    """Rule-backed URL replacement handler."""

    def __init__(self, config: LinkFixerConfig):
        self.name = config.name
        self.description = config.description
        self.pattern = re.compile(config.pattern)
        self.replacement = config.replacement

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Replace matching URLs with alternative.

        Args:
            text: Message text to process

        Returns:
            HandlerResult with fixed text, or None if no match
        """
        if not self.pattern.search(text):
            return None

        fixed = self.pattern.sub(self.replacement, text)

        if fixed == text:
            return None

        return LinkFixResult(content=fixed)
