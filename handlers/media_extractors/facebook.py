"""Facebook media extractor."""

import json
import logging
import re
from contextlib import suppress
from dataclasses import dataclass
from typing import Any
from urllib.parse import parse_qsl, urlencode, urlparse, urlunparse

import httpx
import jmespath
from lxml import html
from lxml.etree import HTMLParser, ParserError

from config import FACEBOOK_HEADERS, FACEBOOK_PARAMS_TO_KEEP, HTTP_TIMEOUT
from core.types import MediaMetadata, MediaResult
from services.facebook_auth import facebook_auth_available, get_facebook_cookies
from services.http import get_client
from utils.text import strip_url_params

from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_FACEBOOK = re.compile(r"(https?://(?:www\.|m\.|touch\.)?facebook\.com/\S+)")

_MAX_REDIRECTS = 10
_JSON_PARSER = HTMLParser(no_network=True, remove_comments=True, remove_pis=True, recover=True)

# JSON script candidates for every supported page shape:
# - /reel/{id}: short-form playback lives under video.creation_story.
# - /{page}/videos/{id}: video post media lives under video.story.attachments.
# - /photo/?fbid= and /photo.php?fbid=: photo media lives under currMedia.
# - /permalink.php?... and /{page}/posts/{pfbid}: story media lives under node_v2 attachments.
# - /stories/{bucket_id}/{story_card_id}: story-card media lives under attachments.
# - /watch/?v={id}: video representations live in DASH prefetch extensions.
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

_REEL_QUERIES = (
    # Endpoint: /reel/{id}
    # JSON shape: video.creation_story.short_form_video_context.playback_video
    # ID check: required; playback_video.id must match /reel/{id}.
    jmespath.compile(
        "video.{"
        "id:id,"
        # Prefer HD video; fall back to SD. Both are direct browser playback URLs.
        "hd:creation_story.short_form_video_context.playback_video.videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:creation_story.short_form_video_context.playback_video.videoDeliveryLegacyFields.browser_native_sd_url,"
        # Thumbnail shown by Telegram inline/video preview.
        "thumb:creation_story.short_form_video_context.playback_video.preferred_thumbnail.image.uri,"
        # Reel author caption.
        "caption:creation_story.message.text"
        "}"
    ),
)
_VIDEO_QUERIES = (
    # Endpoint: /{page}/videos/{id}
    # JSON shape: video.story.attachments[].media
    # ID check: required; media.id must match /videos/{id}.
    jmespath.compile(
        "video.story.attachments[].media.{"
        "id:id,"
        "type:__typename,"
        # Standard Facebook video post direct URLs.
        "hd:videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:videoDeliveryLegacyFields.browser_native_sd_url,"
        # Video preview image.
        "thumb:preferred_thumbnail.image.uri,"
        # Captions appear in either legacy or comet story message nodes.
        "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
        "}"
    ),
)
_PHOTO_QUERIES = (
    # Endpoints: /photo/?fbid=... and /photo.php?fbid=...
    # JSON shape: currMedia
    # ID check: required; currMedia.id must match fbid.
    jmespath.compile(
        "currMedia.{"
        "id:id,"
        "type:__typename,"
        # Direct photo URL for standalone photo pages.
        "photo:image.uri,"
        # Same image is usable as thumbnail.
        "thumb:image.uri,"
        # Standalone photo caption.
        "caption:creation_story.message.text"
        "}"
    ),
)
_STORY_ATTACHMENT_QUERIES = (
    # Endpoints: /permalink.php?story_fbid=...&id=... and /{page}/posts/{pfbid}
    # JSON shape A: node_v2.attachments[].styles.attachment.media
    # Used when the primary story attachment is directly under node_v2.
    jmespath.compile(
        "node_v2.attachments[].styles.attachment.media.{"
        "id:id,"
        "type:__typename,"
        # Photos expose several image sizes; prefer viewer_image when present.
        "photo:viewer_image.uri || photo_image.uri || image.uri || massive_image.uri,"
        # Some story attachments are videos. Older posts use videoDeliveryLegacyFields;
        # album/reel-like attachments can instead expose playable_url[_quality_hd].
        "hd:playable_url_quality_hd || videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:playable_url || videoDeliveryLegacyFields.browser_native_sd_url,"
        "thumb:previewImage.uri || preferred_thumbnail.image.uri || photo_image.uri || image.uri,"
        "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
        "}"
    ),
    # Same endpoints.
    # JSON shape B: node_v2.comet_sections.content.story.attachments[].styles.attachment.media
    # Used by newer Comet story sections.
    jmespath.compile(
        "node_v2.comet_sections.content.story.attachments[].styles.attachment.media.{"
        "id:id,"
        "type:__typename,"
        "photo:viewer_image.uri || photo_image.uri || image.uri || massive_image.uri,"
        # Same video URL variants as shape A.
        "hd:playable_url_quality_hd || videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:playable_url || videoDeliveryLegacyFields.browser_native_sd_url,"
        "thumb:previewImage.uri || preferred_thumbnail.image.uri || photo_image.uri || image.uri,"
        "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
        "}"
    ),
    # Same endpoints.
    # JSON shape C: all_subattachments.nodes[].media
    # Used by multi-photo album posts rendered as StoryAttachmentAlbumStyleRenderer.
    jmespath.compile(
        "all_subattachments.nodes[].media.{"
        "id:id,"
        "type:__typename,"
        "photo:viewer_image.uri || photo_image.uri || image.uri || massive_image.uri,"
        # Album nodes may be mixed photo/video media and often use playable_url.
        "hd:playable_url_quality_hd || videoDeliveryLegacyFields.browser_native_hd_url,"
        "sd:playable_url || videoDeliveryLegacyFields.browser_native_sd_url,"
        "thumb:previewImage.uri || preferred_thumbnail.image.uri || photo_image.uri || image.uri,"
        "caption:creation_story.message.text || creation_story.comet_sections.message.story.message.text"
        "}"
    ),
)
_STORY_CARD_QUERIES = (
    # Endpoint: /stories/{bucket_id}/{story_card_id}
    # JSON shape: story-card object attachments[].media
    # ID check: required on the parent story-card object, not media.id.
    jmespath.compile(
        "attachments[].media.{"
        "id:id,"
        "type:__typename,"
        # Story photos use image.uri; story videos use playable_url[_quality_hd].
        "photo:image.uri,"
        "hd:playable_url_quality_hd,"
        "sd:playable_url,"
        # previewImage is common for story videos; image is common for story photos.
        "thumb:previewImage.uri || image.uri,"
        "caption:message.text"
        "}"
    ),
)
_WATCH_CAPTION_QUERY = jmespath.compile("creation_story.comet_sections.message.story.message.text")
_WATCH_THUMBNAIL_QUERY = jmespath.compile("preferred_thumbnail.image.uri || image.uri || previewImage.uri")
_VIDEO_NODE_CAPTION_QUERIES = (
    # Handles authenticated /reel/{id} video nodes when legacy playback fields are absent.
    jmespath.compile("creation_story.message.text"),
    # Handles /watch/?v={id} and regular video story nodes.
    _WATCH_CAPTION_QUERY,
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


@dataclass(frozen=True)
class StoryAlbumInfo:
    """Album expansion metadata for a Facebook story attachment."""

    token: str
    count: int


class FacebookAuthExpired(RuntimeError):
    """Raised when authenticated Facebook cookies no longer reach content pages."""


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


def _is_login_url(url: str) -> bool:
    """Return true for Facebook login/checkpoint URLs."""
    path = urlparse(url).path.lower()
    return "/login" in path or "/checkpoint" in path or "/two_step_verification" in path


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


def _safe_log_url(url: str) -> str:
    """Return a URL safe enough for logs without volatile query tokens."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return "<invalid-url>"
    if _is_facebook_domain(url):
        return _normalize_facebook_url(url)
    return urlunparse(parsed._replace(query="", fragment=""))


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
    if path == "/watch":
        return "watch_video"
    if "/stories/" in path:
        return "story_card"
    if "/posts/" in path or path.endswith("/permalink.php"):
        return "story"
    return "story"


def _is_profile_url(url: str) -> bool:
    """Return true for root profile/page URL shapes."""
    parsed = urlparse(url)
    path = parsed.path.rstrip("/")
    parts = [part for part in path.split("/") if part]

    if path == "/profile.php":
        return bool(dict(parse_qsl(parsed.query, keep_blank_values=True)).get("id"))
    return len(parts) == 1


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
    if kind == "watch_video":
        params = dict(parse_qsl(parsed.query, keep_blank_values=True))
        return params.get("v")
    if kind == "story_card":
        parts = [part for part in path.split("/") if part]
        if len(parts) >= 3:
            return parts[2]
    return None


def _url_story_token(url: str) -> str | None:
    """Extract the stable story token from post/permalink URL shapes."""
    parsed = urlparse(url)
    params = dict(parse_qsl(parsed.query, keep_blank_values=True))
    if story_fbid := params.get("story_fbid"):
        return story_fbid

    parts = [part for part in parsed.path.rstrip("/").split("/") if part]
    if len(parts) >= 3 and parts[-2] == "posts":
        return parts[-1]
    return None


def _route_story_tokens(documents: list[Any]) -> tuple[str, ...]:
    """Extract canonical story tokens from Facebook route payloads."""
    tokens = []
    for document in documents:
        for node in _walk_json(document):
            if not isinstance(node, dict):
                continue
            params = node.get("params")
            if not isinstance(params, dict):
                continue
            for key in ("story_token", "story_fbid"):
                story_token = params.get(key)
                if isinstance(story_token, str) and story_token:
                    tokens.append(story_token)
    return tuple(dict.fromkeys(tokens))


def _node_contains_story_token(node: dict[str, Any], story_token: str) -> bool:
    """Return true when a JSON subtree belongs to the requested story token."""
    with suppress(TypeError, ValueError):
        return story_token in json.dumps(node, ensure_ascii=False)
    return False


def _media_file_key(url: str) -> str:
    """Return a stable enough key for deduping CDN variants of the same file."""
    return urlparse(url).path.rsplit("/", 1)[-1]


def _parse_html(html_content: str):
    """Parse Facebook HTML defensively enough for script/meta extraction."""
    try:
        return html.fromstring(html_content, parser=_JSON_PARSER)
    except (ParserError, ValueError) as e:
        logger.debug("Failed to parse Facebook HTML: %r.", e)
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
            logger.debug("Skipping malformed Facebook JSON script.")
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


def _story_photo_candidate(node: dict[str, Any]) -> MediaCandidate | None:
    """Extract a photo candidate from a scoped story subtree."""
    if node.get("__typename") != "Photo":
        return None

    media_url = None
    for image_key in ("viewer_image", "photo_image", "image", "massive_image"):
        image = node.get(image_key)
        if isinstance(image, dict):
            media_url = _clean_url(image.get("uri"))
            if media_url:
                break

    if not media_url:
        return None

    media_id = node.get("id")
    return MediaCandidate(
        id=str(media_id) if media_id is not None else None,
        url=media_url,
        thumbnail=media_url,
    )


def _extract_scoped_story_photo_candidates(node: dict[str, Any]) -> list[MediaCandidate]:
    """Extract all photo media from an already scoped story subtree."""
    candidates = []
    for child in _walk_json(node):
        if isinstance(child, dict) and (candidate := _story_photo_candidate(child)):
            candidates.append(candidate)
    return candidates


def _find_story_album_info(documents: list[Any], url: str) -> StoryAlbumInfo | None:
    """Find an album attachment that advertises more items than are embedded."""
    story_token = _url_story_token(url)
    if not story_token:
        return None

    for document in documents:
        for node in _walk_json(document):
            if not isinstance(node, dict):
                continue
            album_token = node.get("mediaset_token")
            subattachments = node.get("all_subattachments")
            if not isinstance(album_token, str) or not isinstance(subattachments, dict):
                continue
            if story_token not in str(node.get("url") or ""):
                continue
            count = subattachments.get("count")
            nodes = subattachments.get("nodes") or []
            if isinstance(count, int) and isinstance(nodes, list) and count > len(nodes):
                return StoryAlbumInfo(album_token, count)
    return None


def _queries_for_url(url: str) -> tuple[Any, ...]:
    """Select the narrowest structured media queries for a Facebook URL."""
    match _page_kind(url):
        case "reel":
            return _REEL_QUERIES
        case "video":
            return _VIDEO_QUERIES
        case "photo":
            return _PHOTO_QUERIES
        case "story_card":
            return _STORY_CARD_QUERIES
        case _:
            return _STORY_ATTACHMENT_QUERIES


def _extract_watch_video_candidate(documents: list[Any], target_id: str) -> MediaCandidate | None:
    """Extract /watch/?v= media from DASH prefetch data for the current video page."""
    if scoped_candidate := _extract_video_playback_candidate(documents, target_id):
        return scoped_candidate

    best_url = None
    best_bandwidth = -1
    thumbnail = None
    caption = None

    for document in documents:
        for node in _walk_json(document):
            if not isinstance(node, dict):
                continue

            # Endpoint: /watch/?v={id}
            # JSON shape: all_video_dash_prefetch_representations[].representations[]
            # This extension is emitted for the currently loaded watch video. Choose the
            # highest-bandwidth mp4 BaseURL because regular browser_native fields are absent.
            for prefetch in node.get("all_video_dash_prefetch_representations") or []:
                if not isinstance(prefetch, dict):
                    continue
                for representation in prefetch.get("representations") or []:
                    if not isinstance(representation, dict):
                        continue
                    if representation.get("mime_type") != "video/mp4":
                        continue
                    media_url = _clean_url(representation.get("base_url"))
                    if not media_url:
                        continue
                    bandwidth = representation.get("bandwidth")
                    bandwidth = bandwidth if isinstance(bandwidth, int) else 0
                    if bandwidth > best_bandwidth:
                        best_url = media_url
                        best_bandwidth = bandwidth

            if str(node.get("id")) == target_id:
                thumbnail = thumbnail or _clean_url(_WATCH_THUMBNAIL_QUERY.search(node))
                caption = caption or _clean_text(_WATCH_CAPTION_QUERY.search(node))

    if not best_url:
        return None
    return MediaCandidate(id=target_id, url=best_url, thumbnail=thumbnail, caption=caption)


def _extract_video_playback_candidate(documents: list[Any], target_id: str) -> MediaCandidate | None:
    """Extract direct playback URLs from a scoped Facebook Video node."""
    for document in documents:
        for node in _walk_json(document):
            if isinstance(node, dict) and str(node.get("id")) == target_id:
                if candidate := _extract_video_playback_from_node(node, target_id):
                    return candidate
    return None


def _extract_video_playback_from_node(node: dict[str, Any], target_id: str) -> MediaCandidate | None:
    """Extract progressive or DASH playback URLs from a Facebook Video subtree."""
    thumbnail = _clean_url(_WATCH_THUMBNAIL_QUERY.search(node))
    caption = _extract_json_text([node], _VIDEO_NODE_CAPTION_QUERIES)

    progressive_url = None
    progressive_score = -1
    dash_url = None
    dash_bandwidth = -1
    for child in _walk_json(node):
        if not isinstance(child, dict):
            continue

        if media_url := _clean_url(child.get("progressive_url")):
            score = _progressive_quality_score(child)
            if score > progressive_score:
                progressive_url = media_url
                progressive_score = score

        if child.get("mime_type") == "video/mp4" and (media_url := _clean_url(child.get("base_url"))):
            bandwidth = child.get("bandwidth")
            bandwidth = bandwidth if isinstance(bandwidth, int) else 0
            if bandwidth > dash_bandwidth:
                dash_url = media_url
                dash_bandwidth = bandwidth

    media_url = progressive_url or dash_url
    if not media_url:
        return None
    return MediaCandidate(id=target_id, url=media_url, thumbnail=thumbnail, caption=caption)


def _progressive_quality_score(node: dict[str, Any]) -> int:
    """Rank Facebook progressive video variants."""
    metadata = node.get("metadata")
    quality = metadata.get("quality") if isinstance(metadata, dict) else None
    if quality == "HD":
        return 2
    if quality == "SD":
        return 1
    return 0


def _extract_media_candidates(
    documents: list[Any],
    url: str,
    story_tokens: tuple[str, ...] = (),
) -> list[MediaCandidate]:
    """Extract page media from parsed Facebook frontend JSON documents."""
    kind = _page_kind(url)
    target_id = _url_media_id(url, kind)
    target_story_tokens = tuple(
        dict.fromkeys(token for token in ((_url_story_token(url), *story_tokens) if kind == "story" else ()) if token)
    )
    require_id_match = kind in {"reel", "video", "photo", "story_card"}
    if require_id_match and not target_id:
        logger.debug("Refusing %s extraction without a URL media ID: %s.", kind, url)
        return []
    if kind == "watch_video":
        if not target_id:
            logger.debug("Refusing watch extraction without a URL video ID: %s.", url)
            return []
        candidate = _extract_watch_video_candidate(documents, target_id)
        return [candidate] if candidate else []

    queries = _queries_for_url(url)
    seen_urls = set()
    seen_ids = set()
    candidates: list[MediaCandidate] = []
    for document in documents:
        for node in _walk_json(document):
            if kind == "story" and target_story_tokens:
                if not isinstance(node, dict) or not any(
                    _node_contains_story_token(node, token) for token in target_story_tokens
                ):
                    continue
            if kind == "story_card" and (not isinstance(node, dict) or node.get("id") != target_id):
                continue
            for query in queries:
                for raw in _iter_result_items(query.search(node)):
                    candidate = _media_candidate(raw)
                    if not candidate:
                        continue
                    if require_id_match and kind != "story_card" and candidate.id != target_id:
                        continue
                    if candidate.url in seen_urls:
                        continue
                    if candidate.id:
                        seen_ids.add(candidate.id)
                    seen_urls.add(candidate.url)
                    candidates.append(candidate)
    if not candidates and kind in {"reel", "video"} and target_id:
        candidate = _extract_video_playback_candidate(documents, target_id)
        return [candidate] if candidate else []
    return candidates


def _extract_album_candidates(html_content: str, expected_count: int) -> list[MediaCandidate]:
    """Extract photo candidates from a dedicated Facebook mediaset page."""
    tree = _parse_html(html_content)
    if tree is None:
        return []

    documents = _script_json(tree, _MEDIA_SCRIPT_XPATH)
    seen_ids = set()
    seen_urls = set()
    candidates = []
    for document in documents:
        for candidate in _extract_scoped_story_photo_candidates(document):
            if candidate.id and candidate.id in seen_ids:
                continue
            if candidate.url in seen_urls:
                continue
            if candidate.id:
                seen_ids.add(candidate.id)
            seen_urls.add(candidate.url)
            candidates.append(candidate)
            if len(candidates) >= expected_count:
                return candidates
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


def _format_profile_caption(name: str | None, description: str | None) -> str | None:
    """Build a concise profile caption from OpenGraph metadata."""
    if not name and not description:
        return None
    if not description:
        return name

    bio = description
    if name:
        prefix = f"{name}."
        if bio.startswith(prefix):
            bio = bio[len(prefix) :].strip()
    return "\n\n".join(part for part in (name, bio) if part)


def _extract_facebook_profile(tree, url: str) -> MediaResult | None:
    """Extract public profile name, bio, and profile picture from OpenGraph."""
    app_url = _extract_meta_content(tree, "al:ios:url") or _extract_meta_content(tree, "al:android:url")
    if not app_url or not app_url.startswith("fb://profile/"):
        return None

    name = _extract_meta_content(tree, "og:title")
    description = _extract_meta_content(tree, "og:description")
    picture = _extract_meta_content(tree, "og:image")
    if not picture:
        return None

    return MediaResult(
        urls=(picture,),
        metadata=MediaMetadata(
            original_url=url,
            thumbnail=picture,
            caption=_format_profile_caption(name, description),
            title=name,
        ),
    )


def _extract_facebook_media(html_content: str, url: str, warn_missing: bool = True) -> MediaResult | None:
    """Extract media and metadata from a fetched Facebook page."""
    tree = _parse_html(html_content)
    if tree is None:
        return None

    if _is_profile_url(url) and (profile_result := _extract_facebook_profile(tree, url)):
        return profile_result

    media_documents = _script_json(tree, _MEDIA_SCRIPT_XPATH)
    route_documents = _script_json(tree, _ROUTE_SCRIPT_XPATH)
    candidates = _extract_media_candidates(media_documents, url, story_tokens=_route_story_tokens(route_documents))
    if not candidates:
        if warn_missing:
            logger.warning("No structured Facebook media found for %s.", url)
        return None

    thumbnail = next((candidate.thumbnail for candidate in candidates if candidate.thumbnail), None)
    thumbnail = thumbnail or _extract_meta_content(tree, "og:image")

    title = _extract_route_title(route_documents) or _extract_meta_content(tree, "og:title")

    caption = next((candidate.caption for candidate in candidates if candidate.caption), None)
    caption = caption or _extract_json_text(media_documents, _CAPTION_QUERIES)
    caption = caption or _extract_meta_content(tree, "og:description")

    return MediaResult(
        urls=tuple(candidate.url for candidate in candidates),
        metadata=MediaMetadata(
            original_url=url,
            thumbnail=thumbnail,
            caption=caption,
            title=title,
        ),
    )


async def _fetch_facebook(
    client: httpx.AsyncClient,
    url: str,
    cookies: httpx.Cookies | None = None,
    warn_missing: bool = True,
) -> MediaResult | None:
    """Fetch a Facebook URL and extract public media from structured JSON."""
    current_url = _normalize_facebook_url(url)

    for redirect_count in range(_MAX_REDIRECTS + 1):
        if not _is_facebook_domain(current_url):
            logger.warning("Aborting request to non-Facebook domain: %s.", _safe_log_url(current_url))
            return None

        response = await client.get(current_url, headers=FACEBOOK_HEADERS, cookies=cookies)
        if not response.is_redirect:
            response.raise_for_status()
            final_url = _normalize_facebook_url(str(response.url))
            if cookies and _is_login_url(final_url):
                raise FacebookAuthExpired("Facebook authenticated session expired")
            result = _extract_facebook_media(response.text, final_url, warn_missing=warn_missing)
            if result and cookies:
                return await _expand_story_album_if_needed(client, result, response.text, final_url, cookies)
            return result

        if redirect_count == _MAX_REDIRECTS:
            logger.error("Too many Facebook redirects for %s.", _safe_log_url(url))
            return None

        location = response.headers.get("Location")
        if not location:
            logger.warning("Facebook redirect without Location for %s.", _safe_log_url(current_url))
            return None

        current_url = _normalize_facebook_url(str(response.url.join(location)))
        if cookies and _is_login_url(current_url):
            raise FacebookAuthExpired("Facebook authenticated session expired")

    return None


async def _expand_story_album_if_needed(
    client: httpx.AsyncClient,
    result: MediaResult,
    html_content: str,
    url: str,
    cookies: httpx.Cookies,
) -> MediaResult:
    """Fetch a dedicated mediaset page when the story HTML embeds a partial album."""
    if _page_kind(url) != "story":
        return result

    tree = _parse_html(html_content)
    if tree is None:
        return result

    documents = _script_json(tree, _MEDIA_SCRIPT_XPATH)
    album_info = _find_story_album_info(documents, url)
    if not album_info or len(result.urls) >= album_info.count:
        return result

    album_url = f"https://www.facebook.com/media/set/?set={album_info.token}&type=3"
    try:
        response = await client.get(album_url, headers=FACEBOOK_HEADERS, cookies=cookies)
        response.raise_for_status()
    except Exception as e:
        logger.debug("Facebook album expansion fetch failed for %s: %r.", _safe_log_url(url), e)
        return result

    final_url = _normalize_facebook_url(str(response.url))
    if _is_login_url(final_url):
        raise FacebookAuthExpired("Facebook authenticated session expired")

    album_candidates = _extract_album_candidates(response.text, album_info.count)
    if len(album_candidates) <= len(result.urls):
        return result

    seen_keys = {_media_file_key(media_url) for media_url in result.urls}
    expanded_urls = list(result.urls)
    for candidate in album_candidates:
        key = _media_file_key(candidate.url)
        if key in seen_keys:
            continue
        seen_keys.add(key)
        expanded_urls.append(candidate.url)
        if len(expanded_urls) >= album_info.count:
            break

    if len(expanded_urls) > len(result.urls):
        logger.info(
            "Expanded Facebook story album for %s from %d to %d media.",
            _safe_log_url(url),
            len(result.urls),
            len(expanded_urls),
        )
        return MediaResult(urls=tuple(expanded_urls), metadata=result.metadata)
    return result


class FacebookExtractor(MediaExtractor):
    """Extract direct media URLs from public Facebook posts, reels, photos, and videos."""

    name = "facebook"
    url_pattern = RE_FACEBOOK

    def _validate_url(self, url: str) -> bool:
        """Validate Facebook domain."""
        return _is_facebook_domain(url)

    async def _extract_media(self, url: str) -> MediaResult | None:
        """Extract media from a public Facebook URL."""
        log_url = _safe_log_url(url)
        try:
            client = get_client()
            if not client:
                async with httpx.AsyncClient(
                    follow_redirects=False,
                    timeout=HTTP_TIMEOUT,
                    http2=True,
                ) as temp_client:
                    return await self._extract_with_fallback(temp_client, url)
            return await self._extract_with_fallback(client, url)
        except httpx.HTTPError as e:
            logger.error("HTTP error accessing %s: %r.", log_url, e)
        except Exception as e:
            logger.error("Unexpected error extracting Facebook media from %s: %r.", log_url, e)
        return None

    async def _extract_with_fallback(self, client: httpx.AsyncClient, url: str) -> MediaResult | None:
        """Try authenticated cookies first when configured, then public fetch."""
        log_url = _safe_log_url(url)
        if facebook_auth_available():
            try:
                logger.debug("Trying authenticated Facebook fetch for %s.", log_url)
                cookies = await get_facebook_cookies()
                result = await _fetch_facebook(client, url, cookies=cookies, warn_missing=False)
                if result:
                    logger.info("Fetched %d Facebook media with saved auth from %s.", len(result.urls), log_url)
                    return result
            except FacebookAuthExpired:
                logger.info("Facebook session expired while fetching %s; refreshing it.", log_url)
                try:
                    cookies = await get_facebook_cookies(force_refresh=True)
                    result = await _fetch_facebook(client, url, cookies=cookies, warn_missing=False)
                    if result:
                        logger.info("Fetched %d Facebook media after auth refresh from %s.", len(result.urls), log_url)
                        return result
                except Exception as e:
                    logger.warning("Facebook auth refresh failed; falling back to public fetch: %r.", e)
            except Exception as e:
                logger.warning("Facebook authenticated fetch failed; falling back to public fetch: %r.", e)

            try:
                logger.debug("Trying public Facebook fallback for %s.", log_url)
                result = await _fetch_facebook(client, url)
                if result:
                    logger.info("Fetched %d Facebook media with public fallback from %s.", len(result.urls), log_url)
                    return result
                return None
            except Exception as e:
                logger.warning("Facebook public fallback failed after auth miss: %r.", e)
                return None

        return await _fetch_facebook(client, url)
