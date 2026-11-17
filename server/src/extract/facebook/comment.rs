//! Links to one comment (`?comment_id=`, and `&reply_comment_id=` for a reply to it). Facebook
//! opens the post on that comment, logged out too, so the preview is the comment, answering the
//! post. A comment that isn't on the page leaves the post's preview as it was.

use serde_json::Value;

use crate::extract::page::Page;
use crate::extract::{AuthorProfile, Media, Post, PostKind, Relation, Shared, Stats, Target, short_count};

/// The post as the comment the link names, when the page has it.
pub fn focus(page: &Page, target: &Target, post: Post) -> Post {
    let Some(id) = target.comment_id() else { return post };
    let Some(comment) = page.media().find_map(|doc| find(doc, &id)) else {
        tracing::debug!(url = %target.key(), "the linked comment isn't on the page; previewing the post");
        return post;
    };
    let text =
        |pointer: &str| comment.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let Some(author) = text("/author/name") else { return post };
    let answered = Shared {
        url: Some(post.url.clone()),
        title: None,
        author: post.author.clone().or_else(|| post.title.clone()),
        profile: post.profile.clone(),
        published_at: post.published_at,
        text: post.text.clone(),
        relation: Relation::Reply,
    };
    // The comment's own picture; without one, the post's, as what it's about.
    let media = match pictures(comment) {
        own if !own.is_empty() => own,
        _ => post.media,
    };
    let count = |pointer: &str| match comment.pointer(pointer) {
        Some(Value::String(n)) => short_count(n),
        Some(n) => n.as_u64(),
        None => None,
    };
    Post {
        title: None,
        author: Some(author.to_string()),
        profile: AuthorProfile {
            url: text("/author/url").map(str::to_string),
            avatar: text("/author/profile_picture_depth_0/uri").map(str::to_string),
            ..AuthorProfile::default()
        },
        published_at: comment.get("created_time").and_then(Value::as_i64),
        text: text("/body/text").or_else(|| text("/preferred_body/text")).map(str::to_string),
        kind: if media.is_empty() { PostKind::TextOnly } else { PostKind::Media },
        media,
        stats: Stats {
            reactions: count("/feedback/reactors/count_reduced").filter(|n| *n > 0),
            comments: count("/feedback/replies_fields/total_count").filter(|n| *n > 0),
            ..Stats::default()
        },
        shared: Some(answered),
        ..post
    }
}

/// The comment with ID `id` (its `legacy_fbid`) in a page's data.
fn find<'a>(value: &'a Value, id: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if map.get("__typename").and_then(Value::as_str) == Some("Comment")
                && map.get("legacy_fbid").and_then(Value::as_str) == Some(id)
                && map.contains_key("author")
            {
                return Some(value);
            }
            map.values().find_map(|v| find(v, id))
        }
        Value::Array(items) => items.iter().find_map(|v| find(v, id)),
        _ => None,
    }
}

/// The pictures attached to the comment itself (a photo, a GIF, a sticker), never its replies'.
fn pictures(comment: &Value) -> Vec<Media> {
    let Some(attachments) = comment.get("attachments").and_then(Value::as_array) else { return Vec::new() };
    attachments
        .iter()
        .filter_map(|wrapper| {
            let attachment = wrapper.pointer("/style_type_renderer/attachment").unwrap_or(wrapper);
            let media = attachment.get("media")?;
            ["image", "photo_image", "animated_image", "sticker_image"]
                .into_iter()
                .find_map(|key| media.pointer(&format!("/{key}/uri")).and_then(Value::as_str))
                .map(|uri| Media::photo(uri.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::extract::fake::FakeExtractor;
    use crate::extract::page::Script;

    fn page(comments: &Value) -> Page {
        let json = json!({"__bbox": {"result": {"data": {"node": {"feedback": {"comments": {"edges": comments}}}}}}});
        Page { scripts: vec![Script { json, media: true, route: false }], meta: Default::default() }
    }

    fn comment(id: &str, name: &str, text: &str) -> Value {
        json!({"node": {
            "__typename": "Comment", "legacy_fbid": id, "created_time": 1_757_130_908,
            "author": {"name": name, "url": format!("https://www.facebook.com/{name}"),
                       "profile_picture_depth_0": {"uri": "https://scontent.xx.fbcdn.net/a.jpg"}},
            "body": {"text": text},
            "feedback": {"reactors": {"count_reduced": "1.2K"}, "replies_fields": {"total_count": 3}}
        }})
    }

    #[test]
    fn a_comment_link_previews_the_comment_answering_the_post() {
        let target = Target::from_link("https://www.facebook.com/somepage/posts/123?comment_id=9&ref=x").unwrap();
        assert_eq!(target.key(), "https://www.facebook.com/somepage/posts/123?comment_id=9");
        let post = FakeExtractor::post(&target);
        let page = page(&json!([comment("8", "someone", "First"), comment("9", "other", "What about it?")]));
        let focused = focus(&page, &target, post.clone());
        assert_eq!((focused.author.as_deref(), focused.text.as_deref()), (Some("other"), Some("What about it?")));
        assert_eq!(focused.published_at, Some(1_757_130_908));
        assert_eq!((focused.stats.reactions, focused.stats.comments), (Some(1200), Some(3)));
        assert_eq!(focused.media, post.media, "the post's photos, as the comment has none");
        let answered = focused.shared.unwrap();
        assert_eq!((answered.relation, answered.text), (Relation::Reply, post.text.clone()));

        // A reply's own ID wins; a comment that isn't there leaves the post.
        let reply =
            Target::from_link("https://www.facebook.com/somepage/posts/123?comment_id=9&reply_comment_id=8").unwrap();
        assert_eq!(focus(&page, &reply, post.clone()).text.as_deref(), Some("First"));
        let gone = Target::from_link("https://www.facebook.com/somepage/posts/123?comment_id=7").unwrap();
        assert_eq!(focus(&page, &gone, post.clone()), post);
    }

    #[test]
    fn a_comments_own_picture() {
        let mut with_photo = comment("9", "other", "Look");
        with_photo["node"]["attachments"] = json!([{"style_type_renderer": {"attachment": {"media": {
            "__typename": "Photo", "image": {"uri": "https://scontent.xx.fbcdn.net/photo.jpg"}
        }}}}]);
        let target = Target::from_link("https://www.facebook.com/somepage/posts/123?comment_id=9").unwrap();
        let focused = focus(&page(&json!([with_photo])), &target, FakeExtractor::post(&target));
        assert_eq!(
            focused.media.iter().map(|m| m.url.as_str()).collect::<Vec<_>>(),
            ["https://scontent.xx.fbcdn.net/photo.jpg"]
        );
    }
}
