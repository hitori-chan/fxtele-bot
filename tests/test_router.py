"""Regression tests for router error logging."""

import unittest
from types import SimpleNamespace

from core.router import MessageRouter


class _Exploding:
    name = "exploding"

    async def handle(self, text):
        raise RuntimeError("bug inside a handler")


class RouterLogTests(unittest.IsolatedAsyncioTestCase):
    async def test_unexpected_handler_failure_is_logged_with_a_traceback_and_swallowed(self):
        router = MessageRouter([_Exploding(), SimpleNamespace(name="quiet", handle=self._none)])

        with self.assertLogs("core.router", level="ERROR") as logs:
            result = await router.handle("anything")

        self.assertIsNone(result)
        (record,) = logs.records
        self.assertEqual(record.getMessage(), "Handler exploding failed.")
        self.assertIs(record.exc_info[0], RuntimeError)

    @staticmethod
    async def _none(text):
        return None


if __name__ == "__main__":
    unittest.main()
