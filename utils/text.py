from urllib.parse import parse_qs, urlencode, urlparse, urlunparse

from config import FACEBOOK_PARAMS_TO_KEEP


def strip_url_params(
    url: str,
    params_to_remove: set[str] | None = None,
    keep_only: set[str] | None = None,
) -> str:
    """Remove specific parameters from a URL or keep only specific ones."""
    parsed = urlparse(url)
    query_params = parse_qs(parsed.query)

    if keep_only is not None:
        query_params = {k: v for k, v in query_params.items() if k in keep_only}
    elif params_to_remove is not None:
        query_params = {k: v for k, v in query_params.items() if k not in params_to_remove}

    new_query = urlencode(query_params, doseq=True)
    return urlunparse(parsed._replace(query=new_query))


def safe_log_url(url: str) -> str:
    """Return a URL without its query string or fragment, so share-tracking ids stay out of logs."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return "<invalid-url>"
    return urlunparse(parsed._replace(query="", fragment=""))


def redact_url_credentials(url: str) -> str:
    """Drop any user:password@ from a URL so it is safe to log (e.g. an authenticated proxy)."""
    parsed = urlparse(url)
    if not parsed.username and not parsed.password:
        return url
    host = parsed.hostname or ""
    if ":" in host:
        host = f"[{host}]"
    netloc = f"{host}:{parsed.port}" if parsed.port else host
    return urlunparse(parsed._replace(netloc=netloc))


def strip_url_tracking(url: str) -> str:
    """Remove tracking parameters from URL, keeping only essential Facebook params."""
    return strip_url_params(url, keep_only=FACEBOOK_PARAMS_TO_KEEP)
