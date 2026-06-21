"""Regression tests for browser cookie export loading."""

import json
import tempfile
import time
import unittest
from pathlib import Path

from services.cookie_file import cookies_to_httpx, has_any_cookie_name, load_cookie_file


class CookieFileTests(unittest.TestCase):
    def test_loads_raw_browser_export(self):
        cookies = [
            {
                "name": "reddit_session",
                "value": "secret",
                "domain": ".reddit.com",
                "path": "/",
                "expirationDate": time.time() + 3600,
            }
        ]

        loaded = _load_json(cookies)

        self.assertTrue(has_any_cookie_name(loaded, {"reddit_session"}))
        jar = cookies_to_httpx(loaded)
        self.assertTrue(any(cookie.name == "reddit_session" for cookie in jar.jar))

    def test_filters_expired_cookies(self):
        cookies = [
            {
                "name": "reddit_session",
                "value": "expired",
                "domain": ".reddit.com",
                "path": "/",
                "expirationDate": time.time() - 10,
            }
        ]

        loaded = _load_json(cookies)

        self.assertFalse(has_any_cookie_name(loaded, {"reddit_session"}))


def _load_json(data):
    with tempfile.TemporaryDirectory() as temp_dir:
        path = Path(temp_dir) / "cookies.json"
        path.write_text(json.dumps(data), encoding="utf-8")
        return load_cookie_file(path)


if __name__ == "__main__":
    unittest.main()
