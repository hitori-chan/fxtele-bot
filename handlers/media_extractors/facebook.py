"""Facebook media extractor."""

import json
import logging
import re
from dataclasses import dataclass
from typing import Any
from urllib.parse import parse_qsl, urlencode, urlparse, urlunparse

import httpx
import jmespath
from lxml import html
from lxml.etree import HTMLParser, ParserError

from config import FACEBOOK_HEADERS, FACEBOOK_PARAMS_TO_KEEP, HTTP_TIMEOUT
from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from services.http import get_client
from utils.text import strip_url_params

from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.|m\.|touch\.)?facebook\.com/\S+)")

_MAX_REDIRECTS = 10
_JSON_PARSER = HTMLParser(no_network=True, remove_comments=True, remove_pis=True, recover=True)

# Handles every supported public page shape in this extractor:
# /reel/{id}, /{page}/videos/{id}, /photo/?fbid=, /photo.php?fbid=,
# /permalink.php?story_fbid=...&id=..., and /{page}/posts/{pfbid}.
_MEDIA_SCRIPT_XPATH = (
    '//script[@type="application/json" and @data-sjs]'
    '[contains(., "__bbox") and ('
    'contains(., "videoDeliveryLegacyFields") '
    'or contains(., "currMedia") '
    'or contains(., "node_v2") '
    'or contains(., "attachments")'
    ")]/text()"
)

# Handles route titles for /reel/{id}, /{page}/videos/{id},
# /permalink.php?story_fbid=...&id=..., and /{page}/posts/{pfbid}.
_ROUTE_SCRIPT_XPATH = '//script[@type="application/json" and @data-sjs][contains(., "initialRouteInfo")]/text()'

_VIDEO_FIELDS = (
    "id:id,"
    "type:__typename,"
    "hd:videoDeliveryLegacyFields.browser_native_hd_url,"
    "sd:videoDeliveryLegacyFields.browser_native_sd_url,"
    "thumb:preferred_thumbnail.image.uri,"
    "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
)
_PHOTO_FIELDS = (
    "id:id,"
    "type:__typename,"
    "photo:viewer_image.uri || photo_image.uri || image.uri || massive_image.uri,"
    "hd:videoDeliveryLegacyFields.browser_native_hd_url,"
    "sd:videoDeliveryLegacyFields.browser_native_sd_url,"
    "thumb:preferred_thumbnail.image.uri || photo_image.uri || image.uri,"
    "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
)

_REEL_QUERIES = (
    # Handles /reel/{id}; ID must match the reel ID in the URL.
    jmespath.compile(
        "video.{"
        "id:id,"
        "hd:creation_story.short_form_video_context.playback_video.videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:creation_story.short_form_video_context.playback_video.videoDeliveryLegacyFields.browser_native_sd_url,"
        "thumb:creation_story.short_form_video_context.playback_video.preferred_thumbnail.image.uri,"
        "caption:creation_story.message.text"
        "}"
    ),
)
_VIDEO_QUERIES = (
    # Handles /{page}/videos/{id}; ID must match the video ID in the URL.
    jmespath.compile(f"video.story.attachments[].media.{{{_VIDEO_FIELDS}}}"),
)
_PHOTO_QUERIES = (
    # Handles /photo/?fbid=... and /photo.php?fbid=...; ID must match fbid.
    jmespath.compile(
        "currMedia.{id:id,type:__typename,photo:image.uri,thumb:image.uri,caption:creation_story.message.text}"
    ),
)
_STORY_ATTACHMENT_QUERIES = (
    # Handles /permalink.php?story_fbid=...&id=... and /{page}/posts/{pfbid}
    # when the story attachment lives directly under node_v2.attachments.
    jmespath.compile(f"node_v2.attachments[].styles.attachment.media.{{{_PHOTO_FIELDS}}}"),
    # Handles the same post/permalink endpoints when Facebook nests the story
    # attachment under node_v2.comet_sections.content.story.attachments.
    jmespath.compile(f"node_v2.comet_sections.content.story.attachments[].styles.attachment.media.{{{_PHOTO_FIELDS}}}"),
)
_CAPTION_QUERIES = (
    # Handles captions for /permalink.php?story_fbid=...&id=... and /{page}/posts/{pfbid}.
    jmespath.compile("node_v2.comet_sections.content.story.message.text"),
    # Handles the same post/permalink captions in an alternate comet_sections shape.
    jmespath.compile("node_v2.comet_sections.content.story.comet_sections.message.story.message.text"),
    # Handles the same post/permalink captions in the message_container shape.
    jmespath.compile("node_v2.comet_sections.content.story.comet_sections.message_container.story.message.text"),
    # Handles captions for /reel/{id} when the media candidate did not carry one.
    jmespath.compile("video.creation_story.message.text"),
    # Handles captions for /photo/?fbid=... and /photo.php?fbid=...
    # when the media candidate did not carry one.
    jmespath.compile("currMedia.creation_story.message.text"),
)

# Handles route titles for /reel/{id}, /{page}/videos/{id},
# /permalink.php?story_fbid=...&id=..., and /{page}/posts/{pfbid}.
_TITLE_QUERY = jmespath.compile("initialRouteInfo.route.meta.title")


@dataclass(frozen=True)
class MediaCandidate:
    """Structured media found in Facebook frontend JSON."""

    id: str | None
    url: str
    thumbnail: str | None = None
    caption: str | None = None


def _is_facebook_domain(url: str) -> bool:
    """Check if the URL points to a Facebook host."""
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
    return hostname == "facebook.com" or hostname.endswith(".facebook.com")


def _normalize_facebook_url(url: str) -> str:
    """Strip volatile Facebook query params and fragments before requesting."""
    parsed = urlparse(url)
    query = urlencode(
        [
            (key, value)
            for key, value in parse_qsl(parsed.query, keep_blank_values=True)
            if key in FACEBOOK_PARAMS_TO_KEEP
        ],
        doseq=True,
    )
    return urlunparse(parsed._replace(query=query, fragment=""))


def _page_kind(url: str) -> str:
    """Classify the Facebook page shape from its path."""
    parsed = urlparse(url)
    path = parsed.path.rstrip("/")

    if re.search(r"/reel/\d+$", path):
        return "reel"
    if re.search(r"/videos/\d+$", path):
        return "video"
    if path == "/photo" or path.endswith("/photo") or path.endswith("/photo.php"):
        return "photo"
    if "/posts/" in path or path.endswith("/permalink.php"):
        return "story"
    return "story"


def _url_media_id(url: str, kind: str) -> str | None:
    """Extract the media ID that should match a photo, video, or reel page."""
    parsed = urlparse(url)
    path = parsed.path.rstrip("/")

    if kind == "reel" and (match := re.search(r"/reel/(\d+)$", path)):
        return match.group(1)
    if kind == "video" and (match := re.search(r"/videos/(\d+)$", path)):
        return match.group(1)
    if kind == "photo":
        params = dict(parse_qsl(parsed.query, keep_blank_values=True))
        return params.get("fbid")
    return None


def _parse_html(html_content: str):
    """Parse Facebook HTML defensively enough for script/meta extraction."""
    try:
        return html.fromstring(html_content, parser=_JSON_PARSER)
    except (ParserError, ValueError) as e:
        logger.debug("Failed to parse Facebook HTML: %r", e)
        return None


def _script_json(tree, xpath: str) -> list[Any]:
    """Load JSON payloads from script nodes selected by XPath."""
    documents = []
    for raw_json in tree.xpath(xpath):
        if not isinstance(raw_json, str) or not raw_json.strip():
            continue
        try:
            documents.append(json.loads(raw_json))
        except json.JSONDecodeError:
            logger.debug("Skipping malformed Facebook JSON script")
    return documents


def _walk_json(value: Any):
    """Yield a JSON value and all nested JSON containers."""
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from _walk_json(child)
    elif isinstance(value, list):
        yield value
        for child in value:
            yield from _walk_json(child)


def _iter_result_items(value: Any):
    """Flatten JMESPath projection results into candidate dictionaries."""
    if isinstance(value, dict):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from _iter_result_items(item)


def _clean_url(value: Any) -> str | None:
    """Return a usable media URL with Facebook's download flag removed."""
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value:
        return None
    return strip_url_params(value, params_to_remove={"dl"})


def _clean_text(value: Any) -> str | None:
    """Return non-empty text from a JSON scalar."""
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value or None


def _media_candidate(raw: dict[str, Any]) -> MediaCandidate | None:
    """Convert a JMESPath media shape into a normalized candidate."""
    media_url = _clean_url(raw.get("hd") or raw.get("sd") or raw.get("photo"))
    if not media_url:
        return None

    media_id = raw.get("id")
    thumbnail = _clean_url(raw.get("thumb"))
    return MediaCandidate(
        id=str(media_id) if media_id is not None else None,
        url=media_url,
        thumbnail=thumbnail,
        caption=_clean_text(raw.get("caption")),
    )


def _queries_for_url(url: str) -> tuple[Any, ...]:
    """Select the narrowest structured media queries for a Facebook URL."""
    match _page_kind(url):
        case "reel":
            return _REEL_QUERIES
        case "video":
            return _VIDEO_QUERIES
        case "photo":
            return _PHOTO_QUERIES
        case _:
            return _STORY_ATTACHMENT_QUERIES


def _extract_media_candidates(documents: list[Any], url: str) -> list[MediaCandidate]:
    """Extract page media from parsed Facebook frontend JSON documents."""
    kind = _page_kind(url)
    target_id = _url_media_id(url, kind)
    require_id_match = kind in {"reel", "video", "photo"}
    if require_id_match and not target_id:
        logger.debug("Refusing %s extraction without a URL media ID: %s", kind, url)
        return []

    queries = _queries_for_url(url)
    seen_urls = set()
    candidates: list[MediaCandidate] = []
    for document in documents:
        for node in _walk_json(document):
            for query in queries:
                for raw in _iter_result_items(query.search(node)):
                    candidate = _media_candidate(raw)
                    if not candidate:
                        continue
                    if require_id_match and candidate.id != target_id:
                        continue
                    if candidate.url in seen_urls:
                        continue
                    seen_urls.add(candidate.url)
                    candidates.append(candidate)
    return candidates


def _extract_json_text(documents: list[Any], queries: tuple[Any, ...]) -> str | None:
    """Extract the first non-empty text value matching any query in priority order."""
    for query in queries:
        for document in documents:
            for node in _walk_json(document):
                text = _clean_text(query.search(node))
                if text:
                    return text
    return None


def _extract_route_title(documents: list[Any]) -> str | None:
    """Extract the frontend route title without using generic UI title labels."""
    for document in documents:
        for node in _walk_json(document):
            title = _clean_text(_TITLE_QUERY.search(node))
            if title:
                return title
    return None


def _extract_meta_content(tree, property_name: str) -> str | None:
    """Extract a single OpenGraph meta value."""
    value = tree.xpath(f'string(//meta[@property="{property_name}"]/@content)')
    return _clean_text(value)


def _extract_facebook_media(html_content: str, url: str) -> HandlerResult | None:
    """Extract media and metadata from a fetched Facebook page."""
    tree = _parse_html(html_content)
    if tree is None:
        return None

    media_documents = _script_json(tree, _MEDIA_SCRIPT_XPATH)
    route_documents = _script_json(tree, _ROUTE_SCRIPT_XPATH)
    candidates = _extract_media_candidates(media_documents, url)
    if not candidates:
        logger.warning("No structured Facebook media found for %s", url)
        return None

    metadata = {"original_url": url}

    thumbnail = next((candidate.thumbnail for candidate in candidates if candidate.thumbnail), None)
    thumbnail = thumbnail or _extract_meta_content(tree, "og:image")
    if thumbnail:
        metadata["thumbnail"] = thumbnail

    caption = next((candidate.caption for candidate in candidates if candidate.caption), None)
    caption = caption or _extract_json_text(media_documents, _CAPTION_QUERIES)
    caption = caption or _extract_meta_content(tree, "og:description")
    if caption:
        metadata["caption"] = caption

    title = _extract_route_title(route_documents) or _extract_meta_content(tree, "og:title")
    if title:
        metadata["title"] = title

    return HandlerResult(
        type=HandlerType.MEDIA_EXTRACTOR,
        content=[candidate.url for candidate in candidates],
        metadata=metadata,
    )


async def _fetch_facebook(client: httpx.AsyncClient, url: str) -> HandlerResult | None:
    """Fetch a Facebook URL and extract public media from structured JSON."""
    current_url = _normalize_facebook_url(url)

    for redirect_count in range(_MAX_REDIRECTS + 1):
        if not _is_facebook_domain(current_url):
            logger.warning("Aborting request to non-Facebook domain: %s", current_url)
            return None

        response = await client.get(current_url, headers=FACEBOOK_HEADERS)
        if not response.is_redirect:
            response.raise_for_status()
            final_url = _normalize_facebook_url(str(response.url))
            return _extract_facebook_media(response.text, final_url)

        if redirect_count == _MAX_REDIRECTS:
            logger.error("Too many Facebook redirects for %s", url)
            return None

        location = response.headers.get("Location")
        if not location:
            logger.warning("Facebook redirect without Location for %s", current_url)
            return None

        current_url = _normalize_facebook_url(str(response.url.join(location)))

    return None


@register_handler("facebook")
class FacebookExtractor(MediaExtractor):
    """Extract direct media URLs from public Facebook posts, reels, photos, and videos."""

    name = "facebook"
    url_pattern = RE_FACEBOOK

    def _validate_url(self, url: str) -> bool:
        """Validate Facebook domain."""
        return _is_facebook_domain(url)

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media from a public Facebook URL."""
        try:
            client = get_client()
            if not client:
                async with httpx.AsyncClient(
                    follow_redirects=False,
                    timeout=HTTP_TIMEOUT,
                    http2=True,
                ) as temp_client:
                    return await _fetch_facebook(temp_client, url)
            return await _fetch_facebook(client, url)
        except httpx.HTTPError as e:
            logger.error("HTTP error accessing %s: %r", url, e)
        except Exception as e:
            logger.error("Unexpected error extracting Facebook media from %s: %r", url, e)
        return None
