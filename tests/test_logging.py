"""Regression tests for log configuration and request labelling."""

import asyncio
import io
import logging
import os
import re
import sys
import unittest
from unittest.mock import patch

from rich.console import Console

import utils.logging as app_logging
from utils.logging import CompactRichHandler, RequestLabelFilter, request_logging, resolve_level, setup_logging

_LOGGERS = ("httpx", "httpcore", "hpack", "h2", "asyncio", "telegram", "main")


class ResolveLevelTests(unittest.TestCase):
    def test_level_names_are_case_and_space_insensitive(self):
        self.assertEqual(resolve_level(" debug "), (logging.DEBUG, True))
        self.assertEqual(resolve_level("WARNING"), (logging.WARNING, True))

    def test_unset_or_empty_means_the_default(self):
        self.assertEqual(resolve_level(None), (logging.INFO, True))
        self.assertEqual(resolve_level(""), (logging.INFO, True))

    def test_numbers_pass_through(self):
        self.assertEqual(resolve_level(logging.ERROR), (logging.ERROR, True))

    def test_unknown_names_fall_back_and_are_flagged(self):
        self.assertEqual(resolve_level("verbose"), (logging.INFO, False))


class SetupLoggingTests(unittest.TestCase):
    def setUp(self):
        root = logging.getLogger()
        saved = (root.handlers[:], root.level, {name: logging.getLogger(name).level for name in _LOGGERS})

        def restore():
            root.handlers[:] = saved[0]
            root.setLevel(saved[1])
            for name, level in saved[2].items():
                logging.getLogger(name).setLevel(level)

        self.addCleanup(restore)

    def _setup(self, env_level: str | None = None, **kwargs) -> str:
        env = {} if env_level is None else {app_logging.LOG_LEVEL_ENV: env_level}
        with patch.dict(os.environ, env, clear=False), patch.object(sys, "stdout", io.StringIO()) as out:
            if env_level is None:
                os.environ.pop(app_logging.LOG_LEVEL_ENV, None)
            setup_logging(**kwargs)
        return out.getvalue()

    def test_log_level_comes_from_the_environment(self):
        self._setup("debug")

        self.assertEqual(logging.getLogger().level, logging.DEBUG)

    def test_default_level_is_info(self):
        self._setup()

        self.assertEqual(logging.getLogger().level, logging.INFO)

    def test_argument_beats_the_environment(self):
        self._setup("DEBUG", level="ERROR")

        self.assertEqual(logging.getLogger().level, logging.ERROR)

    def test_debug_stays_quiet_about_http_internals_and_telegram_payloads(self):
        self._setup("DEBUG")

        for name in ("httpx", "httpcore", "hpack", "h2", "asyncio"):
            self.assertEqual(logging.getLogger(name).level, logging.WARNING, name)
        self.assertEqual(logging.getLogger("telegram").level, logging.INFO)

    def test_a_stricter_level_also_applies_to_telegram(self):
        self._setup("ERROR")

        self.assertEqual(logging.getLogger("telegram").level, logging.ERROR)

    def test_unusable_level_is_reported_not_fatal(self):
        output = self._setup("verbose")

        self.assertEqual(logging.getLogger().level, logging.INFO)
        self.assertIn("Unknown LOG_LEVEL 'verbose'; using INFO.", output)


class RequestLabelTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.output = io.StringIO()
        handler = CompactRichHandler(Console(file=self.output, width=300, force_terminal=False, color_system=None))
        handler.setFormatter(logging.Formatter())
        handler.addFilter(RequestLabelFilter())
        self.logger = logging.getLogger("test.labels")
        self.logger.propagate = False
        self.logger.setLevel(logging.DEBUG)
        self.logger.addHandler(handler)
        self.addCleanup(self.logger.removeHandler, handler)

    def lines(self) -> list[str]:
        return self.output.getvalue().splitlines()

    def test_lines_inside_a_request_carry_its_label_after_the_logger_name(self):
        with request_logging("42:7"):
            self.logger.info("hello")

        (line,) = self.lines()
        self.assertRegex(line, r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d INFO\s+test\.labels \[42:7\] hello$")

    def test_lines_outside_a_request_have_no_label(self):
        self.logger.info("startup")

        (line,) = self.lines()
        self.assertNotIn("[", line)
        self.assertTrue(line.endswith("test.labels startup"))

    def test_label_does_not_leak_past_the_request_even_after_an_error(self):
        with self.assertRaises(RuntimeError), request_logging("1:1"):
            raise RuntimeError

        self.logger.info("after")

        self.assertNotIn("[1:1]", self.lines()[-1])

    def test_nested_labels_restore_the_outer_one(self):
        with request_logging("outer"):
            with request_logging("inner"):
                pass
            self.logger.info("back")

        self.assertIn("[outer]", self.lines()[-1])

    async def test_label_follows_tasks_the_request_starts(self):
        async def child():
            self.logger.info("from child task")

        with request_logging("9:9"):
            await asyncio.create_task(child())

        self.assertIn("[9:9] from child task", self.lines()[-1])

    async def test_concurrent_requests_keep_their_own_labels(self):
        async def request(label: str):
            with request_logging(label):
                for step in range(3):
                    self.logger.info("%s step %d", label, step)
                    await asyncio.sleep(0)

        await asyncio.gather(request("a:1"), request("b:2"))

        lines = self.lines()
        self.assertEqual(len(lines), 6)
        for line in lines:
            label = re.search(r"\[(\w:\d)\]", line).group(1)
            self.assertIn(f"{label} step", line, "a line carries another request's label")

    def test_a_none_label_means_no_label(self):
        with request_logging(None):
            self.logger.info("plain")

        self.assertNotIn("[", self.lines()[0])


if __name__ == "__main__":
    unittest.main()
