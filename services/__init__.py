"""External service integrations."""

from .http import get_client, init_http_client, shutdown_http_client

__all__ = [
    "get_client",
    "init_http_client",
    "shutdown_http_client",
]
