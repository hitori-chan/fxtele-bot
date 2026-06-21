"""Saved Reddit session cookie access."""

import logging
import httpx

from config import REDDIT_COOKIE_PATH
from services.cookie_file import cookies_to_httpx, has_any_cookie_name, load_cookie_file

logger = logging.getLogger(__name__)

_REDDIT_AUTH_COOKIE_NAMES = {"reddit_session"}


async def get_reddit_cookies() -> httpx.Cookies:
    """Return cookies from the configured Reddit cookie file, or an empty jar."""
    if not REDDIT_COOKIE_PATH.exists():
        logger.info("No Reddit cookie file found; using public Reddit JSON.")
        return httpx.Cookies()

    cookies = load_cookie_file(REDDIT_COOKIE_PATH)
    if not has_any_cookie_name(cookies, _REDDIT_AUTH_COOKIE_NAMES):
        logger.info("Reddit cookie file has no session cookie; using public Reddit JSON.")
        return httpx.Cookies()

    logger.info("Using Reddit cookie file.")
    return cookies_to_httpx(cookies)
