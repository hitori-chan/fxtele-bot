"""Regression tests for Facebook media extraction scoping."""

import unittest

from handlers.media_extractors.facebook import _extract_media_candidates


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


if __name__ == "__main__":
    unittest.main()
