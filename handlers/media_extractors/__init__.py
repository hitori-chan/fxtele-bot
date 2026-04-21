"""Media extractor handlers for complex media extraction."""

from .base import MediaExtractor
from .facebook import FacebookExtractor
from .instagram import InstagramExtractor

__all__ = [
    "MediaExtractor",
    "FacebookExtractor",
    "InstagramExtractor",
]
