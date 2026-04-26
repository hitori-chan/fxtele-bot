"""HTTP client management."""

import logging

import httpx

from config import HTTP_TIMEOUT

logger = logging.getLogger(__name__)

# Global HTTP Client
_HTTP_CLIENT: httpx.AsyncClient | None = None


async def init_http_client(app) -> None:
    """Initialize the global HTTP client on bot startup."""
    global _HTTP_CLIENT
    logger.debug("Initialized global HTTP client.")
    _HTTP_CLIENT = httpx.AsyncClient(
        follow_redirects=False,
        timeout=HTTP_TIMEOUT,
        http2=True,
    )


async def shutdown_http_client(app) -> None:
    """Cleanup the global HTTP client on bot shutdown."""
    global _HTTP_CLIENT
    if _HTTP_CLIENT:
        logger.debug("Closed global HTTP client.")
        await _HTTP_CLIENT.aclose()
        _HTTP_CLIENT = None


def get_client() -> httpx.AsyncClient | None:
    """Get the global HTTP client instance."""
    return _HTTP_CLIENT
