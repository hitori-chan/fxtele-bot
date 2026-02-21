#!/usr/bin/env python
from __future__ import annotations

import asyncio
import logging
import os
import sys
import subprocess
import re
import random
import json
from pathlib import Path
from uuid import uuid4
from pixivpy3 import AppPixivAPI, PixivError
from telegram import Update
from telegram.ext import ContextTypes

sys.dont_write_bytecode = True

logger = logging.getLogger(__name__)

COOKIE_FILE = Path("/tmp/pixiv_key.json")
IMAGE_DIR = Path("/tmp/pixiv_image")

_TOKEN_FETCHER = Path(__file__).with_name("pixiv_token_fetcher.py")


def get_token():
    print("🔄 Fetching new Pixiv token...")

    output = subprocess.check_output(
        ["xvfb-run", "-a", "python3", str(_TOKEN_FETCHER)]
    )

    text = output.decode("utf-8")

    match1 = re.search(r"Access Token:\s*([A-Za-z0-9_-]+)", text)
    match2 = re.search(r"Refresh Token:\s*([A-Za-z0-9_-]+)", text)

    if not match1 or not match2:
        raise RuntimeError("❌ Failed to extract tokens")

    access_token = match1.group(1)
    refresh_token = match2.group(1)

    COOKIE_FILE.parent.mkdir(parents=True, exist_ok=True)

    with open(COOKIE_FILE, "w") as f:
        json.dump(
            {
                "access_token": access_token,
                "refresh_token": refresh_token,
            },
            f,
        )

    print("✅ Token saved")
    return access_token, refresh_token

def load_or_generate_token():
    if not COOKIE_FILE.exists():
        print("🆕 Token file not found. Generating new one...")
        return get_token()

    try:
        with open(COOKIE_FILE, "r") as f:
            data = json.load(f)

        return data["access_token"], data["refresh_token"]

    except Exception:
        print("⚠️ Token file corrupted. Regenerating...")
        return get_token()

def _new_tmp_image_path(illust_id: int) -> Path:
    IMAGE_DIR.mkdir(parents=True, exist_ok=True)
    for _ in range(10):
        candidate = IMAGE_DIR / f"pixiv_{illust_id}_{uuid4().hex}.jpg"
        if not candidate.exists():
            return candidate
    raise RuntimeError("Failed to allocate unique temp filename")


def get_image(access_token: str, refresh_token: str) -> Path:
    api = AppPixivAPI()
    api.set_auth(access_token, refresh_token)

    print("🎨 Fetching illustration ranking...")
    json_result = api.illust_ranking("day_r18", offset=random.randint(1, 100))

    if not json_result.illusts:
        raise RuntimeError("No illustrations returned")

    illust = json_result.illusts[0]
    filename = _new_tmp_image_path(int(illust.id))

    def file_too_large(path: Path):
        return path.exists() and path.stat().st_size >= 10 * 1024 * 1024

    def file_empty(path: Path) -> bool:
        return (not path.exists()) or path.stat().st_size == 0

    def ensure_not_exists(path: Path) -> None:
        try:
            path.unlink(missing_ok=True)
        except Exception:
            raise RuntimeError(f"Unable to remove existing temp file: {path}")

    ensure_not_exists(filename)

    if illust.meta_single_page and illust.meta_single_page.original_image_url:
        api.download(
            illust.meta_single_page.original_image_url,
            fname=str(filename),
        )
    else:
        api.download(illust.image_urls.large, fname=str(filename))

    if file_empty(filename):
        raise RuntimeError("Pixiv download produced an empty file")

    if file_too_large(filename):
        ensure_not_exists(filename)
        api.download(illust.image_urls.medium, fname=str(filename))

    if file_too_large(filename):
        ensure_not_exists(filename)
        api.download(illust.image_urls.square_medium, fname=str(filename))

    if file_empty(filename):
        raise RuntimeError("Pixiv download produced an empty file")

    print(f"✅ Image saved: {filename}")
    return filename


def download_pixiv_image_to_tmp() -> Path:
    """Download a Pixiv image into /tmp and return the file path.

    Will reuse the cached token if present; if the token expired, regenerates it.
    """
    access_token, refresh_token = load_or_generate_token()

    try:
        return get_image(access_token, refresh_token)
    except PixivError:
        print("⚠️ Token expired. Regenerating...")
        access_token, refresh_token = get_token()
        return get_image(access_token, refresh_token)


async def pixiv_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Telegram command: /pixiv

    Downloads an image to /tmp, sends it, then deletes the file.
    """
    if not update.message:
        return

    tmp_path: Path | None = None
    try:
        try:
            tmp_path = await asyncio.to_thread(download_pixiv_image_to_tmp)
        except Exception:
            tmp_path = await asyncio.to_thread(download_pixiv_image_to_tmp)
        with tmp_path.open("rb") as f:
            await update.message.reply_photo(photo=f)
    except Exception as e:
        logger.exception("/pixiv failed")
        await update.message.reply_text(f"Failed to fetch Pixiv image: {e}")
    finally:
        if tmp_path:
            try:
                tmp_path.unlink(missing_ok=True)
            except Exception:
                logger.warning("Failed to delete temp file: %s", tmp_path)


def main():
    download_pixiv_image_to_tmp()

if __name__ == "__main__":
    main()