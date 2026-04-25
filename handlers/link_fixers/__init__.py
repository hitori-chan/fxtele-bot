"""Link fixer handlers for URL replacement."""

from config import LINK_FIXERS

from .base import LinkFixer


def build_link_fixers() -> list[LinkFixer]:
    """Build link fixers from centralized rules."""
    return [LinkFixer(config) for config in LINK_FIXERS]

__all__ = [
    "LinkFixer",
    "build_link_fixers",
]
