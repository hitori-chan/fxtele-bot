//! Reading Threads' pages into a [`Post`] (`docs/threads.md`). A post's page holds the post in
//! its scripts, in Instagram's media shape, with the thread above it and, when it quotes one,
//! the quoted post; a profile's page holds the account.

use serde_json::Value;

use super::url::{Link, ORIGIN};
use crate::extract::instagram::parse::{api_media, best};
use crate::extract::{
    AuthorProfile, ExtractError, Media, PageType, Post, PostKind, Relation, Shared, Stage, Stats, Target,
};

/// What the scripts worth keeping hold: a post's `text_post_app_info`, an account's
/// `biography`.
pub const HINTS: &[&str] = &["text_post_app_info", "\"biography\""];

/// A post that is only text: its `image_versions2` is empty.
const TEXT_ONLY: u64 = 19;

fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn number(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

/// The first object under `roots` that `wanted` picks, depth first.
fn find<'a>(roots: &[&'a Value], wanted: &dyn Fn(&Value) -> bool) -> Option<&'a Value> {
    fn walk<'a>(value: &'a Value, wanted: &dyn Fn(&Value) -> bool) -> Option<&'a Value> {
        match value {
            Value::Object(map) if wanted(value) => Some(value),
            Value::Object(map) => map.values().find_map(|v| walk(v, wanted)),
            Value::Array(items) => items.iter().find_map(|v| walk(v, wanted)),
            _ => None,
        }
    }
    roots.iter().find_map(|root| walk(root, wanted))
}

/// A whole post, not a carousel item or a reference to one: it has an author and a caption
/// (`null` when it has no text).
fn is_post(value: &Value) -> bool {
    value.get("user").is_some_and(Value::is_object)
        && value.get("caption").is_some()
        && value.get("text_post_app_info").is_some_and(Value::is_object)
}

/// The post with code `code` among a page's scripts.
fn find_post<'a>(roots: &[&'a Value], code: &str) -> Option<&'a Value> {
    find(roots, &|v| v.get("code").and_then(Value::as_str) == Some(code) && is_post(v))
}

/// The post `item` answers, when the page has the thread above it.
fn find_parent<'a>(roots: &[&'a Value], item: &Value) -> Option<&'a Value> {
    let parent = text(&item["text_post_app_info"], "/reply_to_post_id")?;
    find(roots, &|v| {
        v.get("pk").and_then(Value::as_str) == Some(parent) && is_post(v) && v.get("code") != item.get("code")
    })
}

/// What a fixture keeps of a page: the post and the one it answers, or the account.
pub fn fixture_parts<'a>(roots: &[&'a Value], link: Link<'_>) -> Vec<&'a Value> {
    match link {
        Link::Post(code) => {
            let item = find_post(roots, code);
            [item, item.and_then(|i| find_parent(roots, i))].into_iter().flatten().collect()
        }
        Link::Profile(user) => account(roots, user).into_iter().collect(),
        Link::Share => Vec::new(),
    }
}

/// The post with code `code` on a post's page.
pub fn post(roots: &[&Value], target: &Target, code: &str) -> Result<Post, ExtractError> {
    let item = find_post(roots, code)
        .ok_or_else(|| ExtractError::new(Stage::Fetch, "Threads' page held no post; its format may have changed"))?;
    let author = Author::of(item)
        .ok_or_else(|| ExtractError::new(Stage::Fetch, "Threads' post had no author; its format may have changed"))?;
    let info = &item["text_post_app_info"];
    let mut media = media_of(item);
    let quoted = info.pointer("/share_info/quoted_post").filter(|q| is_post(q));
    let reply = replied_to(item, roots);
    // A post carries one other post: a reply that quotes shows the quote, and names whom it
    // answers in a first line, as Threads does above a reply on its own.
    let mut answers = None;
    let shared = match quoted {
        Some(quoted) => {
            if media.is_empty() {
                media = media_of(quoted);
            }
            answers = reply.and_then(|r| r.profile.handle.map(|h| format!("@{h}")).or(r.author));
            Author::of(quoted).map(|author| shared(quoted, &author, Relation::Quote))
        }
        None => reply,
    };
    let link = info.get("link_preview_attachment").filter(|l| l.is_object());
    if media.is_empty()
        && let Some(picture) = link.and_then(|l| text(l, "/image_url"))
    {
        media.push(Media::photo(picture.to_string()));
    }
    let hidden = item.get("like_and_view_counts_disabled").and_then(Value::as_bool) == Some(true);
    let reposts = [number(info, "/repost_count"), number(info, "/quote_count")];
    let stats = Stats {
        reactions: number(item, "/like_count").filter(|_| !hidden),
        comments: number(info, "/direct_reply_count"),
        // Threads counts reposts and quotes together.
        shares: reposts.iter().any(Option::is_some).then(|| reposts.iter().flatten().sum()),
        views: None,
    };
    let mut body = caption(item);
    if let Some(url) = link.and_then(|l| text(l, "/url")).map(unwrap_link)
        && !body.contains(&url)
    {
        body = [body.trim(), &url].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    }
    if let Some(whom) = answers {
        body = format!("Replying to {whom}\n\n{}", body.trim());
    }
    Ok(Post {
        key: target.key().to_string(),
        url: permalink(item, &author),
        page_type: PageType::Post,
        title: None,
        author: Some(author.name().to_string()),
        profile: author.profile(),
        // A post's community ("Tech Threads"), which Threads shows after the author.
        group: text(info, "/tag_header/tag_cluster_name")
            .or_else(|| text(info, "/tag_header/display_name"))
            .map(str::to_string),
        published_at: number(item, "/taken_at").and_then(|t| i64::try_from(t).ok()),
        text: Some(body).filter(|b| !b.trim().is_empty()),
        kind: if media.is_empty() { PostKind::TextOnly } else { PostKind::Media },
        media,
        stats,
        shared,
    })
}

/// The post a reply answers: in full from the thread the page holds, or just its author's name.
fn replied_to(item: &Value, roots: &[&Value]) -> Option<Shared> {
    let info = &item["text_post_app_info"];
    text(info, "/reply_to_post_id")?;
    if let Some(parent) = find_parent(roots, item)
        && let Some(author) = Author::of(parent)
    {
        return Some(shared(parent, &author, Relation::Reply));
    }
    let username = text(info, "/reply_to_author/username")?;
    Some(Shared {
        author: Some(username.to_string()),
        profile: AuthorProfile {
            url: Some(format!("{ORIGIN}/@{username}")),
            handle: Some(username.to_string()),
            ..AuthorProfile::default()
        },
        relation: Relation::Reply,
        ..Shared::default()
    })
}

fn shared(item: &Value, author: &Author<'_>, relation: Relation) -> Shared {
    Shared {
        url: Some(permalink(item, author)),
        title: None,
        author: Some(author.name().to_string()),
        profile: author.profile(),
        published_at: number(item, "/taken_at").and_then(|t| i64::try_from(t).ok()),
        text: Some(caption(item)).filter(|c| !c.trim().is_empty()),
        relation,
    }
}

fn permalink(item: &Value, author: &Author<'_>) -> String {
    text(item, "/permalink").filter(|p| p.starts_with(ORIGIN)).map_or_else(
        || format!("{ORIGIN}/@{}/post/{}", author.username, text(item, "/code").unwrap_or_default()),
        str::to_string,
    )
}

/// A post's text: its fragments, whose links are whole where the caption may shorten them,
/// then a long post's attached text and a poll's choices.
fn caption(item: &Value) -> String {
    let info = &item["text_post_app_info"];
    let from_fragments = |fragments: Option<&Value>| -> Option<String> {
        let fragments = fragments?.as_array().filter(|f| !f.is_empty())?;
        let text: String = fragments
            .iter()
            .map(|f| match f.get("fragment_type").and_then(Value::as_str) {
                Some("link") => text(f, "/link_fragment/uri")
                    .or_else(|| text(f, "/link_fragment/url"))
                    .map(unwrap_link)
                    .unwrap_or_else(|| f["plaintext"].as_str().unwrap_or_default().to_string()),
                // A sticker or emoji drawn inline; its text is a placeholder box.
                Some("inline_sticker" | "inline_emoji") => String::new(),
                _ => f["plaintext"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        Some(text)
    };
    let mut body = from_fragments(info.pointer("/text_fragments/fragments"))
        .or_else(|| text(item, "/caption/text").map(str::to_string))
        .unwrap_or_default();
    if let Some(attached) = from_fragments(info.pointer("/snippet_attachment_info/text_fragments/fragments"))
        .filter(|a| !a.trim().is_empty())
    {
        body = format!("{}\n\n{}", body.trim_end(), attached.trim());
    }
    if let Some(poll) = poll(item) {
        body = format!("{}\n\n{poll}", body.trim_end());
    }
    body.trim().to_string()
}

/// A poll's choices, with their votes when Threads shows them (it doesn't logged out).
fn poll(item: &Value) -> Option<String> {
    let poll = item.pointer("/caption_add_on/poll").filter(|p| p.is_object())?;
    let tallies = poll.get("tallies").and_then(Value::as_array)?;
    let lines: Vec<String> = tallies
        .iter()
        .filter_map(|t| {
            let choice = text(t, "/text")?;
            Some(match number(t, "/count") {
                Some(votes) => format!("▫️ {choice} · {votes}"),
                None => format!("▫️ {choice}"),
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let heading =
        if poll.get("finished").and_then(Value::as_bool) == Some(true) { "📊 Poll (closed)" } else { "📊 Poll" };
    Some(format!("{heading}\n{}", lines.join("\n")))
}

/// The address behind Threads' outgoing-link redirect (`l.threads.com/?u=…`).
fn unwrap_link(link: &str) -> String {
    let Ok(url) = url::Url::parse(link) else { return link.to_string() };
    let wrapped = matches!(url.host_str(), Some("l.threads.com" | "l.threads.net" | "l.instagram.com"));
    let target = url.query_pairs().find(|(k, _)| k == "u").map(|(_, v)| v.into_owned());
    match target {
        Some(target) if wrapped && (target.starts_with("https://") || target.starts_with("http://")) => target,
        _ => link.to_string(),
    }
}

/// A post's photos and videos, in order. Instagram's reader takes each, as the shape is
/// Instagram's; an item it can't read is left out.
fn media_of(item: &Value) -> Vec<Media> {
    if number(item, "/media_type") == Some(TEXT_ONLY) {
        return Vec::new();
    }
    match item.get("carousel_media").and_then(Value::as_array) {
        Some(children) if !children.is_empty() => children.iter().filter_map(api_media).collect(),
        _ => api_media(item).into_iter().collect(),
    }
}

/// An account's page.
pub fn profile(roots: &[&Value], target: &Target, username: &str) -> Result<Post, ExtractError> {
    let user =
        account(roots, username).ok_or_else(|| ExtractError::new(Stage::NotFound, "Threads has no such account"))?;
    let author = Author::of_user(user, username);
    let bio = text(user, "/biography").map(str::to_string);
    let picture = author.full_size().or(author.picture).map(str::to_string);
    let url = format!("{ORIGIN}/@{}", author.username);
    Ok(Post {
        key: target.key().to_string(),
        url: url.clone(),
        page_type: PageType::Profile,
        title: Some(author.name().to_string()),
        author: Some(author.name().to_string()),
        profile: AuthorProfile {
            url: Some(url),
            handle: Some(author.username.to_string()),
            avatar: author.picture.map(str::to_string),
            picture: picture.clone(),
            bio: bio.clone(),
        },
        group: None,
        published_at: None,
        text: bio,
        media: picture.into_iter().map(Media::photo).collect(),
        kind: PostKind::Profile,
        stats: Stats::default(),
        shared: None,
    })
}

/// An account's full-size picture and bio, from their page.
pub fn about(roots: &[&Value], username: &str) -> Option<(Option<String>, Option<String>)> {
    let user = account(roots, username)?;
    let author = Author::of_user(user, username);
    Some((author.full_size().map(str::to_string), text(user, "/biography").map(str::to_string)))
}

/// The account `username` on its page: the one with a bio, unlike the posts' authors.
fn account<'a>(roots: &[&'a Value], username: &str) -> Option<&'a Value> {
    find(roots, &|v| {
        v.get("username").and_then(Value::as_str).is_some_and(|u| u.eq_ignore_ascii_case(username))
            && v.get("biography").is_some()
    })
}

struct Author<'a> {
    username: &'a str,
    full_name: Option<&'a str>,
    /// 150 px: fine for a card.
    picture: Option<&'a str>,
    user: &'a Value,
}

impl<'a> Author<'a> {
    fn of(item: &'a Value) -> Option<Self> {
        let user = item.get("user")?;
        Some(Self::of_user(user, text(user, "/username")?))
    }

    fn of_user(user: &'a Value, username: &'a str) -> Self {
        Self {
            username: text(user, "/username").unwrap_or(username),
            full_name: text(user, "/full_name"),
            picture: text(user, "/profile_pic_url"),
            user,
        }
    }

    fn name(&self) -> &'a str {
        self.full_name.unwrap_or(self.username)
    }

    /// The biggest picture an account's page has.
    fn full_size(&self) -> Option<&'a str> {
        let versions = self.user.get("hd_profile_pic_versions")?;
        let (url, ..) = best(versions)?;
        versions.as_array()?.iter().find_map(|v| text(v, "/url").filter(|u| *u == url))
    }

    fn profile(&self) -> AuthorProfile {
        AuthorProfile {
            url: Some(format!("{ORIGIN}/@{}", self.username)),
            handle: Some(self.username.to_string()),
            avatar: self.picture.map(str::to_string),
            picture: None,
            bio: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn target(link: &str) -> Target {
        Target::from_link(link).unwrap()
    }

    fn photo(name: &str) -> Value {
        json!({"media_type": 1, "original_width": 1080, "original_height": 1350, "image_versions2": {"candidates": [
            {"url": format!("https://scontent.cdninstagram.com/{name}-small.jpg"), "width": 640, "height": 800},
            {"url": format!("https://scontent.cdninstagram.com/{name}.jpg"), "width": 1080, "height": 1350}
        ]}})
    }

    fn item(code: &str, pk: &str, user: &str, text: &str) -> Value {
        json!({
            "code": code, "pk": pk, "taken_at": 1_793_988_374, "media_type": 19, "like_count": 2410,
            "user": {"username": user, "full_name": format!("{user} name"), "profile_pic_url": "https://scontent.cdninstagram.com/p.jpg"},
            "caption": {"text": text},
            "image_versions2": {"candidates": []},
            "text_post_app_info": {"direct_reply_count": 556, "repost_count": 104, "quote_count": 32, "share_info": {}}
        })
    }

    #[test]
    fn a_carousel_with_its_community() {
        let mut post = item("DfaNgswkQPG", "1", "zuck", "Using our new spaces");
        post["media_type"] = json!(8);
        post["carousel_media"] = json!([photo("a"), photo("b")]);
        post["text_post_app_info"]["tag_header"] =
            json!({"display_name": "techthreads", "tag_cluster_name": "Tech Threads"});
        // The same post's code on a related post must not be taken for it.
        let related = json!({"relatedPosts": [{"thread_items": [{"post": item("Other", "2", "someone", "x")}]}]});
        let page = json!({"data": {"media": post}});
        let target = target("https://www.threads.net/@zuck/post/DfaNgswkQPG?xmt=1");
        let post = super::post(&[&related, &page], &target, "DfaNgswkQPG").unwrap();
        assert_eq!(post.url, "https://www.threads.com/@zuck/post/DfaNgswkQPG");
        assert_eq!((post.author.as_deref(), post.profile.handle.as_deref()), (Some("zuck name"), Some("zuck")));
        assert_eq!(post.group.as_deref(), Some("Tech Threads"));
        assert_eq!(
            post.media.iter().map(|m| m.url.as_str()).collect::<Vec<_>>(),
            ["https://scontent.cdninstagram.com/a.jpg", "https://scontent.cdninstagram.com/b.jpg"]
        );
        assert_eq!(
            (post.stats.reactions, post.stats.comments, post.stats.shares),
            (Some(2410), Some(556), Some(136)),
            "reposts and quotes together"
        );
        assert_eq!(post.published_at, Some(1_793_988_374));
        assert!(super::post(&[&related], &target, "DfaNgswkQPG").is_err());
    }

    #[test]
    fn text_from_fragments_with_whole_links_a_long_text_and_a_poll() {
        let mut post = item("C", "1", "someone", "Look at exa…");
        post["text_post_app_info"]["text_fragments"] = json!({"fragments": [
            {"fragment_type": "plaintext", "plaintext": "Look at "},
            {"fragment_type": "link", "plaintext": "example.com/a…", "link_fragment": {"uri": "https://l.threads.com/?u=https%3A%2F%2Fexample.com%2Fa%2Flong&e=x"}},
            {"fragment_type": "inline_sticker", "plaintext": "□"},
            {"fragment_type": "plaintext", "plaintext": " with "},
            {"fragment_type": "mention", "plaintext": "@friend", "mention_fragment": {"mentioned_user": {"username": "friend"}}}
        ]});
        post["text_post_app_info"]["snippet_attachment_info"] =
            json!({"text_fragments": {"fragments": [{"fragment_type": "plaintext", "plaintext": "The long part."}]}});
        post["caption_add_on"] =
            json!({"poll": {"finished": false, "tallies": [{"text": "Yes", "count": null}, {"text": "No"}]}});
        let post = super::post(&[&post], &target("https://www.threads.com/t/C1234"), "C").unwrap();
        assert_eq!(
            post.text.as_deref(),
            Some("Look at https://example.com/a/long with @friend\n\nThe long part.\n\n📊 Poll\n▫️ Yes\n▫️ No")
        );
        assert_eq!(post.kind, PostKind::TextOnly);
    }

    #[test]
    fn a_quote_and_a_reply() {
        let mut quoted = item("Q", "9", "colinandsamir", "Instagram launches Features");
        quoted["media_type"] = json!(1);
        quoted["image_versions2"] = photo("q")["image_versions2"].clone();
        let mut quoting = item("A", "1", "mosseri", "Really enjoyed it");
        quoting["text_post_app_info"]["share_info"]["quoted_post"] = quoted;
        let post = post(&[&quoting], &target("https://www.threads.com/@mosseri/post/AAAAA"), "A").unwrap();
        let shared = post.shared.unwrap();
        assert_eq!((shared.relation, shared.author.as_deref()), (Relation::Quote, Some("colinandsamir name")));
        assert_eq!(shared.url.as_deref(), Some("https://www.threads.com/@colinandsamir/post/Q"));
        assert_eq!(post.media.len(), 1, "the quoted post's picture, as the quote has none");

        let mut reply = item("R", "2", "zuck", "Me after today");
        reply["text_post_app_info"]["reply_to_post_id"] = json!("9");
        reply["text_post_app_info"]["reply_to_author"] = json!({"username": "zuck"});
        let parent = item("P", "9", "zuck", "Big day");
        let thread = json!({"containing_thread": {"posts": {"edges": [{"node": parent}, {"node": reply.clone()}]}}});
        let target = target("https://www.threads.com/@zuck/post/RRRRR");
        let shared = super::post(&[&thread], &target, "R").unwrap().shared.unwrap();
        assert_eq!((shared.relation, shared.text.as_deref()), (Relation::Reply, Some("Big day")));
        let shared = super::post(&[&reply], &target, "R").unwrap().shared.unwrap();
        assert_eq!((shared.author.as_deref(), shared.text), (Some("zuck"), None), "the parent isn't on the page");

        let mut both = reply;
        both["text_post_app_info"]["share_info"]["quoted_post"] = item("Q", "8", "colinandsamir", "Features");
        let post = super::post(&[&both], &target, "R").unwrap();
        assert_eq!(post.shared.unwrap().relation, Relation::Quote);
        assert_eq!(post.text.as_deref(), Some("Replying to @zuck\n\nMe after today"), "and whom it answers");
    }

    #[test]
    fn a_link_post_and_hidden_likes() {
        let mut post = item("L", "1", "threads", "Join the conversation here:");
        post["like_and_view_counts_disabled"] = json!(true);
        post["text_post_app_info"]["link_preview_attachment"] = json!({
            "url": "https://l.threads.com/?u=https%3A%2F%2Fexample.com%2Flive&e=AT0",
            "image_url": "https://external.xx.fbcdn.net/emg1/v/t13/x.jpg"
        });
        let post = super::post(&[&post], &target("https://www.threads.com/t/LLLLL"), "L").unwrap();
        assert_eq!(post.text.as_deref(), Some("Join the conversation here:\n\nhttps://example.com/live"));
        assert_eq!(post.media[0].url, "https://external.xx.fbcdn.net/emg1/v/t13/x.jpg");
        assert_eq!(post.stats.reactions, None);
    }

    #[test]
    fn an_account() {
        let page = json!({"data": {"user": {
            "username": "zuck", "full_name": "Mark", "biography": "Mostly MMA takes",
            "profile_pic_url": "https://scontent.cdninstagram.com/s150.jpg",
            "hd_profile_pic_versions": [
                {"url": "https://scontent.cdninstagram.com/s320.jpg", "width": 320, "height": 320},
                {"url": "https://scontent.cdninstagram.com/s640.jpg", "width": 640, "height": 640}
            ]
        }}});
        let post = profile(&[&page], &target("https://www.threads.com/@Zuck"), "zuck").unwrap();
        assert_eq!(
            (post.kind, post.title.as_deref(), post.text.as_deref()),
            (PostKind::Profile, Some("Mark"), Some("Mostly MMA takes"))
        );
        assert_eq!(post.media[0].url, "https://scontent.cdninstagram.com/s640.jpg");
        assert_eq!(
            about(&[&page], "zuck"),
            Some((Some("https://scontent.cdninstagram.com/s640.jpg".into()), Some("Mostly MMA takes".into())))
        );
        assert_eq!(
            profile(&[&page], &target("https://www.threads.com/@other"), "other").unwrap_err().stage,
            Stage::NotFound
        );
    }
}
