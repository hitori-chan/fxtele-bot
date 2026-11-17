//! A post's counts: reactions, comments, shares and views.
//!
//! They sit on the post's `feedback` object, whose `id` is base64 of `feedback:<post id>`. A page
//! carries other posts' feedback too (related videos, the creator's other reels), and the same
//! feedback turns up in several places with different fields, so the post's own feedback ID is
//! found first, then the counts are read from every copy of it.

use std::collections::VecDeque;

use serde_json::Value;

use super::index::{Index, Obj, get, id_text, text};
use crate::extract::Stats;

/// `feedback:` in base64: nine bytes, so it's whole and every feedback ID starts with it.
const FEEDBACK: &str = "ZmVlZGJhY2s6";

pub fn of(index: &Index<'_>, own: &[&Obj], video_id: Option<&str>) -> Stats {
    let Some(id) = feedback_id(index, own, video_id) else { return Stats::default() };
    let copies: Vec<&Obj> = index.with_id(id).collect();
    let first = |paths: &[&str]| copies.iter().find_map(|copy| paths.iter().find_map(|path| count(copy, path)));
    Stats {
        reactions: first(&["reaction_count", "unified_reactors", "reactors", "i18n_reaction_count"]),
        comments: first(&[
            "comment_rendering_instance.comments.total_count",
            "comments_count_summary_renderer.feedback.comment_rendering_instance.comments.total_count",
            "total_comment_count",
        ]),
        shares: first(&["share_count", "share_count_reduced", "i18n_share_count"]),
        // Facebook shows plays where it says so, and views otherwise.
        views: copies.iter().find_map(|copy| match get(copy, "should_show_play_count").and_then(Value::as_bool) {
            Some(true) => count(copy, "play_count"),
            _ => count(copy, "video_view_count"),
        }),
    }
}

/// The post's feedback: for a video, the one counting its views (`associated_video`); otherwise the
/// shallowest one in the post's own nodes, as a shared post's sits deeper inside its attachment.
fn feedback_id<'a>(index: &Index<'a>, own: &[&'a Obj], video_id: Option<&str>) -> Option<&'a str> {
    let for_video = video_id.and_then(|video| {
        index.nodes.iter().find_map(|node| {
            let counts_it = get(node, "associated_video.id").and_then(id_text).as_deref() == Some(video);
            text(node, "id").filter(|id| counts_it && id.starts_with(FEEDBACK))
        })
    });
    for_video.or_else(|| {
        // Breadth first: each node's own `feedback` before anything nested deeper.
        let mut queue: VecDeque<&Obj> = own.iter().copied().collect();
        while let Some(node) = queue.pop_front() {
            if let Some(id) = get(node, "feedback.id").and_then(Value::as_str).filter(|id| id.starts_with(FEEDBACK)) {
                return Some(id);
            }
            for child in node.values() {
                match child {
                    Value::Object(object) => queue.push_back(object),
                    Value::Array(items) => queue.extend(items.iter().filter_map(Value::as_object)),
                    _ => {}
                }
            }
        }
        None
    })
}

/// A count at `path`: a number, `{"count": n}`, or text as Facebook writes it (`1.5K`, `3,204`).
fn count(node: &Obj, path: &str) -> Option<u64> {
    match get(node, path)? {
        Value::Number(n) => n.as_u64(),
        Value::Object(o) => o.get("count").and_then(Value::as_u64),
        Value::String(s) => crate::extract::short_count(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn feedback(post: &str) -> String {
        data_encoding::BASE64.encode(format!("feedback:{post}").as_bytes())
    }

    #[test]
    fn counts_come_from_the_posts_own_feedback() {
        let (own, other) = (feedback("pfbid0own"), feedback("pfbid0other"));
        let page = json!([
            { "id": "story", "feedback": { "id": own },
              "attachments": [{ "feedback": { "id": other } }] },
            { "id": other, "reaction_count": { "count": 999 } },
            { "id": own, "reaction_count": { "count": 1504 }, "i18n_share_count": "1.2K" },
            { "id": own, "comment_rendering_instance": { "comments": { "total_count": 12 } } },
        ]);
        let index = Index::new([&page]);
        let story: Vec<&Obj> = index.with_id("story").collect();
        let stats = of(&index, &story, None);
        assert_eq!(stats, Stats { reactions: Some(1504), comments: Some(12), shares: Some(1200), views: None });
    }

    #[test]
    fn a_video_uses_the_feedback_counting_it() {
        let (own, other) = (feedback("pfbid0video"), feedback("pfbid0related"));
        let page = json!([
            { "id": other, "associated_video": { "id": "2" }, "video_view_count": 5 },
            { "id": own, "associated_video": { "id": "1" }, "should_show_play_count": false,
              "video_view_count": 1013, "play_count": 3958, "unified_reactors": { "count": 320 } },
        ]);
        let index = Index::new([&page]);
        let stats = of(&index, &[], Some("1"));
        assert_eq!((stats.views, stats.reactions), (Some(1013), Some(320)));
    }

    #[test]
    fn no_feedback_no_counts() {
        let page = json!({ "id": "story", "feedback": { "id": "not base64 feedback" } });
        let index = Index::new([&page]);
        let story: Vec<&Obj> = index.with_id("story").collect();
        assert_eq!(of(&index, &story, None), Stats::default());
    }

    #[test]
    fn short_counts() {
        use crate::extract::short_count;
        assert_eq!(short_count("1.5K"), Some(1500));
        assert_eq!(short_count("3,204"), Some(3204));
        assert_eq!(short_count("2M"), Some(2_000_000));
        assert_eq!(short_count("434"), Some(434));
        // As the account's language writes them, with cookies.
        assert_eq!(short_count("1,2K"), Some(1200));
        assert_eq!(short_count("1.234"), Some(1234));
        assert_eq!(short_count("1.234.567"), Some(1_234_567));
        assert_eq!(short_count("1\u{a0}234"), Some(1234));
        assert_eq!(short_count("1.234,5"), Some(1235));
        assert_eq!(short_count("12.5"), Some(13));
        assert_eq!(short_count("1,5 M"), Some(1_500_000));
        assert_eq!(short_count("many"), None);
        assert_eq!(short_count("-1"), None);
        assert_eq!(short_count("inf"), None);
    }
}
