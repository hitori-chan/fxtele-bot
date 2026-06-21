"""Load browser cookie export files for HTTP requests."""

from __future__ import annotations

import json
from pathlib import Path
import time
from typing import Any

import httpx


def load_cookie_file(path: Path) -> list[dict[str, Any]]:
    """Load non-expired cookies from a raw browser-exported JSON list."""
    try:
        with path.open("r", encoding="utf-8") as cookie_file:
            data = json.load(cookie_file)
    except (OSError, json.JSONDecodeError):
        return []
    return _normalized_cookies(data)


def cookies_to_httpx(cookies: list[dict[str, Any]]) -> httpx.Cookies:
    """Convert normalized cookie dictionaries to an httpx cookie jar."""
    jar = httpx.Cookies()
    for cookie in cookies:
        name = cookie.get("name")
        value = cookie.get("value")
        domain = cookie.get("domain")
        path = cookie.get("path", "/")
        if isinstance(name, str) and isinstance(value, str):
            jar.set(name, value, domain=domain, path=path)
    return jar


def has_cookie_names(cookies: list[dict[str, Any]], names: set[str]) -> bool:
    """Return true when the cookie list contains all requested names."""
    return names.issubset(_cookie_names(cookies))


def has_any_cookie_name(cookies: list[dict[str, Any]], names: set[str]) -> bool:
    """Return true when the cookie list contains at least one requested name."""
    return bool(_cookie_names(cookies) & names)


def _normalized_cookies(data: Any) -> list[dict[str, Any]]:
    if isinstance(data, list):
        return _normalized_cookie_list(data)
    return []


def _normalized_cookie_list(cookies: Any) -> list[dict[str, Any]]:
    if not isinstance(cookies, list):
        return []
    normalized = [_normalize_cookie(cookie) for cookie in cookies if isinstance(cookie, dict)]
    return [cookie for cookie in normalized if not _is_expired(cookie)]


def _normalize_cookie(cookie: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(cookie)
    if "expires" not in normalized and "expirationDate" in normalized:
        normalized["expires"] = normalized["expirationDate"]
    if "domain" not in normalized and isinstance(normalized.get("host"), str):
        normalized["domain"] = normalized["host"]
    normalized.setdefault("path", "/")
    return normalized


def _cookie_names(cookies: list[dict[str, Any]]) -> set[str]:
    return {cookie.get("name") for cookie in cookies if isinstance(cookie.get("name"), str)}


def _is_expired(cookie: dict[str, Any]) -> bool:
    expires = cookie.get("expires")
    if expires is None or isinstance(expires, bool) or not isinstance(expires, int | float):
        return False
    return expires > 0 and expires < time.time()
