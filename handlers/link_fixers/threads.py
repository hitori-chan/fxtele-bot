"""Threads link fixer with share-link resolution."""

import logging
import re
from urllib.parse import urlparse, urlunparse

import httpx

from config import HTTP_TIMEOUT, THREADS_HEADERS
from core.types import HandlerResult, LinkFixResult, MessageHandler
from services.http import get_client
from utils.error_text import describe_error

logger = logging.getLogger(__name__)

RE_THREADS = re.compile(r"(https?://(?:www\.)?threads\.com/\S+)")
_MAX_REDIRECTS = 10


def _is_threads_domain(url: str) -> bool:
    """Check if the URL points to a Threads host."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return False

    if parsed.scheme not in ("http", "https"):
        return False

    hostname = parsed.hostname
    if not hostname:
        return False

    hostname = hostname.lower()
    return hostname == "threads.com" or hostname.endswith(".threads.com")


def _is_share_link(url: str) -> bool:
    """Return true for /share/... links that redirect to a canonical post."""
    parts = [part for part in urlparse(url).path.split("/") if part]
    return bool(parts) and parts[0] == "share"


def _rewrite_domain(url: str, *, strip_query: bool = False) -> str:
    """Swap the Threads host for the fzthreads.com embed host."""
    parsed = urlparse(url)
    replacements = {"scheme": "https", "netloc": "fzthreads.com"}
    if strip_query:
        replacements["query"] = ""
        replacements["fragment"] = ""
    return urlunparse(parsed._replace(**replacements))


def _safe_log_url(url: str) -> str:
    """Return a URL safe enough for logs without query/fragment noise."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return "<invalid-url>"
    return urlunparse(parsed._replace(query="", fragment=""))


async def _resolve_share_link(client: httpx.AsyncClient, url: str) -> str:
    """Resolve a /share/... link to its canonical /@user/post/{id} URL.

    Threads share links carry an opaque token rather than the post path, so
    they must be followed through Threads' redirect chain to discover the
    canonical URL before it can be rewritten to fzthreads.com. The redirect
    target carries a single-use `xmt` token: fetching it again (to keep
    following redirects) invalidates it and Threads bounces to an error
    page, so resolution stops as soon as a non-share URL is reached instead
    of requesting it.
    """
    current_url = url
    for redirect_count in range(_MAX_REDIRECTS + 1):
        if not _is_threads_domain(current_url):
            logger.warning(
                "Aborting Threads share resolution at non-Threads domain: %s.", _safe_log_url(current_url)
            )
            return url

        try:
            response = await client.get(current_url, headers=THREADS_HEADERS, timeout=HTTP_TIMEOUT)
        except Exception as e:
            logger.warning("Threads share link resolution failed for %s: %s.", _safe_log_url(url), describe_error(e))
            return url

        if not response.is_redirect:
            resolved = str(response.url)
            return resolved if not _is_share_link(resolved) else url

        if redirect_count == _MAX_REDIRECTS:
            logger.warning("Too many redirects resolving Threads share link %s.", _safe_log_url(url))
            return url

        location = response.headers.get("Location")
        if not location:
            logger.warning("Threads redirect without Location for %s.", _safe_log_url(current_url))
            return url
        current_url = str(response.url.join(location))
        if _is_threads_domain(current_url) and not _is_share_link(current_url):
            return current_url

    return url


class ThreadsLinkFixer(MessageHandler):
    """Resolve Threads share links and rewrite Threads URLs to fzthreads.com."""

    name = "threads"

    async def handle(self, text: str) -> HandlerResult | None:
        """Replace the first Threads URL in text with its fzthreads.com equivalent."""
        match = RE_THREADS.search(text)
        if not match:
            return None

        url = match.group(1).rstrip(".,!?;)")
        if not _is_threads_domain(url):
            return None

        is_share_link = _is_share_link(url)
        target_url = await self._resolve(url) if is_share_link else url
        fixed_url = _rewrite_domain(target_url, strip_query=is_share_link and target_url != url)

        match_end = match.start(1) + len(url)
        fixed_text = f"{text[: match.start(1)]}{fixed_url}{text[match_end:]}"
        if fixed_text == text:
            return None
        return LinkFixResult(content=fixed_text)

    async def _resolve(self, url: str) -> str:
        """Resolve a Threads share link using the shared or a temporary client."""
        client = get_client()
        if client:
            return await _resolve_share_link(client, url)
        async with httpx.AsyncClient(follow_redirects=False, timeout=HTTP_TIMEOUT, http2=True) as temp_client:
            return await _resolve_share_link(temp_client, url)
