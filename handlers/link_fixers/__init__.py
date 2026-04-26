"""Link fixer handlers for URL replacement."""

from .base import LinkFixer
from .rules import LINK_FIXERS


def build_link_fixers() -> list[LinkFixer]:
    """Build link fixers from hardcoded rules."""
    return [LinkFixer(rule) for rule in LINK_FIXERS]


__all__ = [
    "LinkFixer",
    "build_link_fixers",
]
