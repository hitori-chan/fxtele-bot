//! Shares: Facebook nests the shared post inside the sharer's story, with its own author, text,
//! date and media (`docs/facebook.md`).

use super::index::{Index, Obj, first_text, get_obj, id_of};
use super::media::{self, Candidate};
use super::text as texts;
use crate::extract::{AuthorProfile, Shared};

/// Where a story keeps the post it shares, fullest first: the content section's copy has the
/// author, text and media; the top-level one, only the author and link.
const ATTACHED: [&str; 4] = [
    "node_v2.comet_sections.content.story.attached_story",
    "comet_sections.content.story.attached_story",
    "node_v2.attached_story",
    "attached_story",
];

/// The shared post's node, from the sharer's own nodes: the first copy that names its author.
pub fn attached<'a>(story: &[&'a Obj]) -> Option<&'a Obj> {
    ATTACHED.iter().find_map(|path| {
        story.iter().find_map(|node| get_obj(node, path).filter(|shared| texts::author(&[*shared]).is_some()))
    })
}

/// The shared post's author, link, date and text. The date may sit only in another copy of the
/// same story (its header's), found by ID.
pub fn shared(index: &Index<'_>, node: &Obj) -> Shared {
    let copies: Vec<&Obj> = std::iter::once(node)
        .chain(id_of(node).into_iter().flat_map(|id| index.with_id(&id).collect::<Vec<_>>()))
        .collect();
    let author = texts::author(&[node]);
    Shared {
        url: copies
            .iter()
            .find_map(|copy| first_text(copy, &["wwwURL", "permalink_url", "url"]))
            .filter(|url| url.starts_with("https://www.facebook.com/"))
            .map(str::to_string),
        title: None,
        author: author.map(str::to_string),
        profile: AuthorProfile {
            url: author.and_then(|name| texts::profile_url_of(&[node], name)).map(str::to_string),
            avatar: author.and_then(|name| texts::picture_of(&[node], name)).map(str::to_string),
            ..AuthorProfile::default()
        },
        published_at: texts::published_at(&copies),
        text: first_text(node, &["message.text", "comet_sections.message.story.message.text"]).map(str::to_string),
        reply: false,
    }
}

/// The shared post's media, read like a story's own. Their captions are the shared post's text,
/// never the sharer's.
pub fn candidates(index: &Index<'_>, node: &Obj) -> Vec<Candidate> {
    let mut found = media::attached_candidates(index, node);
    for candidate in &mut found {
        candidate.caption = None;
    }
    found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::extract::facebook::scope::story_nodes;

    /// A share as a logged-in page has it: the sharer's story, with the shared post under its
    /// content section, and the shared post's date only in another copy of it.
    pub fn page() -> serde_json::Value {
        json!({ "data": { "node_v2": {
            "__typename": "Story", "post_id": "500", "creation_time": 1_794_739_999,
            "actors": [{ "name": "Sharer", "url": "https://www.facebook.com/sharer" }],
            "attachments": [],
            "attached_story": { "id": "S1", "permalink_url": "https://www.facebook.com/page/posts/pfbid0A",
                "comet_sections": { "context_layout": { "story": { "id": "S1", "comet_sections": {
                    "metadata": [{ "story": { "creation_time": 1_793_410_200, "url": "https://www.facebook.com/page/posts/pfbid0A" } }],
                    "title": { "story": { "actors": [{ "name": "Some Page" }] } } } } } } },
            "comet_sections": { "content": { "story": {
                "message": { "text": "🚩🚩🚩" },
                "attached_story": {
                    "id": "S1", "post_id": "600", "wwwURL": "https://www.facebook.com/page/posts/pfbid0A",
                    "actors": [{ "name": "Some Page", "url": "https://www.facebook.com/page",
                        "profile_picture": { "uri": "https://scontent.xx.fbcdn.net/v/page.jpg" } }],
                    "message": { "text": "What the page wrote" },
                    "attachments": [{ "styles": { "attachment": { "media": {
                        "__typename": "Photo", "id": "700",
                        "creation_story": { "message": { "text": "not the sharer's" } },
                        "photo_image": { "uri": "https://scontent.xx.fbcdn.net/v/photo.jpg", "width": 526, "height": 527 }
                    } } } }]
                }
            } } }
        } } })
    }

    #[test]
    fn reads_the_shared_post() {
        let docs = [page()];
        let index = Index::new(&docs);
        let story = story_nodes(&index, &["500".to_string()]);
        let node = attached(&story).unwrap();
        let shared = shared(&index, node);
        assert_eq!(shared.author.as_deref(), Some("Some Page"));
        assert_eq!(shared.url.as_deref(), Some("https://www.facebook.com/page/posts/pfbid0A"));
        assert_eq!(shared.profile.url.as_deref(), Some("https://www.facebook.com/page"));
        assert_eq!(shared.published_at, Some(1_793_410_200), "from the header's copy");
        assert_eq!(shared.text.as_deref(), Some("What the page wrote"));
        let media = candidates(&index, node);
        assert_eq!(media.len(), 1);
        assert_eq!((media[0].width, media[0].caption.as_deref()), (Some(526), None));
    }

    #[test]
    fn a_post_that_shares_nothing() {
        let docs = [json!({ "node_v2": { "post_id": "500", "actors": [{ "name": "Someone" }], "attachments": [] } })];
        let index = Index::new(&docs);
        assert!(attached(&story_nodes(&index, &["500".to_string()])).is_none());
    }
}
