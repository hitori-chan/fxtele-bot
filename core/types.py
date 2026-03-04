"""Core type definitions for handlers."""

from enum import Enum, auto
from dataclasses import dataclass
from typing import Protocol, runtime_checkable


class HandlerType(Enum):
    """Types of message handlers."""

    LINK_FIXER = auto()  # Simple URL replacement
    MEDIA_EXTRACTOR = auto()  # Download/extract media URLs
    COMMAND = auto()  # Bot command


@dataclass(frozen=True)
class HandlerResult:
    """Result from a handler processing a message."""

    type: HandlerType
    content: str | list[str]
    metadata: dict | None = None


@runtime_checkable
class MessageHandler(Protocol):
    """Protocol for message handlers."""

    name: str
    handler_type: HandlerType

    async def handle(self, text: str) -> HandlerResult | None:
        """
        Process text and return result if handled.

        Args:
            text: The message text to process

        Returns:
            HandlerResult if handled, None if not applicable
        """
        ...
