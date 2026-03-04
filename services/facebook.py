"""Facebook-specific service utilities."""

import json
import logging
import os

import httpx

from config import FB_COOKIES_FILE

logger = logging.getLogger(__name__)

# Cookie cache: {mtime, jar}
_cookie_cache: dict = {"mtime": 0.0, "jar": httpx.Cookies()}


def get_fb_cookies() -> httpx.Cookies:
    """
    Load Facebook cookies with caching based on file modification time.

    Returns:
        httpx.Cookies instance (empty if no cookie file)
    """
    global _cookie_cache

    if not os.path.exists(FB_COOKIES_FILE):
        return httpx.Cookies()

    try:
        current_mtime = os.path.getmtime(FB_COOKIES_FILE)
        if current_mtime > _cookie_cache["mtime"]:
            logger.info("Reloading Facebook cookies...")
            jar = httpx.Cookies()
            with open(FB_COOKIES_FILE, "r") as f:
                cookies_data = json.load(f)
                for c in cookies_data:
                    if "name" in c and "value" in c:
                        jar.set(
                            name=c["name"],
                            value=c["value"],
                            domain=c.get("domain", ".facebook.com"),
                            path=c.get("path", "/"),
                        )
            _cookie_cache = {"mtime": current_mtime, "jar": jar}

        return _cookie_cache["jar"]
    except Exception as e:
        logger.error(f"Error loading cookies from {FB_COOKIES_FILE}: {e}")
        return httpx.Cookies()
