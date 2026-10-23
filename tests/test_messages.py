"""Regression tests for Telegram replies to extracted posts."""

import logging
import re
import unittest
from html import unescape
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
from telegram import InlineQueryResultArticle
from telegram.error import TimedOut

from core.types import MediaMetadata, MediaResult, TextResult
from handlers import messages
from utils.logging import RequestLabelFilter
from handlers.messages import (
    _inline_label,
    _message_label,
    MEDIA_CAPTION_LIMIT,
    TEXT_MESSAGE_LIMIT,
    _build_inline_results,
    _format_media_caption,
    handle_telegram_message,
)

_POST_URL = "https://www.facebook.com/groups/example.group/permalink/3880626025437561/"


def _rendered(html_text: str) -> str:
    """The text Telegram displays for an HTML message: tags and the link URL are dropped, entities decoded."""
    return unescape(re.sub(r"<[^>]+>", "", html_text))


def _units(text: str) -> int:
    """Telegram counts message length in UTF-16 code units, so an emoji is 2."""
    return len(text.encode("utf-16-le")) // 2


def _update() -> SimpleNamespace:
    return SimpleNamespace(
        message=SimpleNamespace(text=_POST_URL, message_id=7, reply_text=AsyncMock()),
        effective_user=SimpleNamespace(id=1, username="owner", first_name="Owner", last_name=None, is_bot=False),
        effective_chat=SimpleNamespace(id=1, type="private", title=None, username=None),
    )


def _callback(result):
    router = SimpleNamespace(handle=AsyncMock(return_value=result))
    access_control = MagicMock()
    access_control.is_user_allowed.return_value = True
    return handle_telegram_message(router, access_control)


def _text_result(caption: str | None, title: str | None = "Example Group | Hiring") -> TextResult:
    return TextResult(metadata=MediaMetadata(original_url=_POST_URL, caption=caption, title=title))


class TextOnlyReplyTests(unittest.IsolatedAsyncioTestCase):
    async def test_text_only_post_is_sent_as_text_with_a_source_link(self):
        update = _update()

        await _callback(_text_result("Hiring <b>now</b> & more"))(update, None)

        update.message.reply_text.assert_awaited_once()
        (text,), kwargs = update.message.reply_text.await_args
        self.assertEqual(text, f'Hiring &lt;b&gt;now&lt;/b&gt; &amp; more\n\n<a href="{_POST_URL}">Source</a>')
        self.assertEqual(kwargs["parse_mode"], "HTML")
        self.assertEqual(kwargs["reply_to_message_id"], 7)
        self.assertTrue(kwargs["link_preview_options"].is_disabled)

    async def test_text_without_a_caption_is_just_the_source_link(self):
        update = _update()

        await _callback(_text_result(None))(update, None)

        (text,), kwargs = update.message.reply_text.await_args
        self.assertEqual(text, f'<a href="{_POST_URL}">Source</a>')
        self.assertEqual(kwargs["parse_mode"], "HTML")

    async def test_long_post_fits_in_one_message(self):
        update = _update()

        await _callback(_text_result("Tom & Jerry's " * 600))(update, None)

        (text,), _ = update.message.reply_text.await_args
        self.assertLessEqual(_units(_rendered(text)), TEXT_MESSAGE_LIMIT)
        self.assertIn('<a href="', text)
        self.assertIn("...\n\n", text)

    async def test_media_fallback_still_replies_with_caption_and_source(self):
        update = _update()
        result = MediaResult(urls=("https://cdn.example/v.mp4",), metadata=MediaMetadata(_POST_URL, caption="clip"))

        with patch.object(messages, "deliver_media", AsyncMock(return_value=False)):
            await _callback(result)(update, None)

        (text,), kwargs = update.message.reply_text.await_args
        self.assertEqual(text, f'clip\n\n<a href="{_POST_URL}">Source</a>')
        self.assertEqual(kwargs["parse_mode"], "HTML")
        self.assertTrue(kwargs["link_preview_options"].is_disabled)


class _Capture(logging.Handler):
    """Collects records after the same label filter the real log handler uses."""

    def __init__(self):
        super().__init__(logging.DEBUG)
        self.addFilter(RequestLabelFilter())
        self.records: list[logging.LogRecord] = []

    def emit(self, record):
        self.records.append(record)


class LoggingTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.capture = _Capture()
        logger = logging.getLogger("handlers.messages")
        previous = logger.level
        logger.setLevel(logging.DEBUG)
        logger.addHandler(self.capture)
        self.addCleanup(logger.setLevel, previous)
        self.addCleanup(logger.removeHandler, self.capture)

    def test_message_and_inline_labels(self):
        self.assertEqual(_message_label(_update()), "1:7")
        self.assertIsNone(_message_label(SimpleNamespace(message=None, effective_chat=None)))
        self.assertEqual(_inline_label(SimpleNamespace(inline_query=SimpleNamespace(id="9876543210"))), "inline:543210")
        self.assertIsNone(_inline_label(SimpleNamespace(inline_query=None)))

    async def test_every_line_of_a_request_is_labelled_and_the_label_ends_with_it(self):
        await _callback(_text_result("hello"))(_update(), None)
        logging.getLogger("handlers.messages").info("after the request")

        *request_records, after = self.capture.records
        self.assertTrue(request_records)
        self.assertEqual({record.request for record in request_records}, {"1:7"})
        self.assertIsNone(after.request)

    async def test_outcome_line_has_sender_kind_source_and_timing(self):
        await _callback(_text_result("hello"))(_update(), None)

        (line,) = [r.getMessage() for r in self.capture.records if r.getMessage().startswith("Message from")]
        self.assertRegex(
            line,
            r"^Message from @owner \(1\) in .*: text-only Facebook post from "
            r"https://www\.facebook\.com/groups/example\.group/permalink/3880626025437561/ in \d+\.\ds\.$",
        )

    async def test_media_outcome_line_counts_the_media(self):
        result = MediaResult(urls=("https://cdn.example/v.mp4",), metadata=MediaMetadata(_POST_URL, caption="clip"))

        with patch.object(messages, "deliver_media", AsyncMock(return_value=True)):
            await _callback(result)(_update(), None)

        lines = [r.getMessage() for r in self.capture.records]
        self.assertTrue(any(re.search(r": 1 Facebook media from \S+ in \d+\.\ds\.$", line) for line in lines), lines)

    async def test_delivery_failure_reports_elapsed_time_and_cause(self):
        timed_out = TimedOut()
        timed_out.__cause__ = httpx.WriteTimeout("x")
        result = MediaResult(urls=("https://cdn.example/v.mp4",), metadata=MediaMetadata(_POST_URL, caption="clip"))

        with patch.object(messages, "deliver_media", AsyncMock(side_effect=timed_out)):
            await _callback(result)(_update(), None)

        (failure,) = [r for r in self.capture.records if r.levelno == logging.ERROR]
        self.assertRegex(
            failure.getMessage(), r"^Failed to deliver media from \S+ after \d+\.\ds: TimedOut \(WriteTimeout\)\.$"
        )


class InlineTextTests(unittest.TestCase):
    def test_text_post_becomes_one_article_result(self):
        (article,) = _build_inline_results(_text_result("Hiring now"))

        self.assertIsInstance(article, InlineQueryResultArticle)
        self.assertEqual(article.title, "Example Group | Hiring")
        self.assertEqual(article.description, "Hiring now")
        self.assertEqual(article.input_message_content.parse_mode, "HTML")
        self.assertIn("Hiring now", article.input_message_content.message_text)

    def test_untitled_post_gets_a_default_title(self):
        (article,) = _build_inline_results(_text_result("Hiring now", title=None))

        self.assertEqual(article.title, "Post")


class MessageBudgetTests(unittest.TestCase):
    """The limit applies to the displayed text, so markup and escapes must not eat into it."""

    def test_entity_escapes_and_the_link_markup_cost_nothing(self):
        text = _format_media_caption("&" * 5000, _POST_URL, TEXT_MESSAGE_LIMIT)

        self.assertIn("&amp;", text)
        self.assertEqual(_units(_rendered(text)), TEXT_MESSAGE_LIMIT)

    def test_post_text_uses_all_the_room_the_source_link_leaves(self):
        text = _format_media_caption("x" * 5000, _POST_URL, TEXT_MESSAGE_LIMIT)

        body, link = text.split("\n\n")
        self.assertEqual(len(body), TEXT_MESSAGE_LIMIT - len("\n\n") - len("Source"))
        self.assertTrue(body.endswith("x..."))
        self.assertTrue(link.startswith("<a href="))

    def test_text_that_fits_is_left_alone(self):
        self.assertEqual(
            _format_media_caption("hello & bye", _POST_URL, TEXT_MESSAGE_LIMIT),
            f'hello &amp; bye\n\n<a href="{_POST_URL}">Source</a>',
        )

    def test_emoji_count_as_two_units_so_the_message_never_exceeds_the_limit(self):
        text = _format_media_caption("\U0001f680" * 3000, _POST_URL, TEXT_MESSAGE_LIMIT)

        units = _units(_rendered(text))
        self.assertLessEqual(units, TEXT_MESSAGE_LIMIT)
        self.assertGreaterEqual(units, TEXT_MESSAGE_LIMIT - 2)
        text.encode("utf-8")  # a cut through a surrogate pair would raise here

    def test_media_captions_get_the_same_treatment_at_their_own_limit(self):
        text = _format_media_caption("&'\"<>" * 1000, _POST_URL)

        self.assertEqual(_units(_rendered(text)), MEDIA_CAPTION_LIMIT)

    def test_without_a_source_link_the_text_alone_fills_the_limit(self):
        text = _format_media_caption("x" * 5000, None, TEXT_MESSAGE_LIMIT)

        self.assertEqual(_units(_rendered(text)), TEXT_MESSAGE_LIMIT)

    def test_cut_ends_on_a_word_without_a_dangling_space(self):
        text = _format_media_caption("word " * 2000, _POST_URL, TEXT_MESSAGE_LIMIT)

        self.assertTrue(text.split("\n\n")[0].endswith("word..."))

    def test_missing_text_and_link_gives_nothing(self):
        self.assertIsNone(_format_media_caption(None, None))
        self.assertEqual(_format_media_caption(None, _POST_URL), f'<a href="{_POST_URL}">Source</a>')


if __name__ == "__main__":
    unittest.main()
