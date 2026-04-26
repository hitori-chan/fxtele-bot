"""Base class for simple URL replacement handlers."""

import re

from core.types import HandlerResult, LinkFixResult, MessageHandler
from handlers.link_fixers.rules import LinkFixerRule


class LinkFixer(MessageHandler):
    """Rule-backed URL replacement handler."""

    def __init__(self, rule: LinkFixerRule):
        self.name = rule.name
        self.description = rule.description
        self.pattern = re.compile(rule.pattern)
        self.replacement = rule.replacement

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
