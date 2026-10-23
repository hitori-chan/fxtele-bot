"""Regression tests for URL helpers that keep secrets out of logs."""

import unittest

from utils.text import redact_url_credentials, safe_log_url


class RedactUrlCredentialsTests(unittest.TestCase):
    def test_removes_user_and_password_but_keeps_host_and_port(self):
        self.assertEqual(redact_url_credentials("socks5://user:p%40ss@127.0.0.1:40000"), "socks5://127.0.0.1:40000")

    def test_user_without_password_is_removed_too(self):
        self.assertEqual(redact_url_credentials("http://token@proxy.test:8080/x"), "http://proxy.test:8080/x")

    def test_ipv6_hosts_stay_bracketed(self):
        self.assertEqual(redact_url_credentials("http://u:p@[::1]:3128"), "http://[::1]:3128")

    def test_urls_without_credentials_are_untouched(self):
        self.assertEqual(redact_url_credentials("socks5://127.0.0.1:40000"), "socks5://127.0.0.1:40000")


class SafeLogUrlTests(unittest.TestCase):
    def test_drops_share_tracking_query_and_fragment(self):
        self.assertEqual(
            safe_log_url("https://www.instagram.com/reel/AbC/?igsh=PERSONAL-ID&utm=x#frag"),
            "https://www.instagram.com/reel/AbC/",
        )

    def test_unparseable_urls_are_masked(self):
        self.assertEqual(safe_log_url("http://[bad"), "<invalid-url>")


if __name__ == "__main__":
    unittest.main()
