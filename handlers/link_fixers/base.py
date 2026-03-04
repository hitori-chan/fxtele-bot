"""Base class for simple URL replacement handlers."""

import re
from abc import ABC

from core.types import HandlerResult, HandlerType, MessageHandler


class LinkFixer(ABC, MessageHandler):
    """
    Base class for link fixing handlers.

    Simply replaces matching URLs with alternative service URLs.

    Attributes:
        name: Unique handler name
        handler_type: Always HandlerType.LINK_FIXER
        pattern: Regex pattern to match URLs
        replacement: Replacement URL template
    """

    name: str = ""
    handler_type: HandlerType = HandlerType.LINK_FIXER
    pattern: re.Pattern = re.compile("")
    replacement: str = ""

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

        return HandlerResult(
            type=HandlerType.LINK_FIXER,
            content=fixed,
        )
