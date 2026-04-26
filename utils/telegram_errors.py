"""Telegram API error classifiers for expected chat-state races."""

from telegram.error import TelegramError


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
