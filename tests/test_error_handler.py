"""Regression tests for Telegram error logging."""

import unittest
from types import SimpleNamespace

import httpx
from telegram.error import BadRequest, Conflict, NetworkError, TimedOut

from handlers.errors import handle_error
from utils.telegram_errors import describe_error


def _context(error: Exception) -> SimpleNamespace:
    return SimpleNamespace(error=error)


def _caused_by(error: Exception, cause: Exception) -> Exception:
    error.__cause__ = cause
    return error


class DescribeErrorTests(unittest.TestCase):
    def test_includes_the_transport_error_behind_a_timeout(self):
        error = _caused_by(TimedOut(), httpx.WriteTimeout("secret-token"))

        self.assertEqual(describe_error(error), "TimedOut (WriteTimeout)")

    def test_plain_error_is_just_its_class_name(self):
        self.assertEqual(describe_error(ValueError("secret-token")), "ValueError")


class HandleErrorTests(unittest.IsolatedAsyncioTestCase):
    async def test_polling_network_error_is_a_one_line_warning(self):
        error = _caused_by(NetworkError("httpx.ReadError: secret-token"), httpx.ReadError("secret-token"))

        with self.assertLogs("handlers.errors", level="WARNING") as logs:
            await handle_error(None, _context(error))

        (record,) = logs.records
        self.assertEqual(record.levelname, "WARNING")
        self.assertEqual(record.getMessage(), "Telegram polling network error: NetworkError (ReadError); retrying.")
        self.assertIsNone(record.exc_info)

    async def test_bad_request_without_an_update_still_logs_a_traceback(self):
        with self.assertLogs("handlers.errors", level="ERROR") as logs:
            await handle_error(None, _context(BadRequest("Chat not found")))

        (record,) = logs.records
        self.assertEqual(record.getMessage(), "Unhandled Telegram update error: BadRequest.")
        self.assertIsNotNone(record.exc_info)

    async def test_network_error_raised_while_handling_an_update_still_logs_a_traceback(self):
        with self.assertLogs("handlers.errors", level="ERROR") as logs:
            await handle_error(object(), _context(NetworkError("httpx.ReadError: boom")))

        (record,) = logs.records
        self.assertEqual(record.getMessage(), "Unhandled Telegram update error: NetworkError.")
        self.assertIsNotNone(record.exc_info)

    async def test_polling_conflict_keeps_its_dedicated_message(self):
        with self.assertLogs("handlers.errors", level="ERROR") as logs:
            await handle_error(None, _context(Conflict("terminated by other getUpdates request")))

        self.assertEqual(
            logs.records[0].getMessage(),
            "Telegram polling conflict: another bot instance is using this token.",
        )


if __name__ == "__main__":
    unittest.main()
