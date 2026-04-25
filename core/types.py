from dataclasses import dataclass
from typing import Protocol, TypeAlias, runtime_checkable


@dataclass(frozen=True)
class LinkFixResult:
    """Text result from a URL replacement handler."""

    content: str


@dataclass(frozen=True)
class MediaMetadata:
    """Metadata associated with extracted media URLs."""

    original_url: str
    thumbnail: str | None = None
    caption: str | None = None
    title: str | None = None


@dataclass(frozen=True)
class MediaResult:
    """Direct media URLs extracted from a source page."""

    urls: tuple[str, ...]
    metadata: MediaMetadata


HandlerResult: TypeAlias = LinkFixResult | MediaResult


@runtime_checkable
class MessageHandler(Protocol):
    """Protocol for message handlers."""

    name: str

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Process text and return result if handled.

        Args:
            text: The message text to process

        Returns:
            HandlerResult if handled, None if not applicable
        """
        ...
