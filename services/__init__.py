"""External service integrations."""

from .http import get_client, init_http_client, shutdown_http_client
from .facebook import get_fb_cookies

__all__ = [
    "get_client",
    "init_http_client",
    "shutdown_http_client",
    "get_fb_cookies",
]
