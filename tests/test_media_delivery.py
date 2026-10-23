"""Regression tests for media download limits and Telegram upload timeouts."""

import os
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import httpx

from config import TELEGRAM_MAX_MEDIA_BYTES
from services import media_delivery
from services.media_delivery import (
    TELEGRAM_UPLOAD_TIMEOUT,
    DownloadedMedia,
    MediaTooLargeError,
    _download_limit,
    _fit_to_upload_limit,
    _raise_if_content_too_large,
    _upload_write_timeout,
    deliver_media,
    download_media,
)

_MIB = 1024 * 1024


def _message(**methods) -> SimpleNamespace:
    return SimpleNamespace(chat_id=1, **methods)


def _temp_media(is_video: bool, size_bytes: int) -> DownloadedMedia:
    fd, path = tempfile.mkstemp(suffix=".mp4" if is_video else ".jpg")
    os.write(fd, b"x")
    os.close(fd)
    return DownloadedMedia(path, is_video, size_bytes)


class UploadTimeoutTests(unittest.TestCase):
    def test_small_uploads_keep_the_default_timeout(self):
        self.assertEqual(_upload_write_timeout(5 * _MIB), TELEGRAM_UPLOAD_TIMEOUT)

    def test_large_uploads_get_time_proportional_to_their_size(self):
        timeout = _upload_write_timeout(50 * _MIB)

        self.assertGreater(timeout, TELEGRAM_UPLOAD_TIMEOUT)
        self.assertEqual(timeout, 50 * _MIB / media_delivery.MIN_UPLOAD_BYTES_PER_SECOND)


class DeliverMediaTests(unittest.IsolatedAsyncioTestCase):
    async def test_large_video_upload_gets_a_size_scaled_write_timeout(self):
        media = _temp_media(is_video=True, size_bytes=48 * _MIB)
        reply_video = AsyncMock()
        message = _message(reply_video=reply_video)

        with patch.object(media_delivery, "download_media", AsyncMock(return_value=media)):
            delivered = await deliver_media(message, ["https://cdn.example/reel.mp4"], None, None)

        self.assertTrue(delivered)
        kwargs = reply_video.await_args.kwargs
        self.assertEqual(kwargs["read_timeout"], TELEGRAM_UPLOAD_TIMEOUT)
        self.assertEqual(kwargs["write_timeout"], _upload_write_timeout(48 * _MIB))
        self.assertGreater(kwargs["write_timeout"], TELEGRAM_UPLOAD_TIMEOUT)
        self.assertFalse(os.path.exists(media.path))

    async def test_album_write_timeout_covers_every_file_in_the_upload(self):
        media = [_temp_media(is_video=True, size_bytes=30 * _MIB), _temp_media(is_video=True, size_bytes=30 * _MIB)]
        reply_media_group = AsyncMock()
        message = _message(reply_media_group=reply_media_group)

        with patch.object(media_delivery, "download_media", AsyncMock(side_effect=media)):
            await deliver_media(message, ["https://cdn.example/a.mp4", "https://cdn.example/b.mp4"], None, None)

        self.assertEqual(reply_media_group.await_args.kwargs["write_timeout"], _upload_write_timeout(60 * _MIB))

    async def test_oversized_media_is_skipped_with_a_warning_not_an_error(self):
        too_large = MediaTooLargeError("media is 63.4 MiB; limit is 50.0 MiB")
        reply_video = AsyncMock()
        message = _message(reply_video=reply_video)

        with (
            patch.object(media_delivery, "download_media", AsyncMock(side_effect=too_large)),
            self.assertLogs("services.media_delivery", level="WARNING") as logs,
        ):
            delivered = await deliver_media(message, ["https://cdn.example/reel.mp4"], None, None)

        self.assertFalse(delivered)
        reply_video.assert_not_awaited()
        self.assertEqual(
            [(record.levelname, record.getMessage()) for record in logs.records],
            [
                ("WARNING", "Skipping media 1/1 from cdn.example: media is 63.4 MiB; limit is 50.0 MiB."),
                ("WARNING", "No media files were downloaded; skipping Telegram upload."),
            ],
        )


class CompressionTests(unittest.IsolatedAsyncioTestCase):
    async def test_video_over_the_upload_limit_is_compressed_then_uploaded(self):
        original = _temp_media(is_video=True, size_bytes=TELEGRAM_MAX_MEDIA_BYTES + 5 * _MIB)
        shrunk = _temp_media(is_video=True, size_bytes=0)
        reply_video = AsyncMock()
        message = _message(reply_video=reply_video)

        with (
            patch.object(media_delivery, "download_media", AsyncMock(return_value=original)),
            patch.object(media_delivery.video_compression, "shrink_video", AsyncMock(return_value=shrunk.path)),
        ):
            delivered = await deliver_media(message, ["https://cdn.example/reel.mp4"], None, None)

        self.assertTrue(delivered)
        self.assertEqual(reply_video.await_args.kwargs["video"].name, shrunk.path)
        self.assertFalse(os.path.exists(original.path))
        self.assertFalse(os.path.exists(shrunk.path))

    async def test_video_within_the_upload_limit_is_not_touched(self):
        media = _temp_media(is_video=True, size_bytes=TELEGRAM_MAX_MEDIA_BYTES)
        shrink = AsyncMock()

        with patch.object(media_delivery.video_compression, "shrink_video", shrink):
            result = await _fit_to_upload_limit(media)

        self.assertIs(result, media)
        shrink.assert_not_awaited()
        os.remove(media.path)

    async def test_video_that_cannot_be_compressed_is_skipped_and_cleaned_up(self):
        original = _temp_media(is_video=True, size_bytes=TELEGRAM_MAX_MEDIA_BYTES + 1)
        reply_video = AsyncMock()
        message = _message(reply_video=reply_video)

        with (
            patch.object(media_delivery, "download_media", AsyncMock(return_value=original)),
            patch.object(media_delivery.video_compression, "shrink_video", AsyncMock(return_value=None)),
            self.assertLogs("services.media_delivery", level="WARNING") as logs,
        ):
            delivered = await deliver_media(message, ["https://cdn.example/reel.mp4"], None, None)

        self.assertFalse(delivered)
        reply_video.assert_not_awaited()
        self.assertIn("could not be compressed", logs.records[0].getMessage())
        self.assertFalse(os.path.exists(original.path))


class DownloadLimitTests(unittest.IsolatedAsyncioTestCase):
    def test_videos_may_exceed_the_upload_limit_only_when_ffmpeg_can_shrink_them(self):
        with patch.object(media_delivery.video_compression, "is_available", return_value=True):
            self.assertGreater(_download_limit(is_video=True), TELEGRAM_MAX_MEDIA_BYTES)
            self.assertEqual(_download_limit(is_video=False), TELEGRAM_MAX_MEDIA_BYTES)
        with patch.object(media_delivery.video_compression, "is_available", return_value=False):
            self.assertEqual(_download_limit(is_video=True), TELEGRAM_MAX_MEDIA_BYTES)

    async def test_download_accepts_an_oversized_video_only_when_ffmpeg_is_available(self):
        def serve(request: httpx.Request) -> httpx.Response:
            return httpx.Response(200, content=b"x" * 150, headers={"Content-Type": "video/mp4"})

        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch.multiple(
                media_delivery, TEMP_DIR=temp_dir, TELEGRAM_MAX_MEDIA_BYTES=100, TELEGRAM_MAX_SOURCE_VIDEO_BYTES=1000
            ),
        ):
            async with httpx.AsyncClient(transport=httpx.MockTransport(serve)) as client:
                with patch.object(media_delivery.video_compression, "is_available", return_value=True):
                    media = await download_media("https://cdn.example/reel.mp4", client)
                    self.assertEqual(media.size_bytes, 150)
                with patch.object(media_delivery.video_compression, "is_available", return_value=False):
                    with self.assertRaises(MediaTooLargeError):
                        await download_media("https://cdn.example/reel.mp4", client)
            self.assertEqual(os.listdir(temp_dir), [os.path.basename(media.path)], "failed download leaks no file")


class DeliveryLogTests(unittest.IsolatedAsyncioTestCase):
    async def test_delivery_reports_size_and_phase_timings(self):
        media = _temp_media(is_video=True, size_bytes=12 * _MIB)
        message = _message(reply_video=AsyncMock())

        with (
            patch.object(media_delivery, "download_media", AsyncMock(return_value=media)),
            self.assertLogs("services.media_delivery", level="INFO") as logs,
        ):
            await deliver_media(message, ["https://cdn.example/reel.mp4"], None, None)

        (line,) = [record.getMessage() for record in logs.records if record.levelname == "INFO"]
        self.assertRegex(
            line, r"^Delivered 1 media item\(s\) \(12\.0 MiB\) to chat 1: download \d+\.\ds, upload \d+\.\ds\.$"
        )

    async def test_download_failure_names_the_host_and_http_status(self):
        request = httpx.Request("GET", "https://cdn.example/reel.mp4?sig=SECRET")
        forbidden = httpx.HTTPStatusError("403", request=request, response=httpx.Response(403, request=request))
        message = _message(reply_video=AsyncMock())

        with (
            patch.object(media_delivery, "download_media", AsyncMock(side_effect=forbidden)),
            self.assertLogs("services.media_delivery", level="ERROR") as logs,
        ):
            await deliver_media(message, ["https://cdn.example/reel.mp4?sig=SECRET"], None, None)

        self.assertEqual(
            logs.records[0].getMessage(), "Failed to download media 1/1 from cdn.example: HTTPStatusError (403)."
        )

    async def test_signed_media_urls_never_reach_the_logs_even_at_debug(self):
        media = _temp_media(is_video=True, size_bytes=_MIB)
        message = _message(reply_video=AsyncMock())

        with (
            patch.object(media_delivery, "download_media", AsyncMock(return_value=media)),
            self.assertLogs("services.media_delivery", level="DEBUG") as logs,
        ):
            await deliver_media(message, ["https://cdn.example/reel.mp4?oh=SIGNATURE&uri=SECRET"], None, None)

        self.assertFalse(
            any("SIGNATURE" in record.getMessage() or "SECRET" in record.getMessage() for record in logs.records)
        )


class ContentLengthTests(unittest.TestCase):
    def test_oversized_content_length_reports_both_sizes(self):
        response = httpx.Response(200, headers={"Content-Length": str(TELEGRAM_MAX_MEDIA_BYTES + 10 * _MIB)})

        with self.assertRaises(MediaTooLargeError) as raised:
            _raise_if_content_too_large(response)

        self.assertIn(f"{TELEGRAM_MAX_MEDIA_BYTES / _MIB + 10:.1f} MiB", str(raised.exception))
        self.assertIn(f"{TELEGRAM_MAX_MEDIA_BYTES / _MIB:.1f} MiB", str(raised.exception))

    def test_content_length_within_the_limit_passes(self):
        _raise_if_content_too_large(httpx.Response(200, headers={"Content-Length": str(5 * _MIB)}))


if __name__ == "__main__":
    unittest.main()
