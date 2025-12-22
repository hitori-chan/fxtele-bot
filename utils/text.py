import json
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse
from config import FACEBOOK_PARAMS_TO_KEEP


def decode_json_string(escaped_str: str) -> str | None:
    """Decode a JSON-escaped string."""
    try:
        return json.loads(f'"{escaped_str}"')
    except json.JSONDecodeError:
        return None


def strip_url_params(url: str, params_to_remove: set[str] | None = None, keep_only: set[str] | None = None) -> str:
    """Remove specific parameters from a URL or keep only specific ones."""
    parsed = urlparse(url)
    query_params = parse_qs(parsed.query)

    if keep_only is not None:
        query_params = {k: v for k, v in query_params.items() if k in keep_only}
    elif params_to_remove is not None:
        query_params = {k: v for k, v in query_params.items() if k not in params_to_remove}

    new_query = urlencode(query_params, doseq=True)
    return urlunparse(parsed._replace(query=new_query))


def strip_url_tracking(url: str) -> str:
    """Remove tracking parameters from URL, keeping only essential Facebook params."""
    return strip_url_params(url, keep_only=FACEBOOK_PARAMS_TO_KEEP)
