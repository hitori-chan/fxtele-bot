"""Telegram media download and delivery helpers."""

import logging
import os
import time
import uuid
from collections.abc import Sequence
from dataclasses import dataclass
from urllib.parse import parse_qs, unquote, urlparse

import httpx
from telegram import InputMediaPhoto, InputMediaVideo, Message
from telegram.error import BadRequest

from config import HTTP_TIMEOUT, TELEGRAM_MAX_MEDIA_BYTES, TELEGRAM_MAX_SOURCE_VIDEO_BYTES, USER_AGENT
from services import video_compression
from services.http import get_client
from utils.error_text import describe_error

logger = logging.getLogger(__name__)

TEMP_DIR = "/tmp/fx-telebot/"
MEDIA_GROUP_LIMIT = 10
TELEGRAM_UPLOAD_TIMEOUT = 120.0
# httpx sends each uploaded file as a single chunk, so the write timeout bounds
# the whole upload rather than one stall. Large videos need a budget that grows
# with their size or a slow uplink times out mid-upload.
MIN_UPLOAD_BYTES_PER_SECOND = 128 * 1024
DOWNLOAD_ATTEMPTS = 3
# CDNs commonly reject the default httpx User-Agent; identify as a browser.
_DOWNLOAD_HEADERS = {"User-Agent": USER_AGENT}


class MediaTooLargeError(ValueError):
    """Raised when a media response exceeds the configured download cap."""


@dataclass(frozen=True)
class DownloadedMedia:
    """Downloaded media file and inferred Telegram type."""

    path: str
    is_video: bool
    size_bytes: int


async def deliver_media(
    message: Message,
    urls: Sequence[str],
    caption: str | None,
    reply_to: int | None,
    parse_mode: str | None = None,
) -> bool:
    """Download media URLs, upload them to Telegram, and clean up temp files."""
    os.makedirs(TEMP_DIR, exist_ok=True)
    client = get_client()
    media_files: list[DownloadedMedia] = []
    download_seconds = 0.0

    try:
        logger.debug("Preparing to deliver %d media item(s).", len(urls))
        for index, media_url in enumerate(urls, start=1):
            host = _host(media_url)
            try:
                logger.debug("Downloading media %d/%d from %s.", index, len(urls), host)
                download_started = time.monotonic()
                media_file = await download_media(media_url, client)
                download_seconds += time.monotonic() - download_started
                media_file = await _fit_to_upload_limit(media_file)
                media_files.append(media_file)
                logger.debug(
                    "Downloaded media %d/%d from %s as %s (%s).",
                    index,
                    len(urls),
                    host,
                    "video" if media_file.is_video else "photo",
                    _format_mib(media_file.size_bytes),
                )
            except MediaTooLargeError as e:
                logger.warning("Skipping media %d/%d from %s: %s.", index, len(urls), host, e)
            except Exception as e:
                logger.error("Failed to download media %d/%d from %s: %s.", index, len(urls), host, describe_error(e))

        if not media_files:
            logger.warning("No media files were downloaded; skipping Telegram upload.")
            return False

        upload_started = time.monotonic()
        await reply_with_media(message, media_files, caption, reply_to, parse_mode=parse_mode)
        logger.info(
            "Delivered %d media item(s) (%s) to chat %s: download %.1fs, upload %.1fs.",
            len(media_files),
            _format_mib(sum(media_file.size_bytes for media_file in media_files)),
            message.chat_id,
            download_seconds,
            time.monotonic() - upload_started,
        )
        return True
    finally:
        for media_file in media_files:
            if os.path.exists(media_file.path):
                try:
                    os.remove(media_file.path)
                    logger.debug("Deleted temp media file %s.", media_file.path)
                except Exception as e:
                    logger.warning("Failed to delete temp media file %s: %s.", media_file.path, describe_error(e))


async def download_media(media_url: str, client: httpx.AsyncClient | None = None) -> DownloadedMedia:
    """Stream a media URL to a temp file and return its local path and type."""
    os.makedirs(TEMP_DIR, exist_ok=True)
    request_client = client
    close_client = False
    if request_client is None:
        request_client = httpx.AsyncClient(timeout=HTTP_TIMEOUT)
        close_client = True

    try:
        urls = [media_url]
        if fallback_url := _proxy_origin_url(media_url):
            urls.append(fallback_url)
        return await _download_media_with_retries(urls, request_client)
    finally:
        if close_client:
            await request_client.aclose()


async def _download_media_with_retries(urls: Sequence[str], client: httpx.AsyncClient) -> DownloadedMedia:
    """Try each candidate URL a few times before giving up."""
    last_error: Exception | None = None
    for url_index, media_url in enumerate(urls):
        if url_index:
            logger.debug("Retrying media download through proxy origin URL.")
        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            try:
                return await _download_media_once(media_url, client)
            except httpx.HTTPError as e:
                last_error = e
                if attempt < DOWNLOAD_ATTEMPTS:
                    logger.debug(
                        "Media download attempt %d/%d failed: %s.", attempt, DOWNLOAD_ATTEMPTS, describe_error(e)
                    )
                    continue
                logger.debug("Media download attempts exhausted for candidate URL: %s.", describe_error(e))
    if last_error:
        raise last_error
    raise RuntimeError("No media download URLs provided")


async def _download_media_once(media_url: str, client: httpx.AsyncClient) -> DownloadedMedia:
    """Stream one media URL attempt to a temp file."""
    file_path: str | None = None
    try:
        size_bytes = 0
        async with client.stream("GET", media_url, headers=_DOWNLOAD_HEADERS, follow_redirects=True) as response:
            response.raise_for_status()
            content_type = response.headers.get("Content-Type", "")
            is_video = is_video_url(media_url, content_type)
            download_limit = _download_limit(is_video)
            _raise_if_content_too_large(response, download_limit)
            ext = ".mp4" if is_video else ".jpg"
            file_path = os.path.join(TEMP_DIR, f"{uuid.uuid4()}{ext}")
            with open(file_path, "wb") as output:
                async for chunk in response.aiter_bytes():
                    size_bytes += len(chunk)
                    if size_bytes > download_limit:
                        raise MediaTooLargeError(f"media exceeds the {_format_mib(download_limit)} limit")
                    output.write(chunk)
        return DownloadedMedia(file_path, is_video, size_bytes)
    except Exception:
        if file_path and os.path.exists(file_path):
            os.remove(file_path)
        raise


async def reply_with_media(
    message: Message,
    media_files: Sequence[DownloadedMedia],
    caption: str | None,
    reply_to: int | None,
    parse_mode: str | None = None,
) -> None:
    """Reply with uploaded media, grouping files into albums when possible."""
    if len(media_files) == 1:
        try:
            await _reply_with_single_media(message, media_files[0], caption, reply_to, parse_mode)
        except BadRequest as e:
            if reply_to is not None and _reply_target_missing(e):
                logger.warning("Reply target disappeared; sending media without a reply target.")
                await _reply_with_single_media(message, media_files[0], caption, None, parse_mode)
                return
            raise
        return

    first_chunk = True
    for chunk_start in range(0, len(media_files), MEDIA_GROUP_LIMIT):
        chunk = media_files[chunk_start : chunk_start + MEDIA_GROUP_LIMIT]
        if len(chunk) == 1:
            await reply_with_media(message, chunk, caption if first_chunk else None, reply_to, parse_mode=parse_mode)
            first_chunk = False
            continue

        handles = []
        try:
            media_group = []
            try:
                for index, media_file in enumerate(chunk):
                    media_handle = open(media_file.path, "rb")
                    handles.append(media_handle)
                    item_caption = caption if first_chunk and index == 0 else None
                    if media_file.is_video:
                        media_group.append(
                            InputMediaVideo(
                                media=media_handle,
                                caption=item_caption,
                                parse_mode=parse_mode,
                                supports_streaming=True,
                            )
                        )
                    else:
                        media_group.append(
                            InputMediaPhoto(media=media_handle, caption=item_caption, parse_mode=parse_mode)
                        )

                await message.reply_media_group(
                    media=media_group,
                    reply_to_message_id=reply_to,
                    read_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                    write_timeout=_upload_write_timeout(sum(media_file.size_bytes for media_file in chunk)),
                )
            except BadRequest as e:
                if reply_to is not None and _reply_target_missing(e):
                    logger.warning("Reply target disappeared; sending media group without a reply target.")
                    await reply_with_media(
                        message, chunk, caption if first_chunk else None, None, parse_mode=parse_mode
                    )
                    first_chunk = False
                    continue
                raise
        finally:
            for media_handle in handles:
                try:
                    media_handle.close()
                except Exception as e:
                    logger.warning("Failed to close media file: %s.", describe_error(e))
        first_chunk = False


async def _reply_with_single_media(
    message: Message,
    media_file: DownloadedMedia,
    caption: str | None,
    reply_to: int | None,
    parse_mode: str | None,
) -> None:
    write_timeout = _upload_write_timeout(media_file.size_bytes)
    with open(media_file.path, "rb") as media_handle:
        if media_file.is_video:
            await message.reply_video(
                video=media_handle,
                caption=caption,
                parse_mode=parse_mode,
                supports_streaming=True,
                reply_to_message_id=reply_to,
                read_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                write_timeout=write_timeout,
            )
        else:
            await message.reply_photo(
                photo=media_handle,
                caption=caption,
                parse_mode=parse_mode,
                reply_to_message_id=reply_to,
                read_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                write_timeout=write_timeout,
            )


def is_video_url(url: str, content_type: str = "") -> bool:
    """Detect videos from response metadata or the direct URL."""
    lowered_type = content_type.lower()
    if lowered_type.startswith("video/"):
        return True
    lowered_url = unquote(url).lower()
    return ".mp4" in lowered_url or "video" in lowered_url


def _raise_if_content_too_large(response: httpx.Response, limit: int = TELEGRAM_MAX_MEDIA_BYTES) -> None:
    content_length = response.headers.get("Content-Length")
    if not content_length:
        return
    try:
        size_bytes = int(content_length)
    except ValueError:
        return
    if size_bytes > limit:
        raise MediaTooLargeError(f"media is {_format_mib(size_bytes)}; limit is {_format_mib(limit)}")


def _host(url: str) -> str:
    """Host of a media URL for logs, leaving out its signed query string."""
    return urlparse(url).hostname or "<no-host>"


def _download_limit(is_video: bool) -> int:
    """Return the largest download worth fetching: videos may exceed the upload limit if ffmpeg can shrink them."""
    if is_video and video_compression.is_available():
        return max(TELEGRAM_MAX_MEDIA_BYTES, TELEGRAM_MAX_SOURCE_VIDEO_BYTES)
    return TELEGRAM_MAX_MEDIA_BYTES


async def _fit_to_upload_limit(media_file: DownloadedMedia) -> DownloadedMedia:
    """Re-encode a video that exceeds Telegram's upload limit, replacing the original temp file."""
    if media_file.size_bytes <= TELEGRAM_MAX_MEDIA_BYTES:
        return media_file

    original_size = _format_mib(media_file.size_bytes)
    logger.info(
        "Compressing %s video to fit the %s upload limit.", original_size, _format_mib(TELEGRAM_MAX_MEDIA_BYTES)
    )
    started = time.monotonic()
    try:
        shrunk_path = await video_compression.shrink_video(media_file.path, TELEGRAM_MAX_MEDIA_BYTES, TEMP_DIR)
    finally:
        _remove_temp_file(media_file.path)
    if not shrunk_path:
        raise MediaTooLargeError(
            f"media is {original_size}; limit is {_format_mib(TELEGRAM_MAX_MEDIA_BYTES)} and it could not be compressed"
        )

    shrunk_size = os.path.getsize(shrunk_path)
    logger.info(
        "Compressed video from %s to %s in %.0fs.", original_size, _format_mib(shrunk_size), time.monotonic() - started
    )
    return DownloadedMedia(shrunk_path, True, shrunk_size)


def _remove_temp_file(path: str) -> None:
    try:
        os.remove(path)
    except FileNotFoundError:
        pass


def _format_mib(size_bytes: int) -> str:
    return f"{size_bytes / (1024 * 1024):.1f} MiB"


def _upload_write_timeout(upload_bytes: int) -> float:
    """Return a write timeout long enough to upload upload_bytes at the minimum assumed uplink speed."""
    return max(TELEGRAM_UPLOAD_TIMEOUT, upload_bytes / MIN_UPLOAD_BYTES_PER_SECOND)


def _proxy_origin_url(media_url: str) -> str | None:
    """Return the origin media URL embedded in supported proxy URLs."""
    parsed = urlparse(media_url)
    if parsed.hostname != "media.anonyig.com" or parsed.path != "/get":
        return None
    origin = parse_qs(parsed.query).get("uri", [None])[0]
    if not origin:
        return None
    origin_parsed = urlparse(origin)
    if origin_parsed.scheme not in {"http", "https"}:
        return None
    return origin


def _reply_target_missing(error: BadRequest) -> bool:
    return "message to be replied not found" in str(error).lower()
