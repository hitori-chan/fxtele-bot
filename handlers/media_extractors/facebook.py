"""Facebook media extractor."""

import json
import logging
import re
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

# JMESPath query for facebook.com/photo.php pages
_PHOTO_QUERY = (
    "require[0][3][0].__bbox.require[3][3][1].__bbox.result.data.currMedia.image.uri"
)
_PHOTO_EXPR = jmespath.compile(_PHOTO_QUERY)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.)?facebook\.com/\S+)")


class FBPatterns:
    """Facebook media extraction patterns."""

    # Video patterns ordered by priority (HD > SD)
    VIDEO = [
        re.compile(r'"browser_native_hd_url":"([^"]*)"'),
        re.compile(r'"browser_native_sd_url":"([^"]*)"'),
    ]

    MEDIA_BLOCK = re.compile(r'"media"\s*:\s*\{')

    PHOTO = re.compile(
        r'"(?:viewer_image|photo_image)"\s*:\s*\{[^}]*?"uri"\s*:\s*"([^"]*)"'
    )
    PHOTO_FALLBACK = re.compile(r'"created_time":\d+,"image":{"uri":"([^"]*)"')
    THUMBNAIL = re.compile(r'"preferred_thumbnail":{"image":{"uri":"([^"]*)"')


def _extract_fb_id(url: str) -> str | None:
    """Extract the numeric ID from a Facebook URL (post, video, reel)."""
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
    """Check if the URL is a valid Facebook domain with a safe protocol."""
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


def _is_photo_url(url: str) -> bool:
    """Check if URL is a facebook.com/photo.php link."""
    return "/photo.php" in url or "/photo/" in url


def _extract_photo_from_html(html_content: str) -> str | None:
    """
    Extract photo URI from facebook.com/photo.php HTML using jmespath.

    Args:
        html_content: The HTML page content

    Returns:
        The image URI if found, None otherwise
    """
    parser = HTMLParser(
        no_network=True, remove_comments=True, remove_pis=True, recover=False
    )

    try:
        tree = html.fromstring(html_content, parser=parser)
    except XMLSyntaxError as e:
        logger.debug(f"HTML parsing failed: {e}")
        return None

    # Get ALL matching script elements
    scripts = tree.xpath('/html/body/script[@type="application/json" and @data-sjs]')

    if not scripts:
        return None

    for script in scripts:
        raw_json = script.text

        if not raw_json or not raw_json.strip():
            continue

        try:
            data = json.loads(raw_json)
        except json.JSONDecodeError:
            continue

        result = _PHOTO_EXPR.search(data)

        if result:
            return result

    return None


async def _fetch_facebook(
    client: httpx.AsyncClient, url: str, cookies: httpx.Cookies
) -> dict | None:
    """Perform the request with manual redirect handling for SSRF protection."""
    current_url = url
    max_redirects = 10
    redirect_count = 0

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

            # Resolve relative URLs
            current_url = str(response.url.join(location))
            continue

        response.raise_for_status()
        break

    html = response.text
    final_url = str(response.url)

    # Extract thumbnail
    thumbnail = None
    thumb_match = FBPatterns.THUMBNAIL.search(html)
    if thumb_match:
        thumbnail = decode_json_string(thumb_match.group(1))

    # Priority 0: Handle facebook.com/photo.php URLs with jmespath
    if _is_photo_url(final_url):
        photo_uri = _extract_photo_from_html(html)
        if photo_uri:
            return {
                "type": HandlerType.MEDIA_EXTRACTOR,
                "urls": [photo_uri],
                "thumbnail": thumbnail,
                "original_url": final_url,
            }

    # Priority 1: Try parsing "media" JSON blocks directly
    target_id = _extract_fb_id(final_url)
    if target_id:
        decoder = json.JSONDecoder()
        for match in FBPatterns.MEDIA_BLOCK.finditer(html):
            start_idx = match.end() - 1
            try:
                obj, _ = decoder.raw_decode(html, start_idx)
                if isinstance(obj, dict):
                    if str(obj.get("id")) == target_id:
                        hd = obj.get("downloadable_uri_hd")
                        sd = obj.get("downloadable_uri_sd")

                        media_url = hd or sd
                        if media_url:
                            media_url = strip_url_params(
                                media_url, params_to_remove={"dl"}
                            )
                            return {
                                "type": HandlerType.MEDIA_EXTRACTOR,
                                "urls": [media_url],
                                "thumbnail": thumbnail,
                                "original_url": final_url,
                            }
            except json.JSONDecodeError:
                continue

    # Priority 2: Fallback to browser_native_url
    for pattern in FBPatterns.VIDEO:
        match = pattern.search(html)
        if match and (media_url := decode_json_string(match.group(1))):
            media_url = strip_url_params(media_url, params_to_remove={"dl"})
            return {
                "type": HandlerType.MEDIA_EXTRACTOR,
                "urls": [media_url],
                "thumbnail": thumbnail,
                "original_url": final_url,
            }

    # BUG: When cookies are added, the photo regex matches all photos on the newsfeed.
    # We disable photo extraction when cookies are present to avoid this.
    if cookies:
        return None

    # If no video, extract all unique photo URLs
    photo_uris = [
        decoded
        for raw_uri in FBPatterns.PHOTO.findall(html)
        if (decoded := decode_json_string(raw_uri))
    ]

    # Fallback for direct photo links
    if not photo_uris:
        photo_uris = [
            decoded
            for raw_uri in FBPatterns.PHOTO_FALLBACK.findall(html)
            if (decoded := decode_json_string(raw_uri))
        ]

    if photo_uris:
        unique_photos = list(set(photo_uris))
        return {
            "type": HandlerType.MEDIA_EXTRACTOR,
            "urls": unique_photos,
            "thumbnail": thumbnail,
            "original_url": final_url,
        }

    logger.warning(f"No media found for Facebook URL (Cookies: {bool(cookies)}): {url}")
    return None


@register_handler("facebook")
class FacebookExtractor(MediaExtractor):
    """Extract direct media URLs from Facebook posts/reels."""

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

            # Step 1: Try with cookies (better for restricted videos/reels)
            result = None
            if cookies:
                if not client:
                    async with httpx.AsyncClient(
                        follow_redirects=False, timeout=HTTP_TIMEOUT
                    ) as temp_client:
                        result = await _fetch_facebook(temp_client, url, cookies)
                else:
                    result = await _fetch_facebook(client, url, cookies)

            # Step 2: Fallback to no-cookies if no video found
            if not result:
                if not client:
                    async with httpx.AsyncClient(
                        follow_redirects=False, timeout=HTTP_TIMEOUT
                    ) as temp_client:
                        result = await _fetch_facebook(
                            temp_client, url, httpx.Cookies()
                        )
                else:
                    result = await _fetch_facebook(client, url, httpx.Cookies())

            if result:
                return HandlerResult(
                    type=HandlerType.MEDIA_EXTRACTOR,
                    content=result["urls"],
                    metadata={
                        "original_url": result["original_url"],
                        "thumbnail": result.get("thumbnail"),
                    },
                )

        except httpx.HTTPError as e:
            logger.error(f"HTTP error accessing {url}: {e}")
        except Exception as e:
            logger.error(f"Unexpected error extracting Facebook media from {url}: {e}")

        return None
