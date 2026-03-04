"""Media extractor handlers for complex media extraction."""

from .base import MediaExtractor
from .facebook import FacebookExtractor

__all__ = [
    "MediaExtractor",
    "FacebookExtractor",
]
