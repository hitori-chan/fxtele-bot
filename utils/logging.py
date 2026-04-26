"""Application logging setup."""

from __future__ import annotations

import logging
import sys

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
        line.append(record.getMessage())
        return line


def setup_logging(level: int = logging.INFO) -> None:
    """Configure colored Rich logs with stable timestamps."""
    console = Console(file=sys.stdout, force_terminal=True)
    handler = CompactRichHandler(console)
    handler.setFormatter(logging.Formatter())

    logging.basicConfig(
        level=level,
        format="%(message)s",
        handlers=[handler],
        force=True,
    )
    logging.getLogger("httpx").setLevel(logging.WARNING)
    logging.getLogger("httpcore").setLevel(logging.WARNING)
