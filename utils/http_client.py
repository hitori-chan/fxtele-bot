import json
import logging
import os
import httpx
from config import FB_COOKIES_FILE, HTTP_TIMEOUT

logger = logging.getLogger(__name__)

# Global Cookie Cache
_cookie_cache: dict = {"mtime": 0.0, "jar": httpx.Cookies()}
# Global HTTP Client
HTTP_CLIENT: httpx.AsyncClient | None = None


def get_fb_cookies() -> httpx.Cookies:
    """Load Facebook cookies with caching based on file modification time."""
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


async def init_http_client(app) -> None:
    """Initialize resources on bot startup."""
    global HTTP_CLIENT
    logger.info("Initializing global HTTP client...")
    HTTP_CLIENT = httpx.AsyncClient(
        follow_redirects=False,
        timeout=HTTP_TIMEOUT,
        http2=True,
    )


async def shutdown_http_client(app) -> None:
    """Cleanup resources on bot shutdown."""
    global HTTP_CLIENT
    if HTTP_CLIENT:
        logger.info("Closing global HTTP client...")
        await HTTP_CLIENT.aclose()
        HTTP_CLIENT = None


def get_client() -> httpx.AsyncClient | None:
    return HTTP_CLIENT
