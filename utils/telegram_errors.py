"""Telegram API error classifiers for expected chat-state races."""

from telegram.error import TelegramError


def describe_error(error: object) -> str:
    """Name an error for logs, adding the transport error behind it (e.g. WriteTimeout).

    python-telegram-bot raises a bare TimedOut for every httpx timeout; the
    chained cause is the only place that says whether connecting, uploading or
    waiting for the reply timed out. Only class names are used so URLs and
    tokens in error text never reach the logs.
    """
    name = type(error).__name__
    cause = getattr(error, "__cause__", None)
    return f"{name} ({type(cause).__name__})" if cause else name


def bot_absent_from_chat(error: TelegramError) -> bool:
    message = str(error).lower()
    return any(
        marker in message
        for marker in (
            "bot was kicked",
            "bot is not a member",
            "chat not found",
        )
    )
