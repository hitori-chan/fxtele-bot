"""Shrink oversized videos so they fit under Telegram's upload limit.

Encoding is constant-quality (CRF) with a bitrate ceiling derived from the size
limit. Easy footage lands well under the limit at the target quality, which also
uploads faster; footage that would not fit is held to the ceiling, and the
picture is downscaled first when the ceiling is too low to carry full
resolution without visible blockiness.
"""

import asyncio
import contextlib
import json
import logging
import os
import shutil
import subprocess
import uuid
from dataclasses import dataclass

from utils.error_text import describe_error

logger = logging.getLogger(__name__)

AUDIO_BITRATE = 96_000
# CRF 23 is x264's default: visually transparent enough for phone-sized playback.
QUALITY_CRF = 23
# At the bitrate ceiling slower presets produce the same size and only slightly better pictures, at
# about 1.6x the encode time, so the quick preset wins for a chat bot.
ENCODER_PRESET = "veryfast"
# Below this the picture is unwatchable, so give up and let the caller fall back to a link.
MIN_VIDEO_BITRATE = 200_000
# The bitrate ceiling is a ceiling, not a guarantee; aim under the limit to land under it.
SIZE_MARGIN = 0.9
# Each retry lowers the ceiling in case the first attempt still overshot the limit.
RETRY_BITRATE_FACTORS = (1.0, 0.75)
# Bits per pixel per frame that x264 needs to avoid visible blocking at typical social-video complexity.
MIN_BITS_PER_PIXEL = 0.06
# Shorter-side sizes to step down through (1080 = 1080x1920 portrait or 1920x1080 landscape).
TARGET_SHORT_SIDES = (1080, 720, 480, 360)
DEFAULT_FPS = 30.0
PROBE_TIMEOUT = 30.0
MIN_TRANSCODE_TIMEOUT = 300.0
# 1080p encodes at about 2x realtime on 8 cores; allow headroom for much slower hosts.
TRANSCODE_SECONDS_PER_VIDEO_SECOND = 4.0
# Every extractor returns MP4. Pinning the demuxer keeps ffmpeg from treating a
# hostile download as an HLS/concat playlist that reads local files.
INPUT_FORMAT = "mov,mp4,m4a,3gp,3g2,mj2"
# Also allow only local files as inputs, so no demuxer feature can open a network URL.
INPUT_ARGS = ("-protocol_whitelist", "file", "-f", INPUT_FORMAT)

# Software encoding saturates the CPU; one video at a time keeps the bot responsive.
_transcode_slot = asyncio.Semaphore(1)


@dataclass(frozen=True)
class VideoInfo:
    """Properties of a source video that decide how it is re-encoded."""

    duration: float
    width: int
    height: int
    fps: float


def is_available() -> bool:
    """Return whether ffmpeg and ffprobe are installed."""
    return bool(shutil.which("ffmpeg") and shutil.which("ffprobe"))


def plan_video_bitrate(duration_seconds: float, max_bytes: int) -> int | None:
    """Return the video bit/s ceiling that fits max_bytes at this duration, or None if it would look too poor."""
    if duration_seconds <= 0:
        return None
    total_bitrate = max_bytes * SIZE_MARGIN * 8 / duration_seconds
    video_bitrate = int(total_bitrate - AUDIO_BITRATE)
    return video_bitrate if video_bitrate >= MIN_VIDEO_BITRATE else None


def pick_short_side(info: VideoInfo, video_bitrate: int) -> int | None:
    """Return the shorter-side size to downscale to, or None to keep the source size.

    Steps down from the source size to the largest one whose pixel rate the
    bitrate can carry at MIN_BITS_PER_PIXEL, bottoming out at the smallest
    target. Sizing by the shorter side treats portrait and landscape alike.
    """
    source_short = min(info.width, info.height)
    candidates = [source_short, *(side for side in TARGET_SHORT_SIDES if side < source_short)]
    for side in candidates:
        scale = side / source_short
        pixel_rate = info.width * scale * info.height * scale * info.fps
        if video_bitrate >= MIN_BITS_PER_PIXEL * pixel_rate:
            return None if side == source_short else side
    return None if candidates[-1] == source_short else candidates[-1]


def scale_filter(short_side: int) -> str:
    """Return a filter that sets the shorter side to short_side and keeps the aspect ratio.

    Written against iw/ih rather than fixed dimensions so it also holds for
    sources that ffmpeg auto-rotates from metadata.
    """
    return f"scale='if(lt(iw,ih),{short_side},-2)':'if(lt(iw,ih),-2,{short_side})'"


async def shrink_video(source_path: str, max_bytes: int, output_dir: str) -> str | None:
    """Re-encode source_path to an MP4 under max_bytes in output_dir.

    Returns the new file's path, or None if the video could not be made to fit.
    The source file is left in place.
    """
    async with _transcode_slot:
        try:
            return await _shrink(source_path, max_bytes, output_dir)
        except (TimeoutError, OSError) as e:
            logger.warning("Video compression failed: %s.", describe_error(e))
            return None


async def _shrink(source_path: str, max_bytes: int, output_dir: str) -> str | None:
    info = await _probe(source_path)
    if info is None:
        return None

    video_bitrate = plan_video_bitrate(info.duration, max_bytes)
    if video_bitrate is None:
        logger.warning("Video of %.0fs is too long to fit %d bytes at usable quality.", info.duration, max_bytes)
        return None

    for factor in RETRY_BITRATE_FACTORS:
        ceiling = int(video_bitrate * factor)
        if ceiling < MIN_VIDEO_BITRATE:
            break
        output_path = os.path.join(output_dir, f"{uuid.uuid4()}.mp4")
        fits = False
        try:
            fits = await _encode(source_path, output_path, info, ceiling) and os.path.getsize(output_path) <= max_bytes
        finally:
            if not fits:
                _remove(output_path)
        if fits:
            return output_path
        logger.debug("Compression attempt with a %d bit/s ceiling did not fit; retrying lower.", ceiling)
    logger.warning("Video still did not fit %d bytes after the last compression attempt.", max_bytes)
    return None


async def _probe(path: str) -> VideoInfo | None:
    returncode, output, error_line = await _run(
        [
            "ffprobe",
            "-v",
            "error",
            *INPUT_ARGS,
            "-select_streams",
            "v:0",
            "-show_entries",
            "format=duration:stream=width,height,avg_frame_rate",
            "-of",
            "json",
            path,
        ],
        PROBE_TIMEOUT,
    )
    if returncode != 0:
        logger.warning("Could not read the video properties (ffprobe exit %d): %s", returncode, error_line)
        return None
    try:
        data = json.loads(output)
        stream = data["streams"][0]
        duration = float(data["format"]["duration"])
        width = int(stream["width"])
        height = int(stream["height"])
    except (ValueError, KeyError, IndexError, TypeError):
        logger.warning("Could not read the video properties: ffprobe reported no usable video stream.")
        return None
    if duration <= 0 or width <= 0 or height <= 0:
        logger.warning("Could not read the video properties: duration %s, size %dx%d.", duration, width, height)
        return None
    return VideoInfo(duration, width, height, _parse_frame_rate(stream.get("avg_frame_rate")))


def _parse_frame_rate(value: object) -> float:
    """Parse ffprobe's "30000/1001" frame rate, falling back to a typical rate when it is missing or absurd."""
    try:
        numerator, denominator = str(value).split("/")
        fps = float(numerator) / float(denominator)
    except (ValueError, ZeroDivisionError):
        return DEFAULT_FPS
    return fps if 1 <= fps <= 120 else DEFAULT_FPS


async def _encode(source_path: str, output_path: str, info: VideoInfo, video_bitrate: int) -> bool:
    command = [
        "ffmpeg",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "error",
        "-y",
        *INPUT_ARGS,
        "-i",
        source_path,
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        "-c:v",
        "libx264",
        "-preset",
        ENCODER_PRESET,
        "-profile:v",
        "high",
        "-pix_fmt",
        "yuv420p",
        "-crf",
        str(QUALITY_CRF),
        "-maxrate",
        str(video_bitrate),
        "-bufsize",
        str(video_bitrate * 2),
    ]
    if (short_side := pick_short_side(info, video_bitrate)) is not None:
        command += ["-vf", scale_filter(short_side)]
    command += ["-c:a", "aac", "-b:a", str(AUDIO_BITRATE), "-movflags", "+faststart", output_path]

    timeout = max(MIN_TRANSCODE_TIMEOUT, info.duration * TRANSCODE_SECONDS_PER_VIDEO_SECOND)
    returncode, _, error_line = await _run(command, timeout)
    if returncode != 0:
        logger.warning("ffmpeg failed (exit %d): %s", returncode, error_line)
    return returncode == 0


async def _run(command: list[str], timeout: float) -> tuple[int, bytes, str]:
    """Run a command to completion, killing it on timeout or cancellation.

    Returns its exit code, stdout and the last line of stderr (the reason, when it failed).
    """
    process = await asyncio.create_subprocess_exec(
        *command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        stdout, stderr = await asyncio.wait_for(process.communicate(), timeout)
    except BaseException:
        with contextlib.suppress(ProcessLookupError):
            process.kill()
        await process.wait()
        raise
    return process.returncode or 0, stdout, _last_line(stderr)


def _last_line(output: bytes) -> str:
    """Last non-empty line of a tool's output, shortened for a log line."""
    lines = [line for line in output.decode(errors="replace").splitlines() if line.strip()]
    return lines[-1].strip()[:200] if lines else "(no output)"


def _remove(path: str) -> None:
    with contextlib.suppress(FileNotFoundError):
        os.remove(path)
