//! Captions, titles, authors, post time, text-only posts, profiles and group posts behind videos.

use std::borrow::Cow;

use super::index::{Index, Obj, each, first_text, get, id_text, number, text, walk_obj};
use super::page::Page;

/// Caption lookups anywhere in the post data, in priority order.
const CAPTIONS: [&str; 5] = [
    "node_v2.comet_sections.content.story.message.text",
    "node_v2.comet_sections.content.story.comet_sections.message.story.message.text",
    "node_v2.comet_sections.content.story.comet_sections.message_container.story.message.text",
    "video.creation_story.message.text",
    "currMedia.creation_story.message.text",
];

const GROUP_NAME: [&str; 4] = [
    "to.name",
    "feedback.associated_group.name",
    "feedback.associated_group.if_viewer_can_see_reel_group_attribution.name",
    "feedback.associated_group.if_viewer_can_see_reel_group_attribution.if_viewer_can_see_reel_group_attribution.name",
];

/// The first caption found, trying each lookup over all `nodes` before the next.
pub fn caption<'a>(nodes: &[&'a Obj]) -> Option<&'a str> {
    CAPTIONS.iter().find_map(|path| nodes.iter().find_map(|node| text(node, path)))
}

/// The caption on the post's own nodes: their `message.text` (photos, the story a video
/// belongs to), then the caption lookups on them.
pub fn own_caption<'a>(nodes: &[&'a Obj]) -> Option<&'a str> {
    nodes.iter().find_map(|node| text(node, "message.text")).or_else(|| caption(nodes))
}

/// The story a video belongs to: nodes with a post time whose attachments include the video,
/// and every other part of those stories (Facebook splits one story across several query
/// results under the same ID, and only some carry the caption). Logged-in reel and video pages
/// keep the caption, post time and author there.
pub fn video_story<'a>(index: &Index<'a>, video_id: &str) -> Vec<&'a Obj> {
    let mut parts: Vec<&'a Obj> = index
        .nodes
        .iter()
        .copied()
        .filter(|node| {
            node.contains_key("creation_time")
                && each(node, "attachments", "media").any(|media| is_video(media, video_id))
        })
        .collect();
    let ids: Vec<Cow<'a, str>> = parts.iter().filter_map(|node| node.get("id").and_then(json_id)).collect();
    for id in &ids {
        for node in index.with_id(id) {
            if !parts.iter().any(|known| std::ptr::eq(*known, node)) {
                parts.push(node);
            }
        }
    }
    parts
}

/// Whose stories a story card belongs to: the bucket's owner.
pub fn story_owner<'a>(index: &Index<'a>, bucket: &str) -> Option<&'a str> {
    index.with_id(bucket).find_map(|node| text(node, "story_bucket_owner.name"))
}

/// Drops engagement counts from a title: "44K views · 3.6K reactions | Someone on Reels"
/// becomes "Someone on Reels". They go stale, and Telegram keeps previews for a long time.
pub fn clean_title(title: &str) -> String {
    let kept: Vec<&str> =
        title.split(" | ").map(str::trim).filter(|part| !part.is_empty() && !is_counts(part)).collect();
    if kept.is_empty() { title.trim().to_string() } else { kept.join(" | ") }
}

/// "44K views · 3.6K reactions", "1,234 likes".
fn is_counts(part: &str) -> bool {
    const WORDS: [&str; 12] = [
        "view",
        "views",
        "play",
        "plays",
        "reaction",
        "reactions",
        "like",
        "likes",
        "comment",
        "comments",
        "share",
        "shares",
    ];
    part.split('·').all(|piece| {
        let mut words = piece.split_whitespace();
        let (Some(number), Some(word), None) = (words.next(), words.next(), words.next()) else { return false };
        is_count(number) && WORDS.contains(&word.to_ascii_lowercase().as_str())
    })
}

/// Drops a leading "773,616 followers · 8,248 talking about this." from a page's bio.
fn without_counts(bio: &str) -> &str {
    let Some((first, rest)) = bio.split_once(". ") else {
        return if starts_with_counts(bio.trim_end_matches('.')) { "" } else { bio };
    };
    if starts_with_counts(first) { rest.trim() } else { bio }
}

/// Every `·`-separated piece is a number and a page statistic: "1.2K likes · 30 talking about this".
fn starts_with_counts(sentence: &str) -> bool {
    const STATS: [&str; 10] = [
        "follower",
        "followers",
        "following",
        "like",
        "likes",
        "talking about this",
        "were here",
        "people like this",
        "people follow this",
        "members",
    ];
    sentence.split('·').all(|piece| {
        let Some((number, stat)) = piece.trim().split_once(' ') else { return false };
        is_count(number) && STATS.contains(&stat.trim().to_lowercase().as_str())
    })
}

/// Titles Facebook gives login walls and empty pages; never worth a preview.
pub fn is_generic_title(title: &str) -> bool {
    let lower = title.trim().to_ascii_lowercase();
    lower.is_empty()
        || lower == "facebook"
        || lower.starts_with("log in")
        || lower.contains("content isn't available")
        || lower.contains("page isn't available")
}

/// `initialRouteInfo.route.meta.title` from the route data.
pub fn route_title<'a>(route: &Index<'a>) -> Option<&'a str> {
    route.nodes.iter().find_map(|node| text(node, "initialRouteInfo.route.meta.title"))
}

/// The text of a story the page confirms has no media. Requires the story's own node to
/// carry an explicit empty `attachments` list, so a login wall is never taken for a text post.
pub fn text_only<'a>(story: &[&'a Obj]) -> Option<&'a str> {
    let mut confirmed = false;
    let mut found = None;
    for node in story {
        confirmed |= node.get("attachments").and_then(|a| a.as_array()).is_some_and(Vec::is_empty);
        found = found.or_else(|| caption(&walk_obj(node).collect::<Vec<_>>()));
    }
    found.filter(|_| confirmed)
}

/// A group post that a reel or video page belongs to.
#[derive(Debug, PartialEq, Eq)]
pub struct GroupPost {
    pub url: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
}

pub fn group_post(index: &Index<'_>, video_id: &str) -> Option<GroupPost> {
    for node in &index.nodes {
        let Some(post_id) = node.get("post_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let video_matches = get(node, "video.id").is_some_and(|id| json_id(id).as_deref() == Some(video_id));
        let attachment_matches = each(node, "attachments", "media").any(|media| is_video(media, video_id));
        if !video_matches && !attachment_matches {
            continue;
        }
        let Some(group_id) = ["to.id", "feedback.associated_group.id"]
            .iter()
            .find_map(|p| get(node, p).and_then(json_id))
            .map(Cow::into_owned)
        else {
            continue;
        };
        let group_name = first_text(node, &GROUP_NAME);
        let caption = text(node, "message.text");
        let story =
            text(node, "seo_title").or_else(|| caption.and_then(|c| c.lines().map(str::trim).find(|l| !l.is_empty())));
        let title = [group_name, story].into_iter().flatten().collect::<Vec<_>>().join(" | ");
        return Some(GroupPost {
            url: format!("https://www.facebook.com/groups/{group_id}/permalink/{post_id}/"),
            name: group_name.map(str::to_string),
            title: (!title.is_empty()).then_some(title),
            caption: caption.map(str::to_string),
        });
    }
    None
}

fn json_id(value: &serde_json::Value) -> Option<Cow<'_, str>> {
    id_text(value).filter(|id| !id.is_empty())
}

/// A video attachment with this ID.
fn is_video(media: &Obj, video_id: &str) -> bool {
    text(media, "__typename") == Some("Video") && media.get("id").and_then(json_id).as_deref() == Some(video_id)
}

/// A count as Facebook writes it: `12`, `1.5K`, `3,204`.
fn is_count(number: &str) -> bool {
    let number = number.trim_end_matches(['K', 'M', 'B', 'k', 'm', 'b']);
    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b',')
}

/// The post time: the first `creation_time` on or inside the post's own nodes.
pub fn published_at(nodes: &[&Obj]) -> Option<i64> {
    nodes.iter().find_map(|node| walk_obj(node).find_map(|n| number(n, "creation_time")).filter(|t| *t > 1_000_000_000))
}

/// Who posted: the story's first actor, or the video's owner.
pub fn author<'a>(nodes: &[&'a Obj]) -> Option<&'a str> {
    nodes.iter().find_map(|node| {
        walk_obj(node).find_map(|n| {
            each(n, "actors", "")
                .find_map(|actor| text(actor, "name"))
                .or_else(|| first_text(n, &["owner.name", "video_owner.name"]))
        })
    })
}

/// The author's picture: from an object in the post's own nodes that names them (a story's
/// actor, a video's owner).
pub fn picture_of<'a>(nodes: &[&'a Obj], name: &str) -> Option<&'a str> {
    nodes.iter().find_map(|node| {
        walk_obj(node)
            .filter(|n| text(n, "name") == Some(name))
            .find_map(|n| first_text(n, &["profile_picture.uri", "profilePicLarge.uri", "profile_pic_uri"]))
    })
}

/// The author's profile page: from an object in the post's own nodes that names them.
pub fn profile_url_of<'a>(nodes: &[&'a Obj], name: &str) -> Option<&'a str> {
    nodes.iter().find_map(|node| {
        walk_obj(node)
            .filter(|n| text(n, "name") == Some(name))
            .find_map(|n| first_text(n, &["url", "profile_url"]))
            .filter(|url| url.starts_with("https://www.facebook.com/"))
    })
}

/// A profile or page: name, bio and picture from the meta tags, when the page says it's a profile.
pub struct Profile {
    pub name: Option<String>,
    pub text: Option<String>,
    pub bio: Option<String>,
    pub picture: String,
}

pub fn profile(page: &Page) -> Option<Profile> {
    let app_url = page.meta("al:ios:url").or_else(|| page.meta("al:android:url"))?;
    if !app_url.starts_with("fb://profile/") {
        return None;
    }
    let picture = page.meta("og:image")?.to_string();
    let name = page.meta("og:title");
    let bio = page
        .meta("og:description")
        .map(|d| name.and_then(|n| d.strip_prefix(&format!("{n}."))).map_or(d, str::trim))
        .map(without_counts);
    let text = match (name, bio) {
        (Some(name), Some(bio)) if !bio.is_empty() => Some(format!("{name}\n\n{bio}")),
        (Some(name), _) => Some(name.to_string()),
        (None, Some(bio)) if !bio.is_empty() => Some(bio.to_string()),
        _ => None,
    };
    let bio = bio.filter(|b| !b.is_empty()).map(str::to_string);
    Some(Profile { name: name.map(str::to_string), text, bio, picture })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::extract::facebook::page::parse;
    use crate::extract::facebook::scope::story_nodes;

    #[test]
    fn captions_in_priority_order() {
        let docs = [
            json!({ "video": { "creation_story": { "message": { "text": "later lookup" } } } }),
            json!({ "node_v2": { "comet_sections": { "content": { "story": { "message": { "text": " first lookup " } } } } } }),
        ];
        let index = Index::new(&docs);
        assert_eq!(caption(&index.nodes), Some("first lookup"));
    }

    #[test]
    fn text_only_needs_an_explicit_empty_list_on_the_post() {
        let post = |token: &str, attachments: Option<serde_json::Value>, text: Option<&str>| {
            let mut story = json!({ "post_id": token });
            if let Some(a) = attachments {
                story["attachments"] = a;
            }
            if let Some(t) = text {
                story["comet_sections"] = json!({ "content": { "story": { "message": { "text": t } } } });
            }
            json!({ "node_v2": story })
        };
        let tokens = vec!["3880".to_string()];
        let check = |doc: serde_json::Value| {
            let docs = [doc];
            let index = Index::new(&docs);
            text_only(&story_nodes(&index, &tokens)).map(str::to_string)
        };
        assert_eq!(check(post("3880", Some(json!([])), Some("Hiring"))).as_deref(), Some("Hiring"));
        assert_eq!(check(post("3880", None, Some("Hiring"))), None, "no attachments field");
        assert_eq!(check(post("3880", Some(json!([])), None)), None, "no text");
        assert_eq!(check(post("9999", Some(json!([])), Some("someone else's"))), None);
    }

    #[test]
    fn group_post_behind_a_video() {
        let doc = json!({ "post_id": "3880", "to": { "id": "77", "name": "Example Group" },
            "attachments": [{ "media": { "__typename": "Video", "id": 123 } }],
            "message": { "text": "\nFirst line\nsecond" } });
        let found = group_post(&Index::new(&[doc]), "123").unwrap();
        assert_eq!(found.url, "https://www.facebook.com/groups/77/permalink/3880/");
        assert_eq!(found.title.as_deref(), Some("Example Group | First line"));
        assert!(
            group_post(&Index::new(&[json!({ "post_id": "1", "video": { "id": "9" }, "to": { "id": "2" } })]), "123")
                .is_none()
        );
    }

    #[test]
    fn profiles_from_meta_tags() {
        let page = parse(
            br#"<meta property="al:ios:url" content="fb://profile/100">
            <meta property="og:title" content="Jane Doe">
            <meta property="og:description" content="Jane Doe. Writer and climber.">
            <meta property="og:image" content="https://scontent.example/jane.jpg">"#,
        )
        .unwrap();
        let p = profile(&page).unwrap();
        assert_eq!(p.text.as_deref(), Some("Jane Doe\n\nWriter and climber."));
        assert_eq!(without_counts("773,616 followers · 8,248 talking about this. are you real?"), "are you real?");
        assert_eq!(without_counts("1.2K likes · 30 talking about this."), "");
        assert_eq!(without_counts("2 cats and a dog. Hello."), "2 cats and a dog. Hello.");
        assert_eq!(p.picture, "https://scontent.example/jane.jpg");

        let page =
            parse(br#"<meta property="al:ios:url" content="fb://page/100"><meta property="og:image" content="x">"#)
                .unwrap();
        assert!(profile(&page).is_none());
    }

    #[test]
    fn titles_lose_engagement_counts() {
        assert_eq!(clean_title("44K views · 3.6K reactions | Tsuki Nova on Reels"), "Tsuki Nova on Reels");
        assert_eq!(clean_title("1,234 likes | Page"), "Page");
        assert_eq!(clean_title("Page - 3 views of the sea"), "Page - 3 views of the sea");
        assert_eq!(clean_title("Top 10 | Music"), "Top 10 | Music", "not counts");
        assert_eq!(clean_title("12 views"), "12 views", "nothing else left");
        assert!(is_generic_title("Log in or sign up to view"));
        assert!(is_generic_title("Facebook"));
        assert!(!is_generic_title("Facebook Gaming Week"));
    }

    #[test]
    fn video_story_and_story_owner() {
        // The story comes in two parts under one ID; only the second has the caption.
        let docs = [json!({ "data": {
            "story": { "id": "S1", "creation_time": 1_700_000_000, "message": null, "actors": [{ "name": "Owner" }],
                "attachments": [{ "media": { "__typename": "Video", "id": "42" } }] },
            "story_text": { "id": "S1", "message": { "text": "Caption" } },
            "other": { "creation_time": 1, "attachments": [{ "media": { "__typename": "Video", "id": "43" } }] },
            "bucket": { "id": "100", "story_bucket_owner": { "name": "Bucket Owner" } } } })];
        let index = Index::new(&docs);
        let story = video_story(&index, "42");
        assert_eq!(own_caption(&story), Some("Caption"));
        assert_eq!(author(&story), Some("Owner"));
        assert_eq!(published_at(&story), Some(1_700_000_000));
        assert!(video_story(&index, "44").is_empty());
        assert_eq!(story_owner(&index, "100"), Some("Bucket Owner"));
    }

    #[test]
    fn post_time_and_author() {
        let doc = json!({ "post_id": "1", "comet_sections": { "context_layout": { "story": {
            "actors": [{ "name": "Some Page" }], "creation_time": 1_700_000_000 } } } });
        let docs = [doc];
        let nodes = Index::new(&docs);
        assert_eq!(published_at(&nodes.nodes[..1]), Some(1_700_000_000));
        assert_eq!(author(&nodes.nodes[..1]), Some("Some Page"));
    }
}
