"""Telegram application error handling."""

import logging

from telegram.error import BadRequest, Conflict, NetworkError
from telegram.ext import ContextTypes

from utils.error_text import describe_error

logger = logging.getLogger(__name__)


async def handle_error(update: object, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Log expected polling conflicts cleanly and unexpected errors with tracebacks."""
    error = context.error
    if isinstance(error, Conflict):
        logger.error("Telegram polling conflict: another bot instance is using this token.")
        return
    # BadRequest subclasses NetworkError but means a rejected request, which is a real bug.
    if update is None and isinstance(error, NetworkError) and not isinstance(error, BadRequest):
        # Raised by the getUpdates loop, which retries on its own, so nothing was lost.
        logger.warning("Telegram polling network error: %s; retrying.", describe_error(error))
        return
    exc_info = (type(error), error, error.__traceback__) if error else None
    logger.exception("Unhandled Telegram update error: %s.", describe_error(error), exc_info=exc_info)
