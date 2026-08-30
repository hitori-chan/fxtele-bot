"""Regression tests for Instagram media extraction, exercised against
https://www.instagram.com/p/DcGDayNkZQ3/ (a 3-item carousel: 2 photos + 1 video).
"""

import base64
import json
import unittest
from unittest.mock import patch

from handlers.media_extractors.instagram import (
    InstagramExtractor,
    _decode_download_token,
    _extract_anonyig_result,
    _extract_media_candidates,
    _resolve_anonyig_proxy_url,
    _unpack_response,
)

_POST_URL = "https://www.instagram.com/p/DcGDayNkZQ3/"
_SNAPSAVE_URL = "https://snapsave.app/action.php?lang=en"
_ANONYIG_URL = "https://api-wh.anonyig.com/api/convert"

# The exact packer parameters observed from a live snapsave.app response for
# _POST_URL: payload segments are base-`BASE` numerals over the first `BASE`
# characters of `ALPHABET`, delimited by `ALPHABET[BASE]`, offset by `OFFSET`.
_ALPHABET = "KnyMEPjmi"
_BASE = 2
_OFFSET = 30
_U = 94
_R = 5


def _pack(text: str, *, alphabet: str = _ALPHABET, base: int = _BASE, offset: int = _OFFSET) -> str:
    """Encode text the way snapsave.app's packed eval() response does (test-only inverse of _unpack_response)."""
    segments = []
    for byte in text.encode("utf-8"):
        value = byte + offset
        digits = _to_base(value, base)
        segments.append("".join(alphabet[int(d)] for d in digits) + alphabet[base])
    return "".join(segments)


def _to_base(value: int, base: int) -> str:
    if value == 0:
        return "0"
    digits = []
    while value:
        digits.append(str(value % base))
        value //= base
    return "".join(reversed(digits))


def _packed_eval_response(decoded_html: str) -> str:
    """Wrap decoded_html in the same eval(function(h,u,n,t,e,r){...}(...)) shell snapsave.app returns."""
    escaped = decoded_html.replace('"', '\\"')
    payload = _pack(escaped)
    decoder_body = (
        'r="";for(var i=0,len=h.length;i<len;i++){var s="";while(h[i]!==n[e]){s+=h[i];i++}'
        'for(var j=0;j<n.length;j++)s=s.replace(new RegExp(n[j],"g"),j);'
        'r+=String.fromCharCode(_0xdecode(s,e,10)-t)}return decodeURIComponent(escape(r))'
    )
    return f'eval(function(h,u,n,t,e,r){{{decoder_body}}}("{payload}",{_U},"{_ALPHABET}",{_OFFSET},{_BASE},{_R}))'


def _download_item(thumb_url: str, media_url: str, filename: str) -> str:
    thumb_token = _make_token({"url": thumb_url, "filename": filename})
    media_token = _make_token({"url": media_url, "filename": filename})
    return (
        '<div class="download-items">'
        f'<div class="download-items__thumb"><img src="https://d.rapidcdn.app/thumb?token={thumb_token}"></div>'
        '<div class="download-items__btn">'
        f'<a href="https://d.rapidcdn.app/v2?token={media_token}" title="Download Photo">Download</a>'
        "</div></div>"
    )


def _make_token(payload: dict) -> str:
    body = base64.urlsafe_b64encode(json.dumps(payload).encode()).rstrip(b"=").decode()
    return f"header.{body}.signature"


_CAROUSEL_HTML = (
    '<div class="container download-box"><div class="row">'
    + _download_item(
        "https://instagram.fna.fbcdn.net/photo1_thumb.jpg",
        "https://instagram.fna.fbcdn.net/photo1.jpg",
        "snapsave-app_DcGDayNkZQ3.jpg",
    )
    + _download_item(
        "https://instagram.fna.fbcdn.net/photo2_thumb.jpg",
        "https://instagram.fna.fbcdn.net/photo2.jpg",
        "snapsave-app_DcGDayNkZQ3.jpg",
    )
    + _download_item(
        "https://instagram.fna.fbcdn.net/video_thumb.jpg",
        "https://instagram.fna.fbcdn.net/video.mp4",
        "snapsave-app_DcGDayNkZQ3.mp4",
    )
    + "</div></div>"
)


def _anonyig_proxy_url(real_url: str) -> str:
    """Build an anonyig media proxy URL wrapping real_url, matching its live response shape."""
    from urllib.parse import quote

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
_SIGNED_PAYLOAD = {
    "target_url": _POST_URL,
    "ts": 1788068824717,
    "_ts": 1787143199835,
    "_tsc": 0,
    "_sv": 2,
    "_s": "75f952a966c4f7526fd11f0a403b042331fb64f95cce62d746e62511dab38544",
}


class _FakeResponse:
    def __init__(self, *, text: str | None = None, json_data=None):
        self.text = text
        self._json_data = json_data

    def raise_for_status(self) -> None:
        pass

    def json(self):
        return self._json_data


class _FakeClient:
    """Minimal stand-in for httpx.AsyncClient that scripts responses per URL."""

    def __init__(self, responses: dict[str, _FakeResponse]):
        self._responses = responses
        self.requests: list[tuple[str, dict | None, dict | None]] = []

    async def post(self, url, data=None, json=None, headers=None):
        self.requests.append((url, data if data is not None else json, headers))
        return self._responses[url]


class UnpackResponseTests(unittest.TestCase):
    def test_round_trips_arbitrary_text_with_the_observed_packer_params(self):
        packed = _pack('Hello <div class="x">世界</div>')
        raw_response = f'eval(function(h,u,n,t,e,r){{}}("{packed}",{_U},"{_ALPHABET}",{_OFFSET},{_BASE},{_R}))'

        decoded = _unpack_response(raw_response)

        self.assertEqual(decoded, 'Hello <div class="x">世界</div>')

    def test_non_packed_text_returns_none(self):
        self.assertIsNone(_unpack_response("<html>not a packed response</html>"))


class DecodeDownloadTokenTests(unittest.TestCase):
    def test_extracts_the_wrapped_cdn_url(self):
        token = _make_token({"url": "https://instagram.fna.fbcdn.net/photo1.jpg", "filename": "x.jpg"})

        self.assertEqual(
            _decode_download_token(f"https://d.rapidcdn.app/v2?token={token}"),
            "https://instagram.fna.fbcdn.net/photo1.jpg",
        )

    def test_missing_token_returns_none(self):
        self.assertIsNone(_decode_download_token("https://d.rapidcdn.app/v2?nope=1"))


class ExtractMediaCandidatesTests(unittest.TestCase):
    def test_carousel_html_yields_three_ordered_candidates(self):
        candidates = _extract_media_candidates(_CAROUSEL_HTML)

        self.assertEqual(
            [candidate.url for candidate in candidates],
            [
                "https://instagram.fna.fbcdn.net/photo1.jpg",
                "https://instagram.fna.fbcdn.net/photo2.jpg",
                "https://instagram.fna.fbcdn.net/video.mp4",
            ],
        )
        self.assertEqual(candidates[0].thumbnail, "https://instagram.fna.fbcdn.net/photo1_thumb.jpg")


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
    async def test_post_url_prefers_anonyig_with_caption_when_signer_available(self):
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
        # Caption is only available via anonyig; snapsave must not have been tried.
        self.assertEqual([call[0] for call in fake_client.requests], [_ANONYIG_URL])

    async def test_post_url_falls_back_to_snapsave_when_signer_unavailable(self):
        fake_client = _FakeClient({_SNAPSAVE_URL: _FakeResponse(text=_packed_eval_response(_CAROUSEL_HTML))})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=None),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(f"look at this {_POST_URL}")

        self.assertIsNotNone(result)
        self.assertEqual(
            result.urls,
            (
                "https://instagram.fna.fbcdn.net/photo1.jpg",
                "https://instagram.fna.fbcdn.net/photo2.jpg",
                "https://instagram.fna.fbcdn.net/video.mp4",
            ),
        )
        self.assertIsNone(result.metadata.caption)
        self.assertEqual(result.metadata.thumbnail, "https://instagram.fna.fbcdn.net/photo1_thumb.jpg")
        self.assertEqual(result.metadata.original_url, _POST_URL)
        self.assertEqual([call[0] for call in fake_client.requests], [_SNAPSAVE_URL])

    async def test_unparseable_snapsave_response_degrades_to_none(self):
        fake_client = _FakeClient({_SNAPSAVE_URL: _FakeResponse(text="<html>Something went wrong</html>")})

        with (
            patch("handlers.media_extractors.instagram.get_client", return_value=fake_client),
            patch("handlers.media_extractors.instagram.sign_target_url", return_value=None),
        ):
            extractor = InstagramExtractor()
            result = await extractor.handle(_POST_URL)

        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
