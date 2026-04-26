"""Telegram media download and delivery helpers."""

import logging
import os
import uuid
from collections.abc import Sequence
from dataclasses import dataclass
from urllib.parse import parse_qs, unquote, urlparse

import httpx
from telegram import InputMediaPhoto, InputMediaVideo, Message
from telegram.error import BadRequest

from config import HTTP_TIMEOUT, TELEGRAM_MAX_MEDIA_BYTES
from services.http import get_client

logger = logging.getLogger(__name__)

TEMP_DIR = "/tmp/fx-telebot/"
MEDIA_GROUP_LIMIT = 10
TELEGRAM_UPLOAD_TIMEOUT = 120.0
DOWNLOAD_ATTEMPTS = 3


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

    try:
        logger.info("Preparing to deliver %d media item(s)", len(urls))
        for index, media_url in enumerate(urls, start=1):
            try:
                logger.debug("Downloading media %d/%d from %s", index, len(urls), media_url)
                media_file = await download_media(media_url, client)
                media_files.append(media_file)
                logger.debug(
                    "Downloaded media %d/%d from %s as %s (%d bytes)",
                    index,
                    len(urls),
                    media_url,
                    "video" if media_file.is_video else "photo",
                    media_file.size_bytes,
                )
            except Exception as e:
                logger.error("Failed to download media %d/%d from %s: %r", index, len(urls), media_url, e)

        if not media_files:
            logger.info("No media files downloaded; skipping Telegram upload")
            return False

        await reply_with_media(message, media_files, caption, reply_to, parse_mode=parse_mode)
        logger.info("Delivered %d media item(s) to Telegram", len(media_files))
        return True
    finally:
        for media_file in media_files:
            if os.path.exists(media_file.path):
                try:
                    os.remove(media_file.path)
                    logger.debug("Deleted temp media file")
                except Exception as e:
                    logger.warning("Failed to delete temp media file: %r", e)


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
            logger.info("Retrying media download through proxy origin URL")
        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            try:
                return await _download_media_once(media_url, client)
            except httpx.HTTPError as e:
                last_error = e
                if attempt < DOWNLOAD_ATTEMPTS:
                    logger.info("Media download attempt %d/%d failed: %s", attempt, DOWNLOAD_ATTEMPTS, type(e).__name__)
                    continue
                logger.info("Media download attempts exhausted for candidate URL: %s", type(e).__name__)
    if last_error:
        raise last_error
    raise RuntimeError("No media download URLs provided")


async def _download_media_once(media_url: str, client: httpx.AsyncClient) -> DownloadedMedia:
    """Stream one media URL attempt to a temp file."""
    file_path: str | None = None
    try:
        size_bytes = 0
        async with client.stream("GET", media_url, follow_redirects=True) as response:
            response.raise_for_status()
            _raise_if_content_too_large(response)
            content_type = response.headers.get("Content-Type", "")
            is_video = is_video_url(media_url, content_type)
            ext = ".mp4" if is_video else ".jpg"
            file_path = os.path.join(TEMP_DIR, f"{uuid.uuid4()}{ext}")
            with open(file_path, "wb") as output:
                async for chunk in response.aiter_bytes():
                    size_bytes += len(chunk)
                    if size_bytes > TELEGRAM_MAX_MEDIA_BYTES:
                        raise MediaTooLargeError(f"media exceeds configured limit of {TELEGRAM_MAX_MEDIA_BYTES} bytes")
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
                logger.info("Reply target disappeared; sending media without reply target")
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
                    write_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                )
            except BadRequest as e:
                if reply_to is not None and _reply_target_missing(e):
                    logger.info("Reply target disappeared; sending media group without reply target")
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
                    logger.warning("Failed to close media file: %r", e)
        first_chunk = False


async def _reply_with_single_media(
    message: Message,
    media_file: DownloadedMedia,
    caption: str | None,
    reply_to: int | None,
    parse_mode: str | None,
) -> None:
    with open(media_file.path, "rb") as media_handle:
        if media_file.is_video:
            await message.reply_video(
                video=media_handle,
                caption=caption,
                parse_mode=parse_mode,
                supports_streaming=True,
                reply_to_message_id=reply_to,
                read_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                write_timeout=TELEGRAM_UPLOAD_TIMEOUT,
            )
        else:
            await message.reply_photo(
                photo=media_handle,
                caption=caption,
                parse_mode=parse_mode,
                reply_to_message_id=reply_to,
                read_timeout=TELEGRAM_UPLOAD_TIMEOUT,
                write_timeout=TELEGRAM_UPLOAD_TIMEOUT,
            )


def is_video_url(url: str, content_type: str = "") -> bool:
    """Detect videos from response metadata or the direct URL."""
    lowered_type = content_type.lower()
    if lowered_type.startswith("video/"):
        return True
    lowered_url = unquote(url).lower()
    return ".mp4" in lowered_url or "video" in lowered_url


def _raise_if_content_too_large(response: httpx.Response) -> None:
    content_length = response.headers.get("Content-Length")
    if not content_length:
        return
    try:
        size_bytes = int(content_length)
    except ValueError:
        return
    if size_bytes > TELEGRAM_MAX_MEDIA_BYTES:
        raise MediaTooLargeError(f"media is {size_bytes} bytes; limit is {TELEGRAM_MAX_MEDIA_BYTES} bytes")


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
