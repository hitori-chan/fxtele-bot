"""Base class for media extraction handlers."""

import re
from abc import ABC, abstractmethod

from core.types import HandlerResult, HandlerType, MessageHandler


class MediaExtractor(ABC, MessageHandler):
    """
    Base class for media extraction handlers.

    Extracts direct media URLs from complex pages (requires HTTP requests).

    Attributes:
        name: Unique handler name
        handler_type: Always HandlerType.MEDIA_EXTRACTOR
        url_pattern: Regex pattern to match URLs
    """

    name: str = ""
    handler_type: HandlerType = HandlerType.MEDIA_EXTRACTOR
    url_pattern: re.Pattern = re.compile("")

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Extract media from text containing a matching URL.

        Args:
            text: Message text to process

        Returns:
            HandlerResult with media URLs and metadata, or None if no match
        """
        match = self.url_pattern.search(text)
        if not match:
            return None

        url = match.group(1)
        if not self._validate_url(url):
            return None

        result = await self._extract_media(url)
        return result

    @abstractmethod
    async def _extract_media(self, url: str) -> HandlerResult | None:
        """
        Extract media from the given URL.

        Args:
            url: The URL to extract media from

        Returns:
            HandlerResult with:
                - content: List of media URLs
                - metadata: Dict with 'original_url' and optionally 'thumbnail'
        """
        ...

    def _validate_url(self, url: str) -> bool:
        """
        Validate that URL is safe to request.

        Override for custom validation (domain checks, etc.)

        Args:
            url: URL to validate

        Returns:
            True if valid, False otherwise
        """
        return True
