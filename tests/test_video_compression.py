"""Regression tests for shrinking oversized videos with ffmpeg."""

import asyncio
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from services import video_compression
from services.video_compression import (
    AUDIO_BITRATE,
    MIN_VIDEO_BITRATE,
    VideoInfo,
    _parse_frame_rate,
    pick_short_side,
    plan_video_bitrate,
    scale_filter,
    shrink_video,
)

_MIB = 1024 * 1024


def _encoder_available() -> bool:
    if not video_compression.is_available():
        return False
    encoders = subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True, check=False)
    return "libx264" in encoders.stdout


def _probe(path: str) -> dict:
    result = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type,width,height",
            "-of",
            "json",
            path,
        ],  # fmt: skip
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(result.stdout)


class PlanningTests(unittest.TestCase):
    def test_planned_bitrate_fits_the_limit_with_audio(self):
        duration = 60.0

        video_bitrate = plan_video_bitrate(duration, 50 * _MIB)

        self.assertIsNotNone(video_bitrate)
        self.assertLessEqual((video_bitrate + AUDIO_BITRATE) * duration / 8, 50 * _MIB)

    def test_video_too_long_for_a_watchable_bitrate_is_rejected(self):
        self.assertIsNone(plan_video_bitrate(3 * 60 * 60, 50 * _MIB))

    def test_bitrate_just_at_the_minimum_is_accepted(self):
        duration = 100.0
        max_bytes = int((MIN_VIDEO_BITRATE + AUDIO_BITRATE) * duration / 8 / video_compression.SIZE_MARGIN) + 1

        self.assertGreaterEqual(plan_video_bitrate(duration, max_bytes), MIN_VIDEO_BITRATE)

    def test_nonpositive_duration_is_rejected(self):
        self.assertIsNone(plan_video_bitrate(0, 50 * _MIB))

    def test_full_size_is_kept_while_the_bitrate_can_carry_it(self):
        self.assertIsNone(pick_short_side(VideoInfo(30, 1080, 1920, 30), 6_000_000))

    def test_picture_steps_down_as_the_bitrate_drops(self):
        reel = VideoInfo(30, 1080, 1920, 30)

        self.assertEqual(pick_short_side(reel, 2_000_000), 720)
        self.assertEqual(pick_short_side(reel, 900_000), 480)
        self.assertEqual(pick_short_side(reel, 300_000), 360)

    def test_portrait_and_landscape_are_sized_alike(self):
        for bitrate in (6_000_000, 2_000_000, 900_000, 300_000):
            self.assertEqual(
                pick_short_side(VideoInfo(30, 1080, 1920, 30), bitrate),
                pick_short_side(VideoInfo(30, 1920, 1080, 30), bitrate),
            )

    def test_high_frame_rate_needs_more_bitrate_for_the_same_size(self):
        self.assertIsNone(pick_short_side(VideoInfo(30, 1080, 1920, 30), 4_000_000))
        self.assertEqual(pick_short_side(VideoInfo(30, 1080, 1920, 60), 4_000_000), 720)

    def test_a_source_smaller_than_every_target_is_never_upscaled(self):
        self.assertIsNone(pick_short_side(VideoInfo(30, 320, 240, 30), 200_000))

    def test_scale_filter_sets_the_shorter_side_for_either_orientation(self):
        self.assertEqual(scale_filter(480), "scale='if(lt(iw,ih),480,-2)':'if(lt(iw,ih),-2,480)'")

    def test_frame_rate_parsing_tolerates_bad_probe_output(self):
        self.assertAlmostEqual(_parse_frame_rate("30000/1001"), 29.97, places=2)
        for bad in ("0/0", "garbage", None, "1000/1"):
            self.assertEqual(_parse_frame_rate(bad), video_compression.DEFAULT_FPS)


@unittest.skipUnless(_encoder_available(), "ffmpeg with libx264 is not installed")
class ShrinkVideoTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.source_dir = Path(self._tmp.name, "source")
        self.output_dir = Path(self._tmp.name, "output")
        self.source_dir.mkdir()
        self.output_dir.mkdir()

    def _make_clip(self, seconds: int = 6, size: str = "640x360") -> str:
        path = str(self.source_dir / "clip.mp4")
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-y",
                "-f", "lavfi", "-i", f"testsrc2=size={size}:rate=25:duration={seconds}",
                "-f", "lavfi", "-i", f"sine=frequency=440:duration={seconds}",
                "-c:v", "libx264", "-crf", "8", "-c:a", "aac", "-shortest",
                path,
            ],
            check=True,
        )  # fmt: skip
        return path

    async def test_shrinks_a_clip_under_the_limit_and_keeps_audio_and_video(self):
        source = await asyncio.to_thread(self._make_clip)
        limit = os.path.getsize(source) // 3

        result = await shrink_video(source, limit, str(self.output_dir))

        self.assertIsNotNone(result)
        self.assertLessEqual(os.path.getsize(result), limit)
        probed = await asyncio.to_thread(_probe, result)
        self.assertEqual(sorted(stream["codec_type"] for stream in probed["streams"]), ["audio", "video"])
        self.assertAlmostEqual(float(probed["format"]["duration"]), 6, delta=1)
        self.assertTrue(os.path.exists(source), "the source file is the caller's to remove")
        self.assertEqual([str(p) for p in self.output_dir.iterdir()], [result])

    async def test_easy_footage_is_not_inflated_to_fill_the_size_budget(self):
        source = await asyncio.to_thread(self._make_clip)
        limit = os.path.getsize(source)

        result = await shrink_video(source, limit, str(self.output_dir))

        self.assertIsNotNone(result)
        self.assertLess(
            os.path.getsize(result), limit * 0.5, "constant quality, not the bitrate ceiling, sets the size"
        )

    async def test_a_tight_limit_downscales_a_landscape_picture(self):
        source = await asyncio.to_thread(self._make_clip, 10, "1280x720")
        # ~300 kbit/s of video after audio and the safety margin, which maps to a 360p picture.
        limit = int((300_000 + AUDIO_BITRATE) * 10 / 8 / video_compression.SIZE_MARGIN)

        result = await shrink_video(source, limit, str(self.output_dir))

        self.assertIsNotNone(result)
        video = await self._video_stream(result)
        self.assertEqual((video["width"], video["height"]), (640, 360))

    async def test_a_tight_limit_keeps_a_portrait_picture_portrait(self):
        source = await asyncio.to_thread(self._make_clip, 10, "540x960")
        limit = int((300_000 + AUDIO_BITRATE) * 10 / 8 / video_compression.SIZE_MARGIN)

        result = await shrink_video(source, limit, str(self.output_dir))

        self.assertIsNotNone(result)
        video = await self._video_stream(result)
        self.assertEqual((video["width"], video["height"]), (360, 640))

    async def _video_stream(self, path: str) -> dict:
        probed = await asyncio.to_thread(_probe, path)
        return next(stream for stream in probed["streams"] if stream["codec_type"] == "video")

    async def test_clip_too_long_for_the_limit_is_rejected_without_output(self):
        source = await asyncio.to_thread(self._make_clip)

        result = await shrink_video(source, 10_000, str(self.output_dir))

        self.assertIsNone(result)
        self.assertEqual(list(self.output_dir.iterdir()), [])

    async def test_a_playlist_disguised_as_mp4_is_not_read(self):
        playlist = self.source_dir / "evil.mp4"
        playlist.write_text("#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1,\nfile:///etc/passwd\n#EXT-X-ENDLIST\n")

        result = await shrink_video(str(playlist), 1 * _MIB, str(self.output_dir))

        self.assertIsNone(result)
        self.assertEqual(list(self.output_dir.iterdir()), [])

    async def test_cancellation_stops_ffmpeg_and_leaves_no_output(self):
        source = await asyncio.to_thread(self._make_clip, 20)
        limit = os.path.getsize(source) // 3

        task = asyncio.create_task(shrink_video(source, limit, str(self.output_dir)))
        await asyncio.sleep(0.3)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task

        self.assertEqual(list(self.output_dir.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
