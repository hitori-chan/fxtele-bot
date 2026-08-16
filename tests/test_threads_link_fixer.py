"""Regression tests for Threads share-link resolution and domain rewriting."""

import unittest

import httpx

from handlers.link_fixers.threads import ThreadsLinkFixer, _is_share_link, _resolve_share_link

_SHARE_URL = "https://www.threads.com/share/FV0GhI4-s/"
_CANONICAL_URL = (
    "https://www.threads.com/@chacco.o/post/DcGJ_NkEuhx"
    "?xmt=AQG0Zt-Ab8ixjKqQuM6aq1MU52pw5KAcWgnSGjoa_EAzOl6gVn8yNg-8OYCWOJBUU_LmLL4"
)
_DIRECT_URL = "https://www.threads.com/@zuck/post/abc123"


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


class ThreadsShareLinkTests(unittest.IsolatedAsyncioTestCase):
    def test_is_share_link_recognizes_share_shape(self):
        self.assertTrue(_is_share_link(_SHARE_URL))

    def test_is_share_link_rejects_canonical_post(self):
        self.assertFalse(_is_share_link(_CANONICAL_URL))

    async def test_share_link_resolves_to_canonical_post(self):
        client = _FakeClient({_SHARE_URL: (302, _CANONICAL_URL)})

        resolved = await _resolve_share_link(client, _SHARE_URL)

        self.assertEqual(resolved, _CANONICAL_URL)
        # The canonical target carries a single-use token; fetching it again
        # would invalidate it, so resolution must stop without requesting it.
        self.assertEqual(client.requests, [_SHARE_URL])

    async def test_resolution_failure_falls_back_to_original_url(self):
        # A share link that never leaves a client-rendered shell (no redirect)
        # must not be treated as resolved.
        client = _FakeClient({_SHARE_URL: (200, None)})

        resolved = await _resolve_share_link(client, _SHARE_URL)

        self.assertEqual(resolved, _SHARE_URL)


class ThreadsLinkFixerTests(unittest.IsolatedAsyncioTestCase):
    async def test_direct_post_link_is_rewritten_without_http(self):
        fixer = ThreadsLinkFixer()

        result = await fixer.handle(f"check this {_DIRECT_URL} out")

        self.assertEqual(result.content, "check this https://fzthreads.com/@zuck/post/abc123 out")

    async def test_non_threads_text_is_untouched(self):
        fixer = ThreadsLinkFixer()

        self.assertIsNone(await fixer.handle("no threads link here"))
