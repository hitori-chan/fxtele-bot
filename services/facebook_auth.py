"""Saved Facebook cookie access."""

import logging

import httpx

from config import FACEBOOK_COOKIE_PATH
from services.cookie_file import cookies_to_httpx, has_cookie_names, load_cookie_file

logger = logging.getLogger(__name__)

_FACEBOOK_SESSION_COOKIE_NAMES = {"c_user", "xs"}


def facebook_auth_available() -> bool:
    """Return true when the Facebook cookie file contains session cookies."""
    return FACEBOOK_COOKIE_PATH.exists() and _has_session_cookies(load_cookie_file(FACEBOOK_COOKIE_PATH))


async def get_facebook_cookies() -> httpx.Cookies:
    """Return cookies from the configured Facebook cookie file, or an empty jar."""
    if not FACEBOOK_COOKIE_PATH.exists():
        logger.info("No Facebook cookie file found; using public extraction.")
        return httpx.Cookies()

    cookies = load_cookie_file(FACEBOOK_COOKIE_PATH)
    if not _has_session_cookies(cookies):
        logger.info("Facebook cookie file has no session cookies; using public extraction.")
        return httpx.Cookies()

    logger.info("Using Facebook cookie file.")
    return cookies_to_httpx(cookies)


def _has_session_cookies(cookies: list[dict]) -> bool:
    return has_cookie_names(cookies, _FACEBOOK_SESSION_COOKIE_NAMES)
