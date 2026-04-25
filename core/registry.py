"""Explicit handler factory."""

from .types import MessageHandler


def build_handlers() -> list[MessageHandler]:
    """Build handlers in the fixed routing order."""
    from handlers.link_fixers import build_link_fixers
    from handlers.media_extractors.facebook import FacebookExtractor
    from handlers.media_extractors.instagram import InstagramExtractor

    return [
        FacebookExtractor(),
        InstagramExtractor(),
        *build_link_fixers(),
    ]
