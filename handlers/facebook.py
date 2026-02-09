import re
import json
import logging
import httpx
from urllib.parse import urlparse
from config import FACEBOOK_HEADERS, HTTP_TIMEOUT
from utils.http_client import get_client, get_fb_cookies
from utils.text import decode_json_string, strip_url_params

logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.)?facebook\.com/\S+)")


class FBPatterns:
    """Facebook media extraction patterns."""

    # Video patterns ordered by priority (HD > SD)
    VIDEO = [
        re.compile(r'"browser_native_hd_url":"([^"]*)"'),
        re.compile(r'"browser_native_sd_url":"([^"]*)"'),
    ]

    MEDIA_BLOCK = re.compile(r'"media"\s*:\s*\{')

    PHOTO = re.compile(r'"(?:viewer_image|photo_image)"\s*:\s*\{[^}]*?"uri"\s*:\s*"([^"]*)"')
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


async def _fetch_facebook(client: httpx.AsyncClient, url: str, cookies: httpx.Cookies) -> dict | None:
    """Perform the request using a provided client with manual redirect handling for SSRF protection."""
    current_url = url
    max_redirects = 10
    redirect_count = 0

    while True:
        if not _is_facebook_domain(current_url):
            logger.warning(f"Aborting request to non-Facebook domain: {current_url}")
            return None

        response = await client.get(current_url, cookies=cookies, headers=FACEBOOK_HEADERS)

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

    # Priority 1: Try parsing "media" JSON blocks directly (safest, targeted by ID)
    target_id = _extract_fb_id(final_url)
    if target_id:
        decoder = json.JSONDecoder()
        for match in FBPatterns.MEDIA_BLOCK.finditer(html):
            start_idx = match.end() - 1  # Start from the '{'
            try:
                obj, _ = decoder.raw_decode(html, start_idx)
                if isinstance(obj, dict):
                    # Verify ID matches the requested one
                    if str(obj.get("id")) == target_id:
                        # Extract URLs (HD preferred)
                        hd = obj.get("downloadable_uri_hd")
                        sd = obj.get("downloadable_uri_sd")

                        media_url = hd or sd
                        if media_url:
                            media_url = strip_url_params(media_url, params_to_remove={"dl"})
                            return {
                                "type": "media",
                                "urls": [media_url],
                                "thumbnail": thumbnail,
                                "original_url": final_url,
                            }
            except json.JSONDecodeError:
                continue

    # Priority 2: Fallback to browser_native_url (usually directly linked to the post, but less strict on ID)
    for pattern in FBPatterns.VIDEO:
        match = pattern.search(html)
        if match and (media_url := decode_json_string(match.group(1))):
            media_url = strip_url_params(media_url, params_to_remove={"dl"})
            return {"type": "media", "urls": [media_url], "thumbnail": thumbnail, "original_url": final_url}

    # BUG: When cookies are added, the photo regex matches all photos on the newsfeed.
    # We disable photo extraction when cookies are present to avoid this.
    if cookies:
        return None

    # If no video, extract all unique photo URLs
    photo_uris = [decoded for raw_uri in FBPatterns.PHOTO.findall(html) if (decoded := decode_json_string(raw_uri))]

    # Fallback for direct photo links (e.g., facebook.com/photo/?fbid=...)
    if not photo_uris:
        photo_uris = [
            decoded for raw_uri in FBPatterns.PHOTO_FALLBACK.findall(html) if (decoded := decode_json_string(raw_uri))
        ]

    if photo_uris:
        unique_photos = list(set(photo_uris))
        return {"type": "media", "urls": unique_photos, "thumbnail": thumbnail, "original_url": final_url}

    logger.warning(f"No media found for Facebook URL (Cookies: {bool(cookies)}): {url}")
    return None


async def handle_facebook(text: str) -> dict[str, str | list[str]] | None:
    """Extract direct media URLs from Facebook links."""
    match = RE_FACEBOOK.search(text)
    if not match:
        return None

    url = match.group(1)
    if not _is_facebook_domain(url):
        return None

    try:
        cookies = get_fb_cookies()
        client = get_client()

        # Step 1: Try with cookies (better for restricted videos/reels)
        result = None
        if cookies:
            if not client:
                async with httpx.AsyncClient(follow_redirects=False, timeout=HTTP_TIMEOUT) as temp_client:
                    result = await _fetch_facebook(temp_client, url, cookies)
            else:
                result = await _fetch_facebook(client, url, cookies)

        # Step 2: Fallback to no-cookies if no video found (safest for photos)
        if not result:
            if not client:
                async with httpx.AsyncClient(follow_redirects=False, timeout=HTTP_TIMEOUT) as temp_client:
                    result = await _fetch_facebook(temp_client, url, httpx.Cookies())
            else:
                result = await _fetch_facebook(client, url, httpx.Cookies())

        return result

    except httpx.HTTPError as e:
        logger.error(f"HTTP error accessing {url}: {e}")
    except Exception as e:
        logger.error(f"Unexpected error extracting Facebook media from {url}: {e}")

    return None
