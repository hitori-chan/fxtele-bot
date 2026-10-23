"""Application logging setup."""

from __future__ import annotations

import logging
import os
import sys
from collections.abc import Iterator
from contextlib import contextmanager
from contextvars import ContextVar

from rich.console import Console
from rich.text import Text
from rich.traceback import Traceback

LEVEL_STYLES = {
    "DEBUG": "dim cyan",
    "INFO": "blue",
    "WARNING": "yellow",
    "ERROR": "bold red",
    "CRITICAL": "bold white on red",
}

LOG_LEVEL_ENV = "LOG_LEVEL"
DEFAULT_LEVEL = logging.INFO
# Third-party loggers that drown the bot's own lines at DEBUG: HTTP/2 framing and per-request chatter.
_QUIET_LOGGERS = ("httpx", "httpcore", "hpack", "h2", "asyncio")
# python-telegram-bot logs every Bot API call at DEBUG, parameters included, which means message text.
_TELEGRAM_LOGGER = "telegram"

_request_label: ContextVar[str | None] = ContextVar("request_label", default=None)


@contextmanager
def request_logging(label: str | None) -> Iterator[None]:
    """Tag every log record emitted inside the block, and by tasks it starts, with label.

    Lets the lines of one request (extract, download, upload, fallback) be
    followed through the interleaved output of concurrent requests.
    """
    token = _request_label.set(label)
    try:
        yield
    finally:
        _request_label.reset(token)


class RequestLabelFilter(logging.Filter):
    """Attach the current request label, if any, to each record as ``record.request``."""

    def filter(self, record: logging.LogRecord) -> bool:
        record.request = _request_label.get()
        return True


class CompactRichHandler(logging.Handler):
    """Rich-colored logging handler without RichHandler's padded table output."""

    def __init__(self, console: Console) -> None:
        super().__init__()
        self.console = console

    def emit(self, record: logging.LogRecord) -> None:
        try:
            self.console.print(self._line(record), soft_wrap=True)
            if record.exc_info:
                self.console.print(
                    Traceback.from_exception(
                        record.exc_info[0],
                        record.exc_info[1],
                        record.exc_info[2],
                        show_locals=False,
                    )
                )
        except Exception:
            self.handleError(record)

    def _line(self, record: logging.LogRecord) -> Text:
        timestamp = self.formatter.formatTime(record, "%Y-%m-%d %H:%M:%S") if self.formatter else ""
        level = record.levelname
        line = Text()
        line.append(timestamp, style="dim")
        line.append(" ")
        line.append(f"{level:<8}", style=LEVEL_STYLES.get(level, ""))
        line.append(" ")
        line.append(record.name, style="cyan")
        line.append(" ")
        if label := getattr(record, "request", None):
            line.append(f"[{label}]", style="magenta")
            line.append(" ")
        line.append(record.getMessage())
        return line


def resolve_level(value: str | int | None) -> tuple[int, bool]:
    """Turn a level name or number into a logging level; the flag is False when value was unusable."""
    if value is None or value == "":
        return DEFAULT_LEVEL, True
    if isinstance(value, int):
        return value, True
    level = logging.getLevelNamesMapping().get(value.strip().upper())
    return (level, True) if level is not None else (DEFAULT_LEVEL, False)


def setup_logging(level: str | int | None = None) -> None:
    """Configure colored Rich logs with stable timestamps.

    The level comes from the argument, else the LOG_LEVEL environment variable
    (DEBUG, INFO, WARNING, ERROR), else INFO.
    """
    requested = level if level is not None else os.environ.get(LOG_LEVEL_ENV)
    resolved, valid = resolve_level(requested)

    console = Console(file=sys.stdout, force_terminal=True)
    handler = CompactRichHandler(console)
    handler.setFormatter(logging.Formatter())
    handler.addFilter(RequestLabelFilter())

    logging.basicConfig(
        level=resolved,
        format="%(message)s",
        handlers=[handler],
        force=True,
    )
    for name in _QUIET_LOGGERS:
        logging.getLogger(name).setLevel(logging.WARNING)
    logging.getLogger(_TELEGRAM_LOGGER).setLevel(max(resolved, logging.INFO))

    if not valid:
        logging.getLogger("main").warning("Unknown %s %r; using %s.", LOG_LEVEL_ENV, requested, "INFO")
