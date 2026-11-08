"""Instagram media extractor."""

import logging
import re
from urllib.parse import parse_qs, urlparse, urlunparse

import httpx

from config import HTTP_TIMEOUT, USER_AGENT
from core.types import HandlerResult, LinkFixResult, MediaMetadata, MediaResult
from services.anonyig_signer import sign_target_url
from services.http import get_client
from utils.error_text import describe_error
from utils.text import safe_log_url

from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_INSTAGRAM = re.compile(r"(https?://(?:www\.)?instagram\.com/\S+)")

# anonyig.com returns full post metadata (including the caption), but only
# when the request carries a signature computed by its own client-side JS
# (see services/anonyig_signer/signer.js). If that signer is unavailable or
# its signature is rejected (e.g. anonyig's Cloudflare Turnstile gate), fall
# back to an oginstagram.com link: Telegram's own preview fetcher renders
# that into a native-looking embed with caption, media, and stats, since
# oginstagram.com is built specifically for link-preview bots and refuses
# direct requests from a plain backend like ours.
_ANONYIG_CONVERT_ENDPOINT = "https://api-wh.anonyig.com/api/convert"
_ANONYIG_HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "application/json, text/plain, */*",
    "Content-Type": "application/json",
    "Origin": "https://anonyig.com",
    "Referer": "https://anonyig.com/",
}
_ANONYIG_MEDIA_PREFIX = "https://media.anonyig.com/get?"


def _resolve_anonyig_proxy_url(value: object) -> str | None:
    """Unwrap a media.anonyig.com proxy link to the real, Instagram-signed CDN URL it wraps."""
    if not isinstance(value, str):
        return None
    if not value.startswith(_ANONYIG_MEDIA_PREFIX):
        return value

    real_urls = parse_qs(urlparse(value).query).get("uri")
    return real_urls[0] if real_urls else value


def _extract_anonyig_result(data: object, url: str) -> MediaResult | None:
    """Build a media result from anonyig's convert response for one Instagram URL."""
    items = data if isinstance(data, list) else [data]

    media_urls: list[str] = []
    thumbnail: str | None = None
    caption: str | None = None
    for item in items:
        if not isinstance(item, dict):
            continue

        for media in item.get("url") or []:
            real_url = _resolve_anonyig_proxy_url(media.get("url")) if isinstance(media, dict) else None
            if real_url:
                media_urls.append(real_url)

        if thumbnail is None:
            thumbnail = _resolve_anonyig_proxy_url(item.get("thumb"))

        if caption is None:
            meta = item.get("meta")
            title = meta.get("title") if isinstance(meta, dict) else None
            if isinstance(title, str) and title.strip():
                caption = title.strip()

    unique_urls = tuple(dict.fromkeys(media_urls))
    if not unique_urls:
        return None

    return MediaResult(
        urls=unique_urls,
        metadata=MediaMetadata(original_url=url, thumbnail=thumbnail or unique_urls[0], caption=caption),
    )


def _oginstagram_fallback_url(url: str) -> str:
    """Build a last-resort oginstagram.com link, preserving the original path and query."""
    parsed = urlparse(url)
    return urlunparse(parsed._replace(scheme="https", netloc="oginstagram.com"))


class InstagramExtractor(MediaExtractor):
    """Extract direct media from Instagram posts."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media URLs from a public Instagram post."""
        url = self._normalize_instagram_url(url)

        try:
            client = get_client()
            if not client:
                async with httpx.AsyncClient(timeout=HTTP_TIMEOUT, http2=True) as temp_client:
                    return await self._fetch_with_fallback(temp_client, url)
            return await self._fetch_with_fallback(client, url)
        except Exception:
            logger.exception("Error extracting Instagram media from %s.", safe_log_url(url))
            return None

    async def _fetch_with_fallback(self, client: httpx.AsyncClient, url: str) -> HandlerResult | None:
        """Try anonyig (media + caption), then an oginstagram.com link."""
        try:
            result = await self._fetch_via_anonyig(client, url)
            if result:
                return result
        except Exception as e:
            logger.warning("anonyig Instagram fetch failed for %s: %s.", safe_log_url(url), describe_error(e))

        logger.info("No Instagram media found; replying with an oginstagram.com link for %s.", safe_log_url(url))
        return LinkFixResult(content=_oginstagram_fallback_url(url))

    async def _fetch_via_anonyig(self, client: httpx.AsyncClient, url: str) -> MediaResult | None:
        """Fetch media and caption from anonyig.com using a freshly-signed request."""
        payload = await sign_target_url(url)
        if not payload:
            return None

        response = await client.post(_ANONYIG_CONVERT_ENDPOINT, json=payload, headers=_ANONYIG_HEADERS)
        response.raise_for_status()
        return _extract_anonyig_result(response.json(), url)

    def _normalize_instagram_url(self, url: str) -> str:
        return url.rstrip(".,!?;)")
