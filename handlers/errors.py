"""Telegram application error handling."""

import logging

from telegram.error import Conflict
from telegram.ext import ContextTypes

logger = logging.getLogger(__name__)


async def handle_error(update: object, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Log expected polling conflicts cleanly and unexpected errors with tracebacks."""
    error = context.error
    if isinstance(error, Conflict):
        logger.error("Telegram polling conflict: another bot instance is using this token.")
        return
    exc_info = (type(error), error, error.__traceback__) if error else None
    logger.exception("Unhandled Telegram update error: %s.", type(error).__name__, exc_info=exc_info)
