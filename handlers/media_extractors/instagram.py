"""Instagram media extractor."""

import base64
import json
import logging
import re
from dataclasses import dataclass
from urllib.parse import parse_qs, urlparse

import httpx
from lxml import html as lxml_html

from config import HTTP_TIMEOUT, USER_AGENT
from core.types import MediaMetadata, MediaResult
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
# its signature is rejected, fall back to snapsave.app, which needs no
# signing but only returns bare media, no caption.
_ANONYIG_CONVERT_ENDPOINT = "https://api-wh.anonyig.com/api/convert"
_ANONYIG_HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "application/json, text/plain, */*",
    "Content-Type": "application/json",
    "Origin": "https://anonyig.com",
    "Referer": "https://anonyig.com/",
}
_ANONYIG_MEDIA_PREFIX = "https://media.anonyig.com/get?"

_DOWNLOADER_ENDPOINT = "https://snapsave.app/action.php?lang=en"
_DOWNLOADER_HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "*/*",
    "Origin": "https://snapsave.app",
    "Referer": "https://snapsave.app/",
    "X-Requested-With": "XMLHttpRequest",
}

# snapsave.app renders its result as a self-executing packed eval() call:
# eval(function(h,u,n,t,e,r){<fixed decoder>}(payload,u,n,t,e,r)). The decoder
# body is a stock JS "packer" template; only the payload and its five numeric
# parameters change per response, so the regex targets the call shape rather
# than the (fixed) decoder source.
_PACKED_RESPONSE = re.compile(
    r'eval\(function\(h,u,n,t,e,r\)\{.*?\}\("([A-Za-z0-9]+)",(\d+),"([^"]+)",(\d+),(\d+),(\d+)\)\)',
    re.DOTALL,
)


@dataclass(frozen=True)
class MediaCandidate:
    """One resolved Instagram media item from a snapsave.app result."""

    url: str
    thumbnail: str | None = None


def _unpack_response(raw_text: str) -> str | None:
    """Decode snapsave.app's packed eval() response into its raw HTML payload.

    The delimiter and digit alphabet double as each other's bound: the
    payload is a sequence of `delimiter`-separated segments, each a number
    written in base `base` using the first `base` characters of `alphabet`
    as digits, with `alphabet[base]` itself reserved as the delimiter so it
    can never collide with a digit.
    """
    match = _PACKED_RESPONSE.search(raw_text)
    if not match:
        return None

    payload, _u, alphabet, offset_text, base_text, _r = match.groups()
    offset = int(offset_text)
    base = int(base_text)
    if base >= len(alphabet):
        return None
    delimiter = alphabet[base]

    try:
        characters = []
        index = 0
        while index < len(payload):
            segment_start = index
            while payload[index] != delimiter:
                index += 1
            digits = "".join(str(alphabet.index(char)) for char in payload[segment_start:index])
            value = int(digits, base) if digits else 0
            characters.append(chr(value - offset))
            index += 1
    except (ValueError, IndexError):
        return None

    decoded_bytes = "".join(characters)
    html_payload = decoded_bytes.encode("latin-1", errors="ignore").decode("utf-8", errors="ignore")
    return html_payload.replace('\\"', '"').replace("\\/", "/")


def _decode_download_token(url_with_token: str) -> str | None:
    """Resolve a snapsave.app download/thumbnail link to the real CDN URL it wraps.

    snapsave.app proxies media through a signed JWT (`?token=...`) whose
    payload embeds the original Instagram CDN URL. That CDN URL is already
    signed by Instagram and directly fetchable, so it is used as-is instead
    of routing media downloads through the proxy.
    """
    tokens = parse_qs(urlparse(url_with_token).query).get("token")
    if not tokens:
        return None

    parts = tokens[0].split(".")
    if len(parts) < 2:
        return None

    padded_payload = parts[1] + "=" * (-len(parts[1]) % 4)
    try:
        data = json.loads(base64.urlsafe_b64decode(padded_payload))
    except (ValueError, json.JSONDecodeError):
        return None

    url = data.get("url")
    return url if isinstance(url, str) else None


def _extract_media_candidates(decoded_html: str) -> list[MediaCandidate]:
    """Extract ordered media candidates from a decoded snapsave.app result page."""
    try:
        tree = lxml_html.fromstring(decoded_html)
    except (ValueError, TypeError) as e:
        logger.debug("Failed to parse Instagram downloader HTML: %r.", e)
        return []

    candidates = []
    for item in tree.xpath('//div[contains(concat(" ", normalize-space(@class), " "), " download-items ")]'):
        hrefs = item.xpath('.//a[contains(@href, "token=")]/@href')
        media_url = _decode_download_token(hrefs[0]) if hrefs else None
        if not media_url:
            continue

        thumb_srcs = item.xpath('.//img[contains(@src, "token=")]/@src')
        thumbnail = _decode_download_token(thumb_srcs[0]) if thumb_srcs else None
        candidates.append(MediaCandidate(url=media_url, thumbnail=thumbnail))

    return candidates


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


class InstagramExtractor(MediaExtractor):
    """Extract direct media from Instagram posts."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> MediaResult | None:
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

    async def _fetch_with_fallback(self, client: httpx.AsyncClient, url: str) -> MediaResult | None:
        """Try anonyig (media + caption) first, then snapsave.app (media only)."""
        try:
            result = await self._fetch_via_anonyig(client, url)
            if result:
                return result
        except Exception as e:
            logger.warning("anonyig Instagram fetch failed for %s: %s.", safe_log_url(url), describe_error(e))

        logger.info("Falling back to snapsave.app for Instagram media: %s.", safe_log_url(url))
        return await self._fetch_via_snapsave(client, url)

    async def _fetch_via_anonyig(self, client: httpx.AsyncClient, url: str) -> MediaResult | None:
        """Fetch media and caption from anonyig.com using a freshly-signed request."""
        payload = await sign_target_url(url)
        if not payload:
            return None

        response = await client.post(_ANONYIG_CONVERT_ENDPOINT, json=payload, headers=_ANONYIG_HEADERS)
        response.raise_for_status()
        return _extract_anonyig_result(response.json(), url)

    async def _fetch_via_snapsave(self, client: httpx.AsyncClient, url: str) -> MediaResult | None:
        """Fetch and decode a snapsave.app result for one Instagram URL."""
        response = await client.post(_DOWNLOADER_ENDPOINT, data={"url": url}, headers=_DOWNLOADER_HEADERS)
        response.raise_for_status()

        decoded_html = _unpack_response(response.text)
        if not decoded_html:
            logger.warning("No packed Instagram downloader response for %s.", safe_log_url(url))
            return None

        candidates = _extract_media_candidates(decoded_html)
        if not candidates:
            logger.warning("No Instagram media links found for %s.", safe_log_url(url))
            return None

        thumbnail = next((candidate.thumbnail for candidate in candidates if candidate.thumbnail), None)
        thumbnail = thumbnail or candidates[0].url

        return MediaResult(
            urls=tuple(candidate.url for candidate in candidates),
            metadata=MediaMetadata(original_url=url, thumbnail=thumbnail),
        )

    def _normalize_instagram_url(self, url: str) -> str:
        return url.rstrip(".,!?;)")
