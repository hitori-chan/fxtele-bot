"""Instagram media extractor."""

import base64
import hashlib
import hmac
import json
import logging
import re
import time

import httpx

from config import USER_AGENT, HTTP_TIMEOUT
from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from services.http import get_client
from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_INSTAGRAM = re.compile(r"(https?://(?:www\.)?instagram\.com/\S+)")


def _decode_text(value: str) -> str:
    return base64.b64decode(value).decode()


def _decode_int(value: str) -> int:
    return int(_decode_text(value))


_CONVERT_ENDPOINT = _decode_text("aHR0cHM6Ly9hcGktd2guYW5vbnlpZy5jb20vYXBpL2NvbnZlcnQ=")
_REQUEST_ORIGIN = _decode_text("aHR0cHM6Ly9hbm9ueWlnLmNvbQ==")
_REQUEST_REFERER = _decode_text("aHR0cHM6Ly9hbm9ueWlnLmNvbS8=")
_MEDIA_ENDPOINT_PREFIX = _decode_text("aHR0cHM6Ly9tZWRpYS5hbm9ueWlnLmNvbS9nZXQ/")
_SIGNING_KEY = base64.b64decode("ARPqyYIZGtF0SeVxEpIhTscd2Z2XLaRq3pH1WiGi/bU=")
_REQUEST_EPOCH = _decode_int("MTc3Njg1Nzc3NDg3Mw==")
_SIGNATURE_COUNTER = 0
_SIGNATURE_VERSION = 2


@register_handler("instagram")
class InstagramExtractor(MediaExtractor):
    """Extract direct media from Instagram posts."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media URLs from a public Instagram post."""
        url = self._normalize_instagram_url(url)

        try:
            client = get_client()
            headers = {
                "User-Agent": USER_AGENT,
                "Accept": "application/json, text/plain, */*",
                "Accept-Language": "en-US,en;q=0.9",
                "Content-Type": "application/json",
                "Origin": _REQUEST_ORIGIN,
                "Referer": _REQUEST_REFERER,
                "Sec-Fetch-Dest": "empty",
                "Sec-Fetch-Mode": "cors",
                "Sec-Fetch-Site": "same-site",
            }
            payload = self._signed_payload(url)

            if not client:
                async with httpx.AsyncClient(
                    timeout=HTTP_TIMEOUT,
                    http2=True,
                    headers=headers,
                ) as temp_client:
                    response = await temp_client.post(
                        _CONVERT_ENDPOINT,
                        json=payload,
                    )
            else:
                response = await client.post(
                    _CONVERT_ENDPOINT,
                    json=payload,
                    headers=headers,
                )

            response.raise_for_status()
            data = response.json()
            media_urls = self._extract_media_urls(data)

            if not media_urls:
                logger.warning("No Instagram media links found for %s", url)
                return None

            metadata = {"original_url": url}
            caption = self._extract_caption(data)
            if caption:
                metadata["caption"] = caption
            metadata["thumbnail"] = media_urls[0]

            return HandlerResult(
                type=HandlerType.MEDIA_EXTRACTOR,
                content=media_urls,
                metadata=metadata,
            )

        except Exception as e:
            logger.error("Error extracting Instagram media from %s: %r", url, e)
            return None

    def _signed_payload(self, url: str) -> dict:
        base_payload = {"target_url": url}
        current_time_ms = int(time.time() * 1000)
        message = json.dumps(
            base_payload,
            separators=(",", ":"),
            sort_keys=True,
        ) + str(current_time_ms)
        signature = hmac.new(
            _SIGNING_KEY,
            message.encode(),
            hashlib.sha256,
        ).hexdigest()

        return {
            **base_payload,
            "ts": current_time_ms,
            "_ts": _REQUEST_EPOCH,
            "_tsc": _SIGNATURE_COUNTER,
            "_sv": _SIGNATURE_VERSION,
            "_s": signature,
        }

    def _extract_media_urls(self, data) -> list[str]:
        media_urls: list[str] = []

        def walk(value) -> None:
            if isinstance(value, dict):
                if self._is_media_url(value):
                    media_urls.append(value["url"])
                    return
                for child in value.values():
                    walk(child)
            elif isinstance(value, list):
                for child in value:
                    walk(child)

        walk(data)
        return list(dict.fromkeys(media_urls))

    def _is_media_url(self, value: dict) -> bool:
        url = value.get("url")
        if not isinstance(url, str):
            return False
        return url.startswith(_MEDIA_ENDPOINT_PREFIX) or (
            "instagram." in url and any(hint in url.lower() for hint in (".jpg", ".jpeg", ".png", ".webp", ".mp4"))
        )

    def _extract_caption(self, data) -> str | None:
        if isinstance(data, list):
            for item in data:
                caption = self._extract_caption(item)
                if caption:
                    return caption
            return None

        if not isinstance(data, dict):
            return None

        meta = data.get("meta")
        if isinstance(meta, dict) and isinstance(meta.get("title"), str):
            return meta["title"]

        for child in data.values():
            caption = self._extract_caption(child)
            if caption:
                return caption
        return None

    def _normalize_instagram_url(self, url: str) -> str:
        return url.rstrip(".,!?;)")
