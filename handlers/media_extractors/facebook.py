"""Facebook media extractor."""

import json
import logging
import re
from dataclasses import dataclass
from typing import Protocol
from urllib.parse import urlparse

import httpx
import jmespath
from lxml import html
from lxml.etree import HTMLParser, XMLSyntaxError

from config import FACEBOOK_HEADERS, HTTP_TIMEOUT
from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from services.facebook import get_fb_cookies
from services.http import get_client
from utils.text import decode_json_string, strip_url_params

from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.|m\.|touch\.)?facebook\.com/\S+)")


@dataclass(frozen=True)
class ExtractedMedia:
    """Result from a media extraction strategy."""

    urls: list[str]
    thumbnail: str | None = None


class ExtractionStrategy(Protocol):
    """Protocol for media extraction strategies."""

    name: str

    def extract(self, html_content: str, url: str) -> ExtractedMedia | None:
        """
        Extract media from HTML content.

        Args:
            html_content: The HTML page content
            url: The original URL (for context)

        Returns:
            ExtractedMedia if found, None otherwise
        """
        ...


class PhotoJmespathExtractor:
    """Extract photos from facebook.com/photo.php using jmespath."""

    name = "photo_jmespath"
    _QUERY = jmespath.compile(
        "require[0][3][0].__bbox.require[3][3][1].__bbox.result.data.currMedia.image.uri"
    )

    def can_handle(self, url: str) -> bool:
        """Check if this extractor can handle the given URL."""
        return "/photo.php" in url or "/photo/" in url

    def extract(self, html_content: str, url: str) -> ExtractedMedia | None:
        """Extract photo URI using jmespath."""
        parser = HTMLParser(
            no_network=True, remove_comments=True, remove_pis=True, recover=False
        )

        try:
            tree = html.fromstring(html_content, parser=parser)
        except XMLSyntaxError as e:
            logger.debug(f"HTML parsing failed: {e}")
            return None

        # Get all JSON script tags
        scripts = tree.xpath(
            '/html/body/script[@type="application/json" and @data-sjs]'
        )

        for script in scripts:
            raw_json = script.text
            if not raw_json or not raw_json.strip():
                continue

            try:
                data = json.loads(raw_json)
            except json.JSONDecodeError:
                continue

            result = self._QUERY.search(data)
            if result:
                return ExtractedMedia(urls=[result])

        return None


class MediaBlockExtractor:
    """Extract media from JSON media blocks using ID matching."""

    name = "media_block"
    _MEDIA_BLOCK = re.compile(r'"media"\s*:\s*\{')

    def __init__(self, target_id: str | None = None):
        self.target_id = target_id

    def extract(self, html_content: str, url: str) -> ExtractedMedia | None:
        """Extract media from JSON media blocks."""
        if not self.target_id:
            return None

        decoder = json.JSONDecoder()

        for match in self._MEDIA_BLOCK.finditer(html_content):
            start_idx = match.end() - 1
            try:
                obj, _ = decoder.raw_decode(html_content, start_idx)
                if isinstance(obj, dict) and str(obj.get("id")) == self.target_id:
                    hd = obj.get("downloadable_uri_hd")
                    sd = obj.get("downloadable_uri_sd")
                    media_url = hd or sd

                    if media_url:
                        media_url = strip_url_params(media_url, params_to_remove={"dl"})
                        return ExtractedMedia(urls=[media_url])
            except json.JSONDecodeError:
                continue

        return None


class VideoRegexExtractor:
    """Extract videos using regex fallback."""

    name = "video_regex"
    _VIDEO_PATTERNS = [
        re.compile(r'"browser_native_hd_url":"([^"]*)"'),
        re.compile(r'"browser_native_sd_url":"([^"]*)"'),
    ]

    def extract(self, html_content: str, url: str) -> ExtractedMedia | None:
        """Extract video URLs using regex."""
        for pattern in self._VIDEO_PATTERNS:
            match = pattern.search(html_content)
            if match and (media_url := decode_json_string(match.group(1))):
                media_url = strip_url_params(media_url, params_to_remove={"dl"})
                return ExtractedMedia(urls=[media_url])
        return None


class PhotoRegexExtractor:
    """Extract photos using regex fallback (no cookies only)."""

    name = "photo_regex"
    _PHOTO = re.compile(
        r'"(?:viewer_image|photo_image)"\s*:\s*\{[^}]*?"uri"\s*:\s*"([^"]*)"'
    )
    _PHOTO_FALLBACK = re.compile(r'"created_time":\d+,"image":{"uri":"([^"]*)"')

    def extract(self, html_content: str, url: str) -> ExtractedMedia | None:
        """Extract photo URLs using regex."""
        # Primary pattern
        photo_uris = [
            decoded
            for raw_uri in self._PHOTO.findall(html_content)
            if (decoded := decode_json_string(raw_uri))
        ]

        # Fallback pattern
        if not photo_uris:
            photo_uris = [
                decoded
                for raw_uri in self._PHOTO_FALLBACK.findall(html_content)
                if (decoded := decode_json_string(raw_uri))
            ]

        if photo_uris:
            unique_photos = list(set(photo_uris))
            return ExtractedMedia(urls=unique_photos)

        return None


def _extract_fb_id(url: str) -> str | None:
    """Extract the numeric ID from a Facebook URL."""
    patterns = [
        r"/(?:reel|videos|p|posts|stories)/(\d+)",
        r"[?&]id=(\d+)",
        r"[?&]story_fbid=(\d+)",
        r"[?&]fbid=(\d+)",
    ]
    for pattern in patterns:
        match = re.search(pattern, url)
        if match:
            return match.group(1)
    return None


def _is_facebook_domain(url: str) -> bool:
    """Check if the URL is a valid Facebook domain."""
    try:
        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https"):
            return False

        hostname = parsed.hostname
        if not hostname:
            return False

        hostname = hostname.lower()
        return hostname == "facebook.com" or hostname.endswith(".facebook.com")
    except Exception:
        return False


def _extract_thumbnail(html_content: str) -> str | None:
    """Extract thumbnail URL from HTML."""
    thumb_match = re.search(
        r'"preferred_thumbnail":\{"image":\{"uri":"([^"]*)"', html_content
    )
    if thumb_match:
        return decode_json_string(thumb_match.group(1))
    return None


async def _fetch_facebook(
    client: httpx.AsyncClient, url: str, cookies: httpx.Cookies
) -> HandlerResult | None:
    """Fetch Facebook URL and extract media."""
    current_url = url
    max_redirects = 10
    redirect_count = 0

    # Follow redirects with SSRF protection
    while True:
        if not _is_facebook_domain(current_url):
            logger.warning(f"Aborting request to non-Facebook domain: {current_url}")
            return None

        response = await client.get(
            current_url, cookies=cookies, headers=FACEBOOK_HEADERS
        )

        if response.is_redirect:
            redirect_count += 1
            if redirect_count > max_redirects:
                logger.error(f"Too many redirects for {url}")
                return None

            location = response.headers.get("Location")
            if not location:
                break

            current_url = str(response.url.join(location))
            continue

        response.raise_for_status()
        break

    html_content = response.text
    final_url = str(response.url)
    thumbnail = _extract_thumbnail(html_content)
    target_id = _extract_fb_id(final_url)

    # Priority 1: Photo pages via jmespath
    photo_extractor = PhotoJmespathExtractor()
    if photo_extractor.can_handle(final_url):
        result = photo_extractor.extract(html_content, final_url)
        if result:
            return HandlerResult(
                type=HandlerType.MEDIA_EXTRACTOR,
                content=result.urls,
                metadata={"original_url": final_url, "thumbnail": thumbnail},
            )

    # Priority 2: Media blocks via ID matching
    media_extractor = MediaBlockExtractor(target_id=target_id)
    result = media_extractor.extract(html_content, final_url)
    if result:
        return HandlerResult(
            type=HandlerType.MEDIA_EXTRACTOR,
            content=result.urls,
            metadata={"original_url": final_url, "thumbnail": thumbnail},
        )

    # Priority 3: Video regex fallback
    video_extractor = VideoRegexExtractor()
    result = video_extractor.extract(html_content, final_url)
    if result:
        return HandlerResult(
            type=HandlerType.MEDIA_EXTRACTOR,
            content=result.urls,
            metadata={"original_url": final_url, "thumbnail": thumbnail},
        )

    # Priority 4: Photo regex fallback (no cookies only to avoid newsfeed leakage)
    if not cookies:
        photo_regex = PhotoRegexExtractor()
        result = photo_regex.extract(html_content, final_url)
        if result:
            return HandlerResult(
                type=HandlerType.MEDIA_EXTRACTOR,
                content=result.urls,
                metadata={"original_url": final_url, "thumbnail": thumbnail},
            )

    logger.warning(f"No media found for Facebook URL (Cookies: {bool(cookies)}): {url}")
    return None


@register_handler("facebook")
class FacebookExtractor(MediaExtractor):
    """Extract direct media URLs from Facebook posts/reels/photos."""

    name = "facebook"
    url_pattern = RE_FACEBOOK

    def _validate_url(self, url: str) -> bool:
        """Validate Facebook domain."""
        return _is_facebook_domain(url)

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media from Facebook URL."""
        try:
            cookies = get_fb_cookies()
            client = get_client()

            # Try with cookies first (for restricted content)
            if cookies:
                if not client:
                    async with httpx.AsyncClient(
                        follow_redirects=False, timeout=HTTP_TIMEOUT
                    ) as temp_client:
                        result = await _fetch_facebook(temp_client, url, cookies)
                else:
                    result = await _fetch_facebook(client, url, cookies)

                if result:
                    return result

            # Fallback to no cookies
            if not client:
                async with httpx.AsyncClient(
                    follow_redirects=False, timeout=HTTP_TIMEOUT
                ) as temp_client:
                    result = await _fetch_facebook(temp_client, url, httpx.Cookies())
            else:
                result = await _fetch_facebook(client, url, httpx.Cookies())

            return result

        except httpx.HTTPError as e:
            logger.error(f"HTTP error accessing {url}: {e}")
        except Exception as e:
            logger.error(f"Unexpected error extracting Facebook media from {url}: {e}")

        return None
