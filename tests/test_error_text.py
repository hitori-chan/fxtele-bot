"""Regression tests for the compact error descriptions used in logs."""

import unittest

import httpx
from telegram.error import NetworkError, TimedOut

from utils.error_text import describe_error


def _caused_by(error: Exception, *causes: Exception) -> Exception:
    """Chain error -> causes[0] -> causes[1] ... the way raise ... from ... does."""
    current = error
    for cause in causes:
        current.__cause__ = cause
        current = cause
    return error


class DescribeErrorTests(unittest.TestCase):
    def test_timeout_names_which_httpx_timeout_it_was(self):
        error = _caused_by(TimedOut(), httpx.WriteTimeout("secret-token"), TimeoutError())

        self.assertEqual(describe_error(error), "TimedOut (WriteTimeout)")

    def test_http_status_errors_report_the_status_code(self):
        request = httpx.Request("GET", "https://example.test/post?token=SECRET")
        error = httpx.HTTPStatusError("boom", request=request, response=httpx.Response(403, request=request))

        self.assertEqual(describe_error(error), "HTTPStatusError (403)")

    def test_connection_errors_name_the_os_error(self):
        refused = ConnectionRefusedError(111, "Connection refused")
        error = _caused_by(
            httpx.ConnectError("x"), type("ConnectError", (Exception,), {})("same name as outer"), refused
        )

        self.assertEqual(describe_error(error), "ConnectError (ConnectionRefusedError/ECONNREFUSED)")

    def test_reset_connections_keep_the_whole_story(self):
        error = _caused_by(NetworkError("x"), httpx.ReadError("y"), ConnectionResetError(104, "reset"))

        self.assertEqual(describe_error(error), "NetworkError (ReadError > ConnectionResetError/ECONNRESET)")

    def test_os_errors_raised_directly_carry_their_errno_name(self):
        self.assertEqual(describe_error(OSError(28, "No space left on device")), "OSError/ENOSPC")
        self.assertEqual(describe_error(PermissionError(13, "denied: /secret/path")), "PermissionError/EACCES")

    def test_os_errors_without_a_known_errno_stay_plain(self):
        self.assertEqual(describe_error(OSError("no errno")), "OSError")

    def test_plain_error_is_just_its_class_name_and_never_its_text(self):
        self.assertEqual(describe_error(ValueError("secret-token")), "ValueError")

    def test_a_cause_loop_cannot_hang_it(self):
        first, second = ValueError("a"), KeyError("b")
        first.__cause__, second.__cause__ = second, first

        self.assertEqual(describe_error(first), "ValueError (KeyError)")


if __name__ == "__main__":
    unittest.main()
