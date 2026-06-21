"""Media extractor handlers for complex media extraction."""

from .base import MediaExtractor
from .facebook import FacebookExtractor
from .instagram import InstagramExtractor
from .reddit import RedditExtractor


def build_media_extractors() -> list[MediaExtractor]:
    """Build media extractors in routing priority order."""
    return [
        FacebookExtractor(),
        InstagramExtractor(),
        RedditExtractor(),
    ]


__all__ = [
    "MediaExtractor",
    "FacebookExtractor",
    "InstagramExtractor",
    "RedditExtractor",
    "build_media_extractors",
]
