"""Regression tests for Facebook media extraction scoping."""

import json
import unittest

from core.types import MediaResult, TextResult
from handlers.media_extractors.facebook import (
    _canonical_share_target,
    _extract_facebook_media,
    _extract_media_candidates,
)


def _story_document(story_token: str, media_id: str = "photo-1") -> dict:
    return {
        "node_v2": {
            "url": f"https://www.facebook.com/example/posts/{story_token}",
            "attachments": [
                {
                    "styles": {
                        "attachment": {
                            "media": {
                                "id": media_id,
                                "__typename": "Photo",
                                "image": {"uri": f"https://scontent.example/{media_id}.jpg"},
                            }
                        }
                    }
                }
            ],
        }
    }


def _story_video_document(story_token: str, video_id: str = "video-1") -> dict:
    return {
        "node_v2": {
            "post_id": story_token,
            "attachments": [
                {
                    "media": {"__typename": "Video", "id": video_id},
                    "styles": {
                        "attachment": {
                            "media": {
                                "__typename": "Video",
                                "id": video_id,
                                "thumbnailImage": {"uri": f"https://scontent.example/{video_id}.jpg"},
                            },
                        }
                    },
                }
            ],
        },
        "all_video_dash_prefetch_representations": [
            {
                "video_id": video_id,
                "representations": [
                    {
                        "mime_type": "video/mp4",
                        "base_url": f"https://video.example/{video_id}-low.mp4",
                        "bandwidth": 100,
                    },
                    {
                        "mime_type": "video/mp4",
                        "base_url": f"https://video.example/{video_id}-high.mp4",
                        "bandwidth": 200,
                    },
                ],
            }
        ],
    }


class FacebookStoryExtractionTests(unittest.TestCase):
    def test_opaque_share_video_url_does_not_extract_unscoped_feed_media(self):
        candidates = _extract_media_candidates(
            [_story_document("latest-feed-story")],
            "https://www.facebook.com/share/v/1JE8AhF9Fj/",
        )

        self.assertEqual(candidates, [])

    def test_story_url_extracts_media_when_url_story_token_matches(self):
        candidates = _extract_media_candidates(
            [_story_document("pfbid-target")],
            "https://www.facebook.com/example/posts/pfbid-target",
        )

        self.assertEqual([candidate.url for candidate in candidates], ["https://scontent.example/photo-1.jpg"])

    def test_share_url_extracts_media_when_route_story_token_matches(self):
        candidates = _extract_media_candidates(
            [_story_document("resolved-story")],
            "https://www.facebook.com/share/v/1JE8AhF9Fj/",
            story_tokens=("resolved-story",),
        )

        self.assertEqual([candidate.url for candidate in candidates], ["https://scontent.example/photo-1.jpg"])

    def test_story_video_uses_only_video_id_referenced_by_scoped_story(self):
        candidates = _extract_media_candidates(
            [
                _story_video_document("target-story", "target-video"),
                _story_video_document("other-story", "other-video"),
            ],
            "https://www.facebook.com/groups/example/permalink/target-story",
        )

        self.assertEqual([candidate.url for candidate in candidates], ["https://video.example/target-video-high.mp4"])


class FacebookShareRedirectTests(unittest.TestCase):
    def test_story_php_redirect_maps_to_posts_permalink(self):
        target = _canonical_share_target(
            "https://www.facebook.com/story.php?story_fbid=2115533999314371"
            "&id=100025733741330&rdid=vUB8ON1Kouie72XJ"
        )

        self.assertEqual(target, "https://www.facebook.com/100025733741330/posts/2115533999314371")

    def test_permalink_php_redirect_maps_to_posts_permalink(self):
        target = _canonical_share_target(
            "https://www.facebook.com/permalink.php?story_fbid=pfbid02PBxxU4T5E8J249k4Shb7sf"
            "deGN6iD9EXT9pMouUmYUaCLKin2tbyA1Y8VUWb6oi1l&id=61586903485431&rdid=ORUBA7KBkJVYBv96"
        )

        self.assertEqual(
            target,
            "https://www.facebook.com/61586903485431/posts/"
            "pfbid02PBxxU4T5E8J249k4Shb7sfdeGN6iD9EXT9pMouUmYUaCLKin2tbyA1Y8VUWb6oi1l",
        )

    def test_story_php_without_both_ids_is_not_resolved(self):
        self.assertIsNone(_canonical_share_target("https://www.facebook.com/story.php?story_fbid=2115533999314371"))

    def test_non_story_php_redirect_is_not_resolved(self):
        self.assertIsNone(
            _canonical_share_target("https://www.facebook.com/login/?next=https%3A%2F%2Fwww.facebook.com%2F")
        )


if __name__ == "__main__":
    unittest.main()


_GROUP_POST_URL = "https://www.facebook.com/groups/example.group/permalink/3880626025437561/"


def _page(*documents: dict, title: str = "Example Group | Hello") -> str:
    """Build a Facebook-shaped page whose structured JSON blocks are the given bbox payloads."""
    scripts = "".join(
        f'<script type="application/json" data-sjs>{json.dumps({"__bbox": {"result": {"data": document}}})}</script>'
        for document in documents
    )
    return f"<html><head><title>{title}</title><meta property='og:title' content='{title}'></head><body>{scripts}</body></html>"


def _group_story(
    token: str = "3880626025437561", *, attachments=None, text: str | None = "Hiring: security researcher"
):
    story: dict = {"post_id": token}
    if attachments is not None:
        story["attachments"] = attachments
    if text is not None:
        story["comet_sections"] = {"content": {"story": {"message": {"text": text}}}}
    return {"node_v2": story}


class FacebookTextOnlyPostTests(unittest.TestCase):
    def test_post_confirmed_to_have_no_attachments_returns_its_text(self):
        html = _page(_group_story(attachments=[]), title="Example Group | Hiring")

        result = _extract_facebook_media(html, _GROUP_POST_URL)

        self.assertIsInstance(result, TextResult)
        self.assertEqual(result.metadata.caption, "Hiring: security researcher")
        self.assertEqual(result.metadata.original_url, _GROUP_POST_URL)
        self.assertEqual(result.metadata.title, "Example Group | Hiring")

    def test_post_with_media_is_still_a_media_result(self):
        html = _page(_story_document("3880626025437561"))

        result = _extract_facebook_media(html, "https://www.facebook.com/example/posts/3880626025437561")

        self.assertIsInstance(result, MediaResult)
        self.assertEqual(result.urls, ("https://scontent.example/photo-1.jpg",))

    def test_missing_attachments_field_is_not_taken_as_a_text_post(self):
        for html in (_page(_group_story(attachments=None)), _page(_group_story(attachments=[], text=None))):
            with self.assertLogs("handlers.media_extractors.facebook", level="WARNING") as logs:
                result = _extract_facebook_media(html, _GROUP_POST_URL)

            self.assertIsNone(result)
            self.assertIn("No structured Facebook media found", logs.records[0].getMessage())

    def test_another_posts_text_is_never_used(self):
        html = _page(_group_story("9999999999999999", attachments=[], text="someone else's post"))

        with self.assertLogs("handlers.media_extractors.facebook", level="WARNING"):
            self.assertIsNone(_extract_facebook_media(html, _GROUP_POST_URL))

    def test_page_without_the_post_is_a_failure_not_a_text_post(self):
        with self.assertLogs("handlers.media_extractors.facebook", level="WARNING"):
            self.assertIsNone(_extract_facebook_media("<html><body>Log in to continue</body></html>", _GROUP_POST_URL))

    def test_missing_media_on_a_reel_stays_a_failure(self):
        html = _page(
            {
                "node_v2": {
                    "post_id": "123",
                    "attachments": [],
                    "comet_sections": {"content": {"story": {"message": {"text": "t"}}}},
                }
            }
        )

        with self.assertLogs("handlers.media_extractors.facebook", level="WARNING"):
            self.assertIsNone(_extract_facebook_media(html, "https://www.facebook.com/reel/123"))
