"""Centralized configuration settings."""

from __future__ import annotations

import os
from pathlib import Path
import tomllib
from typing import Any

CONFIG_PATH = Path(__file__).resolve().parent.parent / "config.toml"
DEFAULT_TELEGRAM_MAX_MEDIA_BYTES = 50 * 1024 * 1024
DEFAULT_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"
)


class ConfigError(ValueError):
    """Raised when config.toml is missing or malformed."""


def _load_config(path: Path = CONFIG_PATH) -> dict[str, Any]:
    if not path.exists():
        raise ConfigError(f"{path} not found; create it from the repository config.toml template")
    try:
        with path.open("rb") as config_file:
            data = tomllib.load(config_file)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{path} is not valid TOML: {exc}") from exc
    if not isinstance(data, dict):
        raise ConfigError(f"{path} root must be a TOML table")
    return data


def _section(config: dict[str, Any], name: str) -> dict[str, Any]:
    value = config.get(name, {})
    if not isinstance(value, dict):
        raise ConfigError(f"[{name}] must be a TOML table")
    return value


def _string(section: dict[str, Any], key: str, *, default: str | None = None) -> str:
    value = section.get(key, default)
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{key} must be a non-empty string")
    return value


def _number(section: dict[str, Any], key: str, *, default: int | float) -> int | float:
    value = section.get(key, default)
    if not isinstance(value, int | float) or isinstance(value, bool):
        raise ConfigError(f"{key} must be a number")
    return value


def _positive_int(section: dict[str, Any], key: str, *, default: int) -> int:
    value = _number(section, key, default=default)
    if not isinstance(value, int) or value <= 0:
        raise ConfigError(f"{key} must be a positive integer")
    return value


def _int(section: dict[str, Any], key: str, *, default: int = 0) -> int:
    value = section.get(key, default)
    if not isinstance(value, int) or isinstance(value, bool):
        raise ConfigError(f"{key} must be an integer")
    return value


def _id_set(section: dict[str, Any], key: str) -> set[int]:
    value = section.get(key, [])
    if not isinstance(value, list):
        raise ConfigError(f"{key} must be a list of integer IDs")
    ids: set[int] = set()
    for item in value:
        if not isinstance(item, int) or isinstance(item, bool):
            raise ConfigError(f"{key} contains a non-integer ID: {item!r}")
        ids.add(item)
    return ids


def _negative_id_set(section: dict[str, Any], key: str) -> set[int]:
    ids = _id_set(section, key)
    invalid = [item for item in ids if item >= 0]
    if invalid:
        raise ConfigError(f"{key} must contain only negative group chat IDs: {invalid!r}")
    return ids


_CONFIG = _load_config()
_HTTP = _section(_CONFIG, "http")
_TELEGRAM = _section(_CONFIG, "telegram")
_FACEBOOK = _section(_CONFIG, "facebook")

# HTTP Configuration
HTTP_TIMEOUT = float(_number(_HTTP, "timeout", default=10.0))
USER_AGENT = _string(_HTTP, "user_agent", default=DEFAULT_USER_AGENT)
INLINE_CACHE_TIME = _positive_int(_TELEGRAM, "inline_cache_time", default=300)

# Telegram access control
TELEGRAM_OWNER_ID = _int(_TELEGRAM, "owner_id")
TELEGRAM_ACCESS_STATE_PATH = Path(_string(_TELEGRAM, "access_state_path", default="/app/data/access_control.json"))
TELEGRAM_ALLOWED_USER_IDS = _id_set(_TELEGRAM, "allowed_user_ids")
TELEGRAM_ALLOWED_CHAT_IDS = _negative_id_set(_TELEGRAM, "allowed_chat_ids")
TELEGRAM_MAX_MEDIA_BYTES = _positive_int(
    _TELEGRAM,
    "max_media_bytes",
    default=DEFAULT_TELEGRAM_MAX_MEDIA_BYTES,
)

# Facebook Request Headers
FACEBOOK_HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.5",
    "DNT": "1",
    "Sec-GPC": "1",
    "Upgrade-Insecure-Requests": "1",
    "Sec-Fetch-Dest": "document",
    "Sec-Fetch-Mode": "navigate",
    "Sec-Fetch-Site": "none",
    "Sec-Fetch-User": "?1",
    "Priority": "u=0, i",
}

# Facebook Auth
FACEBOOK_EMAIL = os.getenv("FACEBOOK_EMAIL")
FACEBOOK_PASSWORD = os.getenv("FACEBOOK_PASSWORD")
FACEBOOK_TOTP_SECRET = os.getenv("FACEBOOK_TOTP_SECRET")
FACEBOOK_AUTH_STATE_PATH = _string(_FACEBOOK, "auth_state_path", default="/app/data/facebook_state.json")
