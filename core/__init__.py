"""Core abstractions for fxtele-bot."""

from .registry import build_handlers
from .router import MessageRouter
from .types import HandlerResult, LinkFixResult, MediaMetadata, MediaResult, MessageHandler, TextResult

__all__ = [
    "HandlerResult",
    "LinkFixResult",
    "MediaMetadata",
    "MediaResult",
    "MessageHandler",
    "TextResult",
    "build_handlers",
    "MessageRouter",
]
