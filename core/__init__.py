"""Core abstractions for fxtele-bot."""

from .types import HandlerResult, HandlerType, MessageHandler
from .registry import register_handler, discover_handlers
from .router import MessageRouter

__all__ = [
    "HandlerResult",
    "HandlerType",
    "MessageHandler",
    "register_handler",
    "discover_handlers",
    "MessageRouter",
]
