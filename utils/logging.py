"""Application logging setup."""

from __future__ import annotations

import logging
import sys

from rich.console import Console
from rich.logging import RichHandler

LOG_CONSOLE_WIDTH = 10_000


def setup_logging(level: int = logging.INFO) -> None:
    """Configure colored Rich logs with stable timestamps."""
    console = Console(file=sys.stdout, force_terminal=True, width=LOG_CONSOLE_WIDTH)
    handler = RichHandler(
        console=console,
        log_time_format="%Y-%m-%d %H:%M:%S",
        markup=False,
        omit_repeated_times=False,
        rich_tracebacks=True,
        show_path=False,
    )
    handler.setFormatter(logging.Formatter("%(name)s %(message)s"))

    logging.basicConfig(
        level=level,
        format="%(message)s",
        handlers=[handler],
        force=True,
    )
    logging.getLogger("httpx").setLevel(logging.WARNING)
    logging.getLogger("httpcore").setLevel(logging.WARNING)
