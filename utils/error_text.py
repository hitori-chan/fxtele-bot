"""Compact, secret-free descriptions of exceptions for log lines."""

import errno

import httpx


def describe_error(error: object) -> str:
    """Name an error for logs: its class plus the failure behind it, never its message.

    Examples: ``HTTPStatusError (403)``, ``OSError/ENOSPC``, ``TimedOut
    (WriteTimeout)`` and ``ConnectError (ConnectionRefusedError/ECONNREFUSED)``.
    python-telegram-bot
    raises a bare TimedOut for every httpx timeout, and httpx wraps the OS
    error, so the chained causes are where the actual reason lives. Error text
    is left out because URLs and tokens appear in it.
    """
    name = _label(error)
    if isinstance(error, httpx.HTTPStatusError):
        return f"{name} ({error.response.status_code})"
    causes = _cause_names(error)
    return f"{name} ({' > '.join(causes)})" if causes else name


def _label(error: object) -> str:
    """Class name, plus the errno name for OS errors (OSError/ENOSPC says what a bare OSError does not)."""
    name = type(error).__name__
    if isinstance(error, OSError) and error.errno in errno.errorcode:
        return f"{name}/{errno.errorcode[error.errno]}"
    return name


def _cause_names(error: object) -> list[str]:
    """Names along the __cause__ chain, skipping repeats (httpx and httpcore share class names) and bare timeouts."""
    names: list[str] = []
    previous = type(error).__name__
    seen = {id(error)}
    current = getattr(error, "__cause__", None)
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        base = type(current).__name__
        name = _label(current)
        bare_timeout = isinstance(current, TimeoutError) and name == base
        if base != previous and not bare_timeout:
            names.append(name)
        previous = base
        current = current.__cause__
    return names
