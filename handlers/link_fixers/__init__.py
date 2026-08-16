"""Link fixer handlers for URL replacement."""

from core.types import MessageHandler

from .base import LinkFixer
from .rules import LINK_FIXERS
from .threads import ThreadsLinkFixer


def build_link_fixers() -> list[MessageHandler]:
    """Build link fixers from hardcoded rules plus custom handlers."""
    return [ThreadsLinkFixer(), *(LinkFixer(rule) for rule in LINK_FIXERS)]


__all__ = [
    "LinkFixer",
    "ThreadsLinkFixer",
    "build_link_fixers",
]
