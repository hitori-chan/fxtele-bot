"""Small Telegram object formatters for logs."""

from __future__ import annotations

from typing import Any


def user_label(user: Any | None) -> str:
    if not user:
        return "unknown user"
    username = f"@{user.username}" if getattr(user, "username", None) else "<no-username>"
    return f"{username} ({user.id})"


def chat_label(chat: Any | None) -> str:
    if not chat:
        return "unknown chat"
    title = getattr(chat, "title", None) or getattr(chat, "username", None) or chat.type
    return f"{title!r} ({chat.id}, {chat.type})"
