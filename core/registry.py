"""Explicit handler factory."""

from .types import MessageHandler


def build_handlers() -> list[MessageHandler]:
    """Build handlers in the fixed routing order."""
    from handlers.link_fixers import build_link_fixers
    from handlers.media_extractors import build_media_extractors

    return [
        *build_media_extractors(),
        *build_link_fixers(),
    ]
