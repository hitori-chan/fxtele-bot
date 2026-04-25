"""External service integrations."""

from .http import get_client, init_http_client, shutdown_http_client
from .facebook_auth import facebook_auth_available, get_facebook_cookies

__all__ = [
    "get_client",
    "init_http_client",
    "shutdown_http_client",
    "facebook_auth_available",
    "get_facebook_cookies",
]
