"""Reddit media extractor."""

from __future__ import annotations

from dataclasses import dataclass
from html import unescape
import logging
import re
from urllib.parse import ParseResult, parse_qsl, urlencode, urlparse, urlunparse

import httpx
import jmespath

from config import HTTP_TIMEOUT, REDDIT_HEADERS
from core.types import MediaMetadata, MediaResult
from services.http import get_client
from services.reddit_auth import get_reddit_cookies

from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_REDDIT = re.compile(r"(https?://(?:(?:www|old|new|sh)\.)?reddit\.com/\S+|https?://redd\.it/\S+)")
# Reddit /comments/<id>.json returns [post_listing, comments]. This projection keeps the
# fixed post fields small; ordered gallery media still needs Python because gallery_data
# stores media IDs and media_metadata is a dynamic object keyed by those IDs.
_POST_METADATA_QUERY = jmespath.compile(
    "[0].data.children[0].data.{"
    "title:title,"
    "content:selftext,"
    "permalink:permalink,"
    "thumbnail:preview.images[0].source.url || thumbnail,"
    "is_gallery:is_gallery,"
    "gallery_items:gallery_data.items,"
    "media_metadata:media_metadata,"
    "post_url:url_overridden_by_dest || url,"
    "post_hint:post_hint,"
    "video_url:secure_media.reddit_video.fallback_url || media.reddit_video.fallback_url"
    "}"
)
_GALLERY_MEDIA_IDS_QUERY = jmespath.compile("gallery_items[].media_id")


@dataclass(frozen=True)
class RedditPostMetadata:
    """Metadata found for a Reddit post."""

    title: str | None = None
    content: str | None = None
    permalink: str | None = None
    thumbnail: str | None = None
    media_urls: tuple[str, ...] = ()


class RedditExtractor(MediaExtractor):
    """Extract Reddit media from Reddit's post JSON."""

    name = "reddit"
    url_pattern = RE_REDDIT

    def _validate_url(self, url: str) -> bool:
        """Validate Reddit domain."""
        return _is_reddit_domain(url)

    async def _extract_media(self, url: str) -> MediaResult | None:
        """Extract media from a public Reddit URL."""
        try:
            client = get_client()
            if client:
                return await _fetch_reddit(client, url)
            async with httpx.AsyncClient(
                follow_redirects=False,
                timeout=HTTP_TIMEOUT,
                http2=True,
            ) as temp_client:
                return await _fetch_reddit(temp_client, url)
        except httpx.HTTPError as e:
            logger.error("HTTP error accessing Reddit URL %s: %r.", _safe_log_url(url), e)
        except Exception as e:
            logger.error("Unexpected error extracting Reddit media from %s: %r.", _safe_log_url(url), e)
        return None


async def _fetch_reddit(client: httpx.AsyncClient, url: str) -> MediaResult | None:
    """Fetch media and caption data directly from Reddit JSON."""
    cookies = await get_reddit_cookies()
    if _has_cookies(cookies):
        try:
            metadata = await _fetch_reddit_json_metadata(client, url, cookies=cookies)
        except Exception as e:
            logger.warning("Cookie-backed Reddit JSON fetch failed for %s: %r.", _safe_log_url(url), e)
        else:
            return _media_result_from_metadata(metadata, url)

    try:
        metadata = await _fetch_reddit_json_metadata(client, url, cookies=None)
    except Exception as e:
        logger.warning("Unauthenticated Reddit JSON fetch failed for %s: %r.", _safe_log_url(url), e)
        return None
    return _media_result_from_metadata(metadata, url)


async def _fetch_reddit_json_metadata(
    client: httpx.AsyncClient,
    url: str,
    *,
    cookies: httpx.Cookies | None,
) -> RedditPostMetadata | None:
    """Fetch title/body/permalink/media from Reddit's public post JSON."""
    auth_mode = "cookie-backed" if cookies else "unauthenticated"
    last_error: Exception | None = None
    for json_url in _reddit_json_urls(url, include_permalink=bool(cookies)):
        try:
            response = await client.get(
                json_url,
                headers=REDDIT_HEADERS,
                cookies=cookies,
                timeout=HTTP_TIMEOUT,
            )
            response.raise_for_status()
            metadata = _metadata_from_json_data(response.json())
            if metadata:
                return metadata
            logger.warning(
                "%s Reddit JSON endpoint returned no post metadata for %s.",
                auth_mode.capitalize(),
                _safe_log_url(json_url),
            )
        except Exception as e:
            last_error = e
            logger.warning(
                "%s Reddit JSON endpoint failed for %s: %r.",
                auth_mode.capitalize(),
                _safe_log_url(json_url),
                e,
            )
    if last_error:
        raise last_error
    return None


def _metadata_from_json_data(data) -> RedditPostMetadata | None:
    """Normalize Reddit post JSON into metadata and direct media URLs."""
    raw = _POST_METADATA_QUERY.search(data)
    if not isinstance(raw, dict):
        return None
    metadata = _metadata_from_mapping(raw)
    media_urls = _media_urls_from_post(raw)
    if not metadata and not media_urls:
        return None
    if not metadata:
        return RedditPostMetadata(media_urls=tuple(media_urls))
    return RedditPostMetadata(
        title=metadata.title,
        content=metadata.content,
        permalink=metadata.permalink,
        thumbnail=metadata.thumbnail or next(iter(media_urls), None),
        media_urls=tuple(media_urls),
    )


def _media_result_from_metadata(metadata: RedditPostMetadata | None, original_url: str) -> MediaResult | None:
    """Build a media result directly from Reddit metadata when media exists."""
    if not metadata or not metadata.media_urls:
        return None
    caption = _caption_from_metadata(metadata) or metadata.title
    return MediaResult(
        urls=metadata.media_urls,
        metadata=MediaMetadata(
            original_url=metadata.permalink or original_url,
            thumbnail=metadata.thumbnail or next(iter(metadata.media_urls), None),
            caption=caption,
            title=metadata.title,
        ),
    )


def _caption_from_metadata(metadata: RedditPostMetadata) -> str | None:
    """Build a caption from Reddit title and post body."""
    parts = [metadata.title, metadata.content]
    caption = "\n\n".join(part for part in parts if part)
    return caption or None


def _metadata_from_mapping(raw: dict) -> RedditPostMetadata | None:
    """Normalize a Reddit metadata projection."""
    title = _clean_text(raw.get("title"))
    content = _clean_text(raw.get("content"))
    permalink = _clean_text(raw.get("permalink"))
    if permalink and permalink.startswith("/"):
        permalink = f"https://www.reddit.com{permalink}"
    permalink = _clean_url(permalink)
    thumbnail = _clean_url(raw.get("thumbnail"))
    if not any((title, content, permalink, thumbnail)):
        return None
    return RedditPostMetadata(
        title=title,
        content=content,
        permalink=permalink,
        thumbnail=thumbnail,
    )


def _media_urls_from_post(raw: dict) -> tuple[str, ...]:
    """Return direct media URLs exposed by Reddit's post JSON."""
    urls = [
        *_gallery_media_urls(raw),
        _clean_url(raw.get("video_url")),
        _single_media_url(raw),
    ]

    unique_urls = []
    seen = set()
    for url in urls:
        if not url or url in seen:
            continue
        unique_urls.append(url)
        seen.add(url)
    return tuple(unique_urls)


def _gallery_media_urls(raw: dict) -> tuple[str, ...]:
    """Return ordered direct image URLs from a Reddit gallery metadata projection."""
    if raw.get("is_gallery") is not True:
        return ()

    media_ids = _GALLERY_MEDIA_IDS_QUERY.search(raw)
    media_metadata = raw.get("media_metadata")
    if not isinstance(media_ids, list) or not isinstance(media_metadata, dict):
        return ()

    urls = []
    seen = set()
    for media_id in media_ids:
        media_item = media_metadata.get(media_id) if isinstance(media_id, str) else None
        media_url = _gallery_media_url(media_item)
        if media_url and media_url not in seen:
            urls.append(media_url)
            seen.add(media_url)
    return tuple(urls)


def _gallery_media_url(media_item) -> str | None:
    """Return the best direct media URL from one Reddit gallery media item."""
    if not isinstance(media_item, dict) or media_item.get("status") != "valid":
        return None
    mime_type = media_item.get("m")
    if not isinstance(mime_type, str) or not mime_type.startswith("image/"):
        return None

    source = media_item.get("s")
    if not isinstance(source, dict):
        return None
    return _clean_url(source.get("u") or source.get("gif"))


def _single_media_url(raw: dict) -> str | None:
    """Return a direct image URL from non-gallery Reddit image posts."""
    post_url = _clean_url(raw.get("post_url"))
    if not post_url:
        return None
    if raw.get("post_hint") == "image" or _is_image_url(post_url):
        return post_url
    return None


def _reddit_json_urls(url: str, *, include_permalink: bool = True) -> tuple[str, ...]:
    """Return Reddit JSON endpoints in request order."""
    parsed = urlparse(url)
    candidates = []

    permalink_path = _permalink_json_path(parsed)
    if include_permalink and permalink_path:
        candidates.append(_reddit_json_url("www.reddit.com", permalink_path))

    post_id = _reddit_post_id(parsed)
    compact_path = f"/comments/{post_id}.json" if post_id else _json_path(parsed.path)
    candidates.append(_reddit_json_url("www.reddit.com", compact_path))

    return tuple(dict.fromkeys(candidates))


def _reddit_json_url(hostname: str, path: str) -> str:
    return urlunparse(("https", hostname, path, "", "raw_json=1", ""))


def _reddit_post_id(parsed: ParseResult) -> str | None:
    """Return the Reddit post ID from common Reddit URL shapes."""
    hostname = (parsed.hostname or "").lower()
    parts = [part for part in parsed.path.rstrip("/").split("/") if part]

    if hostname == "redd.it" and parts:
        return parts[0]
    if parts[:1] == ["gallery"] and len(parts) > 1:
        return parts[1]
    if "comments" in parts:
        comment_index = parts.index("comments")
        if len(parts) > comment_index + 1:
            return parts[comment_index + 1]
    return None


def _permalink_json_path(parsed: ParseResult) -> str | None:
    """Return a subreddit permalink JSON path when the original URL contains it."""
    parts = [part for part in parsed.path.rstrip("/").split("/") if part]
    if "comments" not in parts:
        return None
    comment_index = parts.index("comments")
    if len(parts) <= comment_index + 1:
        return None
    path = "/" + "/".join(parts[: comment_index + 3])
    return _json_path(path)


def _json_path(path: str) -> str:
    """Attach Reddit's .json suffix while preserving the original post path."""
    path = path.rstrip("/")
    if not path:
        return "/.json"
    if path.endswith(".json"):
        return path
    return f"{path}/.json"


def _is_reddit_domain(url: str) -> bool:
    """Check if the URL points to Reddit or a Reddit short-link host."""
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
    return hostname == "redd.it" or hostname == "reddit.com" or hostname.endswith(".reddit.com")


def _is_image_url(url: str) -> bool:
    """Detect image URLs from common Reddit image extensions."""
    path = urlparse(url).path.lower()
    return path.endswith((".jpg", ".jpeg", ".png", ".webp", ".gif"))


def _has_cookies(cookies: httpx.Cookies) -> bool:
    """Return true when an httpx cookie jar contains at least one cookie."""
    return any(True for _ in cookies.jar)


def _clean_url(value) -> str | None:
    """Return a usable URL from an HTML scalar."""
    if not isinstance(value, str):
        return None
    value = unescape(value).strip()
    if not value:
        return None
    if not value.startswith(("http://", "https://")):
        return None
    return value


def _clean_text(value) -> str | None:
    """Return non-empty text from an HTML scalar."""
    if not isinstance(value, str):
        return None
    value = unescape(value).strip()
    return value or None


def _safe_log_url(url: str) -> str:
    """Return a Reddit URL safe enough for logs."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return "<invalid-url>"
    query = dict(parse_qsl(parsed.query, keep_blank_values=True))
    kept_query = urlencode({"raw_json": query["raw_json"]}) if query.get("raw_json") else ""
    return urlunparse(parsed._replace(query=kept_query, fragment=""))
