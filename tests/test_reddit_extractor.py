"""Regression tests for Reddit share-link resolution and JSON endpoint selection."""

import unittest
from urllib.parse import urlparse

import httpx

from handlers.media_extractors.reddit import (
    _is_reddit_share_link,
    _metadata_from_old_reddit_html,
    _old_reddit_html_url,
    _reddit_json_urls,
    _resolved_reddit_url,
)

_SHARE_URL = "https://www.reddit.com/r/PiratedGames/s/fyREoFRf4z"
_CANONICAL_URL = (
    "https://www.reddit.com/r/PiratedGames/comments/1uxuqya/"
    "linux_hypervisor_working_great_on_ac_black_flag/?share_id=MJAhv5MFlOm99IdP8vTkU"
)


class _FakeResponse:
    def __init__(self, url: str, status_code: int, location: str | None = None):
        self.url = httpx.URL(url)
        self.status_code = status_code
        self.headers = {"Location": location} if location else {}

    @property
    def is_redirect(self) -> bool:
        return self.status_code in (301, 302, 303, 307, 308)


class _FakeClient:
    """Minimal stand-in for httpx.AsyncClient with a scripted redirect map."""

    def __init__(self, script: dict[str, tuple[int, str | None]]):
        self._script = script
        self.requests: list[str] = []

    async def get(self, url, **kwargs):
        url = str(url)
        self.requests.append(url)
        status, location = self._script[url]
        return _FakeResponse(url, status, location)


class RedditShareLinkTests(unittest.IsolatedAsyncioTestCase):
    def test_is_share_link_recognizes_s_shape(self):
        self.assertTrue(_is_reddit_share_link(urlparse(_SHARE_URL)))

    def test_is_share_link_rejects_comments_and_gallery(self):
        self.assertFalse(_is_reddit_share_link(urlparse(_CANONICAL_URL)))
        self.assertFalse(_is_reddit_share_link(urlparse("https://www.reddit.com/gallery/1uxuqya")))

    async def test_share_link_resolves_to_canonical_post(self):
        client = _FakeClient({_SHARE_URL: (301, _CANONICAL_URL)})

        resolved = await _resolved_reddit_url(client, _SHARE_URL, cookies=None)

        self.assertEqual(resolved, _CANONICAL_URL)
        # The canonical target is post-shaped, so we stop without fetching it.
        self.assertEqual(client.requests, [_SHARE_URL])

    async def test_non_share_link_is_left_untouched(self):
        client = _FakeClient({})

        resolved = await _resolved_reddit_url(client, _CANONICAL_URL, cookies=None)

        self.assertEqual(resolved, _CANONICAL_URL)
        self.assertEqual(client.requests, [])

    async def test_resolution_failure_falls_back_to_original_url(self):
        # Appending .json makes Reddit bounce a share link to the bare subreddit,
        # which is not post-shaped; resolution must not accept it.
        client = _FakeClient(
            {
                _SHARE_URL: (307, "https://www.reddit.com/r/PiratedGames/"),
                "https://www.reddit.com/r/PiratedGames/": (200, None),
            }
        )

        resolved = await _resolved_reddit_url(client, _SHARE_URL, cookies=None)

        self.assertEqual(resolved, _SHARE_URL)

    def test_json_urls_from_resolved_post_target_correct_endpoint(self):
        self.assertEqual(
            _reddit_json_urls(_CANONICAL_URL, include_permalink=False),
            ("https://www.reddit.com/comments/1uxuqya.json?raw_json=1",),
        )


_IMAGE_POST_HTML = """
<html><head>
<meta property="og:title" content="Test Post Title" />
<meta property="og:description" content="Truncated preview..." />
<meta property="og:image" content="https://preview.redd.it/abc.png?width=1200&amp;s=x" />
</head><body>
<div class="thing link" data-fullname="t3_abc123" data-is-gallery="false"
     data-url="https://i.redd.it/pzup4a804jdh1.png"
     data-permalink="/r/test/comments/abc123/test_post/">
  <div class="entry">
    <div class="usertext-body"><div class="md"><p>Full body text here.</p></div></div>
  </div>
</div>
<div class="thing comment" data-fullname="t1_xyz" data-url="https://i.redd.it/from_a_comment.png">
  <a href="https://i.redd.it/from_a_comment.png">comment image</a>
</div>
</body></html>
"""

_GALLERY_POST_HTML = """
<html><head><meta property="og:title" content="Gallery Post" /></head><body>
<div class="thing link" data-fullname="t3_gal1" data-is-gallery="true"
     data-url="https://www.reddit.com/gallery/gal1"
     data-permalink="/r/test/comments/gal1/g/">
  <a href="https://i.redd.it/img1.jpg">one</a>
  <a href="https://i.redd.it/img2.jpg">two</a>
  <a href="https://i.redd.it/img1.jpg">dup of one</a>
</div>
</body></html>
"""


class RedditHtmlFallbackTests(unittest.TestCase):
    def test_image_post_extracts_direct_url_and_metadata(self):
        metadata = _metadata_from_old_reddit_html(_IMAGE_POST_HTML)

        self.assertIsNotNone(metadata)
        self.assertEqual(metadata.media_urls, ("https://i.redd.it/pzup4a804jdh1.png",))
        self.assertEqual(metadata.title, "Test Post Title")
        self.assertEqual(metadata.content, "Full body text here.")
        self.assertEqual(metadata.thumbnail, "https://i.redd.it/pzup4a804jdh1.png")
        self.assertEqual(metadata.permalink, "https://www.reddit.com/r/test/comments/abc123/test_post/")

    def test_comment_media_is_not_mistaken_for_post_media(self):
        metadata = _metadata_from_old_reddit_html(_IMAGE_POST_HTML)

        self.assertNotIn("https://i.redd.it/from_a_comment.png", metadata.media_urls)

    def test_gallery_post_extracts_ordered_deduped_images(self):
        metadata = _metadata_from_old_reddit_html(_GALLERY_POST_HTML)

        self.assertEqual(
            metadata.media_urls,
            ("https://i.redd.it/img1.jpg", "https://i.redd.it/img2.jpg"),
        )

    def test_text_only_post_yields_no_metadata(self):
        html = '<html><body><div class="thing" data-fullname="t3_x" data-url="/r/test/comments/x/"></div></body></html>'
        self.assertIsNone(_metadata_from_old_reddit_html(html))

    def test_old_reddit_url_from_permalink_swaps_host(self):
        self.assertEqual(
            _old_reddit_html_url("https://www.reddit.com/r/PiratedGames/comments/1uxuqya/title/"),
            "https://old.reddit.com/r/PiratedGames/comments/1uxuqya/title/",
        )

    def test_old_reddit_url_from_short_link_uses_post_id(self):
        self.assertEqual(
            _old_reddit_html_url("https://redd.it/1uxuqya"),
            "https://old.reddit.com/comments/1uxuqya/",
        )


if __name__ == "__main__":
    unittest.main()
