"""Regression tests for Instagram media extraction, exercised against
https://www.instagram.com/p/DcGDayNkZQ3/ (a 3-item carousel: 2 photos + 1 video).
"""

import unittest
from unittest.mock import patch
from urllib.parse import quote

import httpx

from core.types import LinkFixResult
from handlers.media_extractors.instagram import (
    InstagramExtractor,
    _extract_anonyig_result,
    _resolve_anonyig_proxy_url,
)

_POST_URL = "https://www.instagram.com/p/DcGDayNkZQ3/"
_ANONYIG_URL = "https://api-wh.anonyig.com/api/convert"
_SIGNED_PAYLOAD = {
    "target_url": _POST_URL,
    "ts": 1788068824717,
    "_ts": 1787143199835,
    "_tsc": 0,
    "_sv": 2,
    "_s": "75f952a966c4f7526fd11f0a403b042331fb64f95cce62d746e62511dab38544",
}


def _anonyig_proxy_url(real_url: str) -> str:
    """Build an anonyig media proxy URL wrapping real_url, matching its live response shape."""
    return f"https://media.anonyig.com/get?__sig=x&__expires=1&uri={quote(real_url, safe='')}"


# Mirrors the shape of a real anonyig /api/convert response for a 3-item
# carousel: a list with one entry per media item, each carrying the full
# post metadata (including the caption in meta.title) and a proxied CDN URL.
_ANONYIG_CAPTION = "MAXIM B-CUT로 발매된 💕 test caption"
_ANONYIG_RESPONSE = [
    {
        "url": [{"url": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo1.jpg"), "type": "jpg"}],
        "meta": {"title": _ANONYIG_CAPTION, "shortcode": "DcGDayNkZQ3", "username": "yun_yeha"},
        "thumb": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo1_thumb.jpg"),
    },
    {
        "url": [{"url": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo2.jpg"), "type": "jpg"}],
        "meta": {"title": _ANONYIG_CAPTION, "shortcode": "DcGDayNkZQ3", "username": "yun_yeha"},
        "thumb": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo2_thumb.jpg"),
    },
    {
        "url": [{"url": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/video.mp4"), "type": "mp4"}],
        "meta": {"title": _ANONYIG_CAPTION, "shortcode": "DcGDayNkZQ3", "username": "yun_yeha"},
        "thumb": _anonyig_proxy_url("https://instagram.fna.fbcdn.net/video_thumb.jpg"),
    },
]


class _FakeResponse:
    def __init__(self, *, json_data=None, status_code: int = 200):
        self._json_data = json_data
        self.status_code = status_code

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise httpx.HTTPStatusError("error", request=None, response=self)

    def json(self):
        return self._json_data


class _FakeClient:
    """Minimal stand-in for httpx.AsyncClient that scripts responses per URL."""

    def __init__(self, responses: dict[str, _FakeResponse]):
        self._responses = responses
        self.requests: list[tuple[str, dict | None]] = []

    async def post(self, url, json=None, headers=None):
        self.requests.append((url, json))
        return self._responses[url]


class ResolveAnonyigProxyUrlTests(unittest.TestCase):
    def test_unwraps_the_proxied_cdn_url(self):
        wrapped = _anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo1.jpg")

        self.assertEqual(_resolve_anonyig_proxy_url(wrapped), "https://instagram.fna.fbcdn.net/photo1.jpg")

    def test_non_proxy_url_is_returned_unchanged(self):
        self.assertEqual(
            _resolve_anonyig_proxy_url("https://instagram.fna.fbcdn.net/photo1.jpg"),
            "https://instagram.fna.fbcdn.net/photo1.jpg",
        )

    def test_non_string_returns_none(self):
        self.assertIsNone(_resolve_anonyig_proxy_url(None))


class ExtractAnonyigResultTests(unittest.TestCase):
    def test_carousel_response_yields_three_media_and_the_caption(self):
        result = _extract_anonyig_result(_ANONYIG_RESPONSE, _POST_URL)

        self.assertEqual(
            result.urls,
            (
                "https://instagram.fna.fbcdn.net/photo1.jpg",
                "https://instagram.fna.fbcdn.net/photo2.jpg",
                "https://instagram.fna.fbcdn.net/video.mp4",
            ),
        )
        self.assertEqual(result.metadata.caption, _ANONYIG_CAPTION)
        self.assertEqual(result.metadata.thumbnail, "https://instagram.fna.fbcdn.net/photo1_thumb.jpg")
        self.assertEqual(result.metadata.original_url, _POST_URL)

    def test_response_without_media_returns_none(self):
        self.assertIsNone(_extract_anonyig_result([{"meta": {"title": "no media here"}}], _POST_URL))


class InstagramExtractorTests(unittest.IsolatedAsyncioTestCase):
    async def test_post_url_extracts_media_and_caption_when_signer_available(self):
        fake_client = _FakeClient({_ANONYIG_URL: _FakeResponse(json_data=_ANONYIG_RESPONSE)})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=_SIGNED_PAYLOAD),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(f"look at this {_POST_URL}")

        self.assertIsNotNone(result)
        self.assertEqual(len(result.urls), 3)
        self.assertEqual(result.metadata.caption, _ANONYIG_CAPTION)
        self.assertEqual(result.metadata.original_url, _POST_URL)
        self.assertEqual([call[0] for call in fake_client.requests], [_ANONYIG_URL])

    async def test_signer_unavailable_falls_back_to_oginstagram_link(self):
        fake_client = _FakeClient({})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=None),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(f"look at this {_POST_URL}")

        self.assertIsInstance(result, LinkFixResult)
        self.assertEqual(result.content, "https://oginstagram.com/p/DcGDayNkZQ3/")
        self.assertEqual(fake_client.requests, [])

    async def test_anonyig_error_falls_back_to_oginstagram_link(self):
        # Mirrors a live anonyig failure: a captcha challenge or a rejected signature.
        fake_client = _FakeClient({_ANONYIG_URL: _FakeResponse(status_code=422, json_data={"code": "CAPTCHA_REQUIRED"})})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=_SIGNED_PAYLOAD),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(_POST_URL)

        self.assertIsInstance(result, LinkFixResult)
        self.assertEqual(result.content, "https://oginstagram.com/p/DcGDayNkZQ3/")

    async def test_anonyig_response_without_media_falls_back_to_oginstagram_link(self):
        fake_client = _FakeClient({_ANONYIG_URL: _FakeResponse(json_data=[{"meta": {"title": "no media"}}])})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=_SIGNED_PAYLOAD),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(_POST_URL)

        self.assertIsInstance(result, LinkFixResult)
        self.assertEqual(result.content, "https://oginstagram.com/p/DcGDayNkZQ3/")


if __name__ == "__main__":
    unittest.main()
