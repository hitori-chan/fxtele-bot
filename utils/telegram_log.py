"""Small Telegram object formatters for logs."""

from __future__ import annotations

from typing import Any


def user_label(user: Any | None) -> str:
    if not user:
        return "unknown user"
    username = getattr(user, "username", None)
    if username:
        name = f"@{username}"
    elif full_name := _full_name(user):
        name = repr(full_name)
    else:
        name = "<no-username>"
    return f"{name} ({user.id})"


def chat_label(chat: Any | None) -> str:
    if not chat:
        return "unknown chat"
    title = getattr(chat, "title", None) or getattr(chat, "username", None) or chat.type
    return f"{title!r} ({chat.id}, {chat.type})"


def user_state_label(user: Any | None) -> str | None:
    if not user:
        return None
    return _full_name(user)


def user_username(user: Any | None) -> str | None:
    username = getattr(user, "username", None) if user else None
    return username or None


def chat_state_label(chat: Any | None) -> str | None:
    if not chat:
        return None
    title = getattr(chat, "title", None)
    if title:
        return title
    if username := chat_username(chat):
        return f"@{username}"
    return getattr(chat, "type", None)


def chat_username(chat: Any | None) -> str | None:
    username = getattr(chat, "username", None) if chat else None
    return username or None


def _full_name(user: Any | None) -> str | None:
    if not user:
        return None
    name = " ".join(part for part in (getattr(user, "first_name", None), getattr(user, "last_name", None)) if part)
    return name or None
