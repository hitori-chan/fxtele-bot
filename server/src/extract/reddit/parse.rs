//! Reading Reddit's answers (`docs/reddit.md`): a post's JSON, with the comment a link names and
//! the post a crosspost shares; a user's or community's `about.json`; and the tags Reddit gives
//! link-preview crawlers, which make a card without cookies.

use serde_json::Value;

use super::url::ORIGIN;
use crate::extract::page::Page;
use crate::extract::{
    AuthorProfile, ExtractError, Media, PageType, Post, PostKind, Shared, Stage, Stats, Target, short_count,
};

/// A Reddit video's own streams, whose sound its single file lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    /// Its ID on `v.redd.it`.
    pub id: String,
    /// Its DASH playlist.
    pub playlist: Option<String>,
    pub has_audio: bool,
    pub duration_seconds: Option<u64>,
    /// The page of the post it's in, whose embed page has files with the sound.
    pub permalink: Option<String>,
}

fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// A number Reddit may write as `12` or `12.0` (times are floats).
fn number(value: &Value, pointer: &str) -> Option<i64> {
    let n = value.pointer(pointer)?;
    n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}

fn count(value: &Value, pointer: &str) -> Option<u64> {
    number(value, pointer).and_then(|n| u64::try_from(n).ok())
}

fn size(value: &Value, pointer: &str) -> Option<u32> {
    number(value, pointer).and_then(|n| u32::try_from(n).ok())
}

/// `u/someone`, unless the account is gone.
fn user(name: Option<&str>) -> Option<String> {
    name.filter(|n| *n != "[deleted]").map(|n| format!("u/{n}"))
}

/// The post's data in a `/comments/{id}.json` answer: `[0].data.children[0].data`.
fn post_data(response: &Value) -> Option<&Value> {
    let child = response.pointer("/0/data/children/0")?;
    (text(child, "/kind") == Some("t3")).then(|| child.get("data")).flatten()
}

/// An error answer (`{"reason": "quarantined", "error": 403}`) in plain words.
pub fn refusal(response: &Value) -> Option<ExtractError> {
    let reason = text(response, "/reason")?;
    Some(match reason {
        "quarantined" => ExtractError::new(
            Stage::LoginWall,
            "the community is quarantined: Reddit shows it only to accounts that opted in",
        ),
        "private" => ExtractError::new(Stage::LoginWall, "the community is private"),
        "banned" => ExtractError::new(Stage::NotFound, "the community is banned"),
        other => ExtractError::new(Stage::NotFound, format!("Reddit refused: {other}")),
    })
}

/// A post, or the comment the link names under it, with its media and the post it crossposts.
pub fn post(response: &Value, target: &Target, comment: Option<&str>) -> Result<(Post, Option<Video>), ExtractError> {
    let data = post_data(response).ok_or_else(|| ExtractError::new(Stage::NotFound, "Reddit's answer held no post"))?;
    let parent = data.pointer("/crosspost_parent_list/0").filter(|p| p.is_object());
    // A crosspost has no media of its own: they're the post it shares.
    let (mut media, mut video) = media_of(data);
    if media.is_empty()
        && let Some(parent) = parent
    {
        (media, video) = media_of(parent);
    }
    let title = headline(data);
    let mut post = Post {
        key: target.key().to_string(),
        url: permalink(data).unwrap_or_else(|| target.url.to_string()),
        page_type: PageType::Post,
        title: title.clone(),
        author: user(text(data, "/author")),
        profile: profile(text(data, "/author")),
        group: text(data, "/subreddit_name_prefixed").map(str::to_string),
        published_at: number(data, "/created_utc"),
        text: body(data),
        kind: if media.is_empty() { PostKind::TextOnly } else { PostKind::Media },
        media,
        stats: Stats {
            reactions: count(data, "/score"),
            comments: count(data, "/num_comments"),
            shares: count(data, "/num_crossposts"),
            views: None,
        },
        shared: parent.map(|parent| {
            let mut shared = shared(parent);
            shared.title = shared.title.filter(|t| Some(t) != title.as_ref());
            shared
        }),
    };
    if let Some(id) = comment {
        let found = response
            .pointer("/1/data/children/0")
            .filter(|c| text(c, "/kind") == Some("t1") && text(c, "/data/id") == Some(id))
            .and_then(|c| c.get("data"))
            .ok_or_else(|| ExtractError::new(Stage::NotFound, "no such comment: it's gone, or the link is wrong"))?;
        // The comment first, the post below it as what it answers.
        let mut answered = shared(data);
        (answered.title, answered.reply) = (None, true);
        post.shared = Some(answered);
        post.title = title.map(|t| format!("Re: {t}"));
        post.author = user(text(found, "/author"));
        post.profile = profile(text(found, "/author"));
        post.published_at = number(found, "/created_utc");
        post.text = text(found, "/body").map(str::to_string);
        post.url = text(found, "/permalink").map_or(post.url, |p| format!("{ORIGIN}{p}"));
        let hidden = found.get("score_hidden").and_then(Value::as_bool) == Some(true);
        post.stats = Stats { reactions: count(found, "/score").filter(|_| !hidden), ..Stats::default() };
    }
    Ok((post, video))
}

/// The headline, marked when Reddit marks the post.
fn headline(data: &Value) -> Option<String> {
    let title = text(data, "/title")?;
    let flag = |name| data.get(name).and_then(Value::as_bool) == Some(true);
    Some(match (flag("over_18"), flag("spoiler")) {
        (true, _) => format!("NSFW · {title}"),
        (false, true) => format!("Spoiler · {title}"),
        _ => title.to_string(),
    })
}

fn permalink(data: &Value) -> Option<String> {
    text(data, "/permalink").map(|p| format!("{ORIGIN}{p}"))
}

fn profile(author: Option<&str>) -> AuthorProfile {
    AuthorProfile {
        url: author.filter(|a| *a != "[deleted]").map(|a| format!("{ORIGIN}/user/{a}/")),
        ..AuthorProfile::default()
    }
}

/// The post's text (Markdown), and where a link post leads.
fn body(data: &Value) -> Option<String> {
    let selftext = text(data, "/selftext");
    let link = text(data, "/url_overridden_by_dest")
        .filter(|_| data.get("is_self").and_then(Value::as_bool) != Some(true))
        .filter(|url| !is_reddits_own(url));
    match (selftext, link) {
        (Some(text), Some(link)) => Some(format!("{text}\n\n{link}")),
        (Some(text), None) => Some(text.to_string()),
        (None, Some(link)) => Some(link.to_string()),
        (None, None) => None,
    }
}

/// Reddit's own pages and media, which the post shows itself.
fn is_reddits_own(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).is_some_and(|host| {
        let on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        on("reddit.com") || on("redd.it")
    })
}

/// A post another one shares or answers: by whom, where, and what it says.
fn shared(data: &Value) -> Shared {
    let by = match (user(text(data, "/author")), text(data, "/subreddit_name_prefixed")) {
        (Some(author), Some(group)) => Some(format!("{author} in {group}")),
        (author, group) => author.or_else(|| group.map(str::to_string)),
    };
    Shared {
        url: permalink(data),
        title: headline(data),
        author: by,
        profile: profile(text(data, "/author")),
        published_at: number(data, "/created_utc"),
        text: body(data),
        reply: false,
    }
}

/// A post's media, in order, and its video's streams: a hosted video, a gallery, a GIF, or the
/// picture Reddit made for it (a photo's, or a link's preview).
fn media_of(data: &Value) -> (Vec<Media>, Option<Video>) {
    // A text post's preview is of a page its text links to, not media of its own.
    let is_self = data.get("is_self").and_then(Value::as_bool) == Some(true);
    let preview = data.get("preview").filter(|_| !is_self);
    let poster = preview.and_then(|p| text(p, "/images/0/source/url")).map(str::to_string);
    let hosted = data.pointer("/secure_media/reddit_video").or_else(|| data.pointer("/media/reddit_video"));
    if let Some(v) = hosted.or_else(|| preview.and_then(|p| p.pointer("/reddit_video_preview")))
        && let Some(file) = text(v, "/fallback_url")
    {
        let mut media = Media::video(file.to_string(), poster);
        (media.width, media.height) = (size(v, "/width"), size(v, "/height"));
        let video = hosted.and_then(|_| video_id(file)).map(|id| Video {
            id,
            playlist: text(v, "/dash_url").map(str::to_string),
            has_audio: v.get("has_audio").and_then(Value::as_bool) == Some(true)
                && v.get("is_gif").and_then(Value::as_bool) != Some(true),
            duration_seconds: count(v, "/duration"),
            permalink: permalink(data),
        });
        return (vec![media], video);
    }
    if let Some(items) = data.pointer("/gallery_data/items").and_then(Value::as_array) {
        let media = items
            .iter()
            .filter(|item| item.get("is_deleted").and_then(Value::as_bool) != Some(true))
            .filter_map(|item| data.pointer(&format!("/media_metadata/{}", text(item, "/media_id")?)))
            .filter(|meta| text(meta, "/status") == Some("valid"))
            .filter_map(gallery_item)
            .collect();
        return (media, None);
    }
    let image = preview.and_then(|p| p.pointer("/images/0"));
    if let Some(gif) = image.and_then(|i| text(i, "/variants/mp4/source/url")) {
        let mut media = Media::video(gif.to_string(), poster);
        let source = image.and_then(|i| i.pointer("/variants/mp4/source"));
        (media.width, media.height) = source.map_or((None, None), |s| (size(s, "/width"), size(s, "/height")));
        return (vec![media], None);
    }
    let photo = image.and_then(|i| {
        let mut media = Media::photo(text(i, "/source/url")?.to_string());
        (media.width, media.height) = (size(i, "/source/width"), size(i, "/source/height"));
        Some(media)
    });
    (photo.into_iter().collect(), None)
}

/// One gallery item: a picture (`s.u`, full size), or an animated one's MP4.
fn gallery_item(meta: &Value) -> Option<Media> {
    let (width, height) = (size(meta, "/s/x"), size(meta, "/s/y"));
    let mut media = match (text(meta, "/e"), text(meta, "/s/mp4")) {
        (Some("AnimatedImage"), Some(mp4)) => Media::video(mp4.to_string(), None),
        _ => Media::photo(text(meta, "/s/u").or_else(|| text(meta, "/s/gif"))?.to_string()),
    };
    (media.width, media.height) = (width, height);
    Some(media)
}

/// The ID in a `https://v.redd.it/{id}/...` file's address.
fn video_id(file: &str) -> Option<String> {
    let rest = file.strip_prefix("https://v.redd.it/")?;
    let id = rest.split(['/', '?']).next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_string())
}

/// A community from `/r/{name}/about.json`.
pub fn community(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    if let Some(e) = refusal(response) {
        return Err(e);
    }
    let data = response
        .get("data")
        .filter(|_| text(response, "/kind") == Some("t5"))
        .ok_or_else(|| ExtractError::new(Stage::NotFound, "Reddit has no such community"))?;
    let name = text(data, "/display_name_prefixed").map(str::to_string);
    let picture = text(data, "/community_icon").or_else(|| text(data, "/icon_img")).map(str::to_string);
    let mut description = text(data, "/public_description").map(str::to_string);
    if let Some(members) = count(data, "/subscribers") {
        let line = format!("{} members", crate::render::compact(members));
        description = Some(description.map_or(line.clone(), |d| format!("{d}\n\n{line}")));
    }
    let title = match (name.as_deref(), text(data, "/title")) {
        (Some(name), Some(title)) if !title.eq_ignore_ascii_case(name.trim_start_matches("r/")) => {
            Some(format!("{name} · {title}"))
        }
        _ => name.clone(),
    };
    Ok(profile_post(target, title, name, description, picture))
}

/// A user from `/user/{name}/about.json`.
pub fn person(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    let data = response
        .get("data")
        .filter(|_| text(response, "/kind") == Some("t2"))
        .ok_or_else(|| ExtractError::new(Stage::NotFound, "Reddit has no such user, or they're suspended"))?;
    let name = user(text(data, "/name"));
    let picture = text(data, "/snoovatar_img").or_else(|| text(data, "/icon_img")).map(str::to_string);
    let mut description = text(data, "/subreddit/public_description").map(str::to_string);
    if let Some(karma) = count(data, "/total_karma") {
        let line = format!("{} karma", crate::render::compact(karma));
        description = Some(description.map_or(line.clone(), |d| format!("{d}\n\n{line}")));
    }
    Ok(profile_post(target, name.clone(), name, description, picture))
}

/// An author's picture and bio, from their `about.json`.
pub fn author(response: &Value) -> Option<(Option<String>, Option<String>)> {
    let data = response.get("data").filter(|_| text(response, "/kind") == Some("t2"))?;
    let picture = text(data, "/snoovatar_img").or_else(|| text(data, "/icon_img")).map(str::to_string);
    Some((picture, text(data, "/subreddit/public_description").map(str::to_string)))
}

fn profile_post(
    target: &Target,
    title: Option<String>,
    name: Option<String>,
    text: Option<String>,
    picture: Option<String>,
) -> Post {
    Post {
        key: target.key().to_string(),
        url: target.url.to_string(),
        page_type: PageType::Profile,
        title,
        author: name,
        profile: AuthorProfile {
            url: Some(target.url.to_string()),
            picture: picture.clone(),
            ..AuthorProfile::default()
        },
        group: None,
        published_at: None,
        text,
        kind: PostKind::Profile,
        media: picture.map(Media::photo).into_iter().collect(),
        stats: Stats::default(),
        shared: None,
    }
}

/// The card Reddit's tags for link-preview crawlers make: the headline, the start of the text
/// with the counts before it ("216K votes, 23K comments. ..."), and a picture of the post; for a
/// user or community, its name and description.
pub fn crawler_post(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    let missing = || ExtractError::new(Stage::NotFound, "Reddit's page for crawlers had no post");
    let og_title = page.meta("og:title").ok_or_else(missing)?;
    // Reddit's logo, the picture of pages without one of their own, says nothing.
    let picture = page.meta("og:image").filter(|url| crate::extract::trusted_media_url(url));
    if target.page_type == PageType::Profile {
        let text = page.meta("og:description").map(str::to_string);
        let name = Some(og_title.to_string());
        return Ok(profile_post(target, name.clone(), name, text, picture.map(str::to_string)));
    }
    let community = og_title.strip_prefix("From the ").and_then(|t| t.split_once(" community on Reddit")).map(|t| t.0);
    // The headline is in the title, or for text posts only in the picture's description.
    let title = og_title
        .split_once("community on Reddit: ")
        .map(|(_, t)| t)
        .or_else(|| page.meta("og:image:alt").and_then(|alt| alt.split_once("community on Reddit: ")).map(|(_, t)| t))
        .map(|t| t.trim_matches('"').to_string());
    let (stats, text) = page.meta("description").map_or((Stats::default(), None), counted_text);
    if title.is_none() && text.is_none() {
        return Err(missing());
    }
    let picture = picture.map(|url| Media::photo(url.to_string()));
    Ok(Post {
        key: target.key().to_string(),
        url: page
            .meta("og:url")
            .filter(|u| u.starts_with(ORIGIN))
            .map_or_else(|| target.url.to_string(), str::to_string),
        page_type: target.page_type,
        title,
        author: None,
        profile: AuthorProfile::default(),
        group: community.map(|c| format!("r/{c}")),
        published_at: None,
        text,
        kind: if picture.is_some() { PostKind::Media } else { PostKind::TextOnly },
        media: picture.into_iter().collect(),
        stats,
        shared: None,
    })
}

/// "216K votes, 23K comments. The text…": the counts, and the text after them. A community's
/// description ("17M subscribers in the … community.") isn't the post's.
fn counted_text(description: &str) -> (Stats, Option<String>) {
    let Some((counts, rest)) = description.split_once(". ") else { return (Stats::default(), None) };
    let mut stats = Stats::default();
    for part in counts.split(", ") {
        match part.split_once(' ') {
            Some((n, "votes" | "vote")) => stats.reactions = short_count(n),
            Some((n, "comments" | "comment")) => stats.comments = short_count(n),
            _ => return (Stats::default(), Some(description.to_string())),
        }
    }
    let text = Some(rest.trim()).filter(|t| !t.is_empty() && !t.contains(" subscribers in the "));
    (stats, text.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::post as read_post;
    use super::*;

    fn target(link: &str) -> Target {
        Target::from_link(link).unwrap()
    }

    /// A post as `/comments/{id}.json` has it, cut to what's read.
    fn listing(post: &Value, comment: Option<Value>) -> Value {
        let comments = comment.map_or_else(Vec::new, |c| vec![json!({ "kind": "t1", "data": c })]);
        json!([
            { "kind": "Listing", "data": { "children": [{ "kind": "t3", "data": post }] } },
            { "kind": "Listing", "data": { "children": comments } }
        ])
    }

    fn video_post() -> Value {
        json!({
            "id": "1xydu12", "title": "Relighting a candle", "author": "someone", "subreddit_name_prefixed": "r/interesting",
            "created_utc": 1_794_000_000.0, "score": 14_000, "num_comments": 192, "num_crossposts": 3,
            "permalink": "/r/interesting/comments/1xydu12/relighting/", "is_self": false, "selftext": "",
            "url_overridden_by_dest": "https://v.redd.it/3xdmcdmjg42i1",
            "preview": { "images": [{ "source": { "url": "https://external-preview.redd.it/poster.png?s=1", "width": 720, "height": 1280 } }] },
            "secure_media": { "reddit_video": {
                "fallback_url": "https://v.redd.it/3xdmcdmjg42i1/CMAF_720.mp4?source=fallback", "width": 720, "height": 1280,
                "dash_url": "https://v.redd.it/3xdmcdmjg42i1/DASHPlaylist.mpd?a=1797511159%2Cx&v=1&f=sd",
                "has_audio": true, "is_gif": false, "duration": 9
            } }
        })
    }

    #[test]
    fn a_video_post() {
        let (post, video) = read_post(&listing(&video_post(), None), &target("https://redd.it/1xydu12"), None).unwrap();
        assert_eq!(post.title.as_deref(), Some("Relighting a candle"));
        assert_eq!((post.author.as_deref(), post.group.as_deref()), (Some("u/someone"), Some("r/interesting")));
        assert_eq!(post.url, "https://www.reddit.com/r/interesting/comments/1xydu12/relighting/");
        assert_eq!(post.text, None, "the link is the video itself");
        assert_eq!(post.published_at, Some(1_794_000_000));
        assert_eq!((post.stats.reactions, post.stats.comments, post.stats.shares), (Some(14_000), Some(192), Some(3)));
        let media = &post.media[0];
        assert_eq!(
            (media.width, media.thumbnail.as_deref()),
            (Some(720), Some("https://external-preview.redd.it/poster.png?s=1"))
        );
        let video = video.unwrap();
        assert_eq!((video.id.as_str(), video.has_audio, video.duration_seconds), ("3xdmcdmjg42i1", true, Some(9)));
    }

    #[test]
    fn a_gallery_in_order_and_a_crosspost_of_it() {
        let gallery = json!({
            "id": "1xy2btn", "title": "Firms", "author": "cat", "subreddit_name_prefixed": "r/interesting",
            "permalink": "/r/interesting/comments/1xy2btn/firms/", "is_gallery": true, "selftext": "",
            "gallery_data": { "items": [{ "media_id": "b" }, { "media_id": "gone", "is_deleted": true }, { "media_id": "a" }] },
            "media_metadata": {
                "a": { "status": "valid", "e": "Image", "s": { "x": 10, "y": 20, "u": "https://preview.redd.it/a.jpeg?width=10&s=1" } },
                "b": { "status": "valid", "e": "AnimatedImage", "s": { "x": 30, "y": 40, "gif": "https://i.redd.it/b.gif", "mp4": "https://preview.redd.it/b.gif?format=mp4&s=2" } },
                "gone": { "status": "valid", "e": "Image", "s": { "u": "https://preview.redd.it/gone.jpeg" } }
            }
        });
        let (post, _) =
            read_post(&listing(&gallery, None), &target("https://www.reddit.com/gallery/1xy2btn"), None).unwrap();
        let urls: Vec<&str> = post.media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://preview.redd.it/b.gif?format=mp4&s=2", "https://preview.redd.it/a.jpeg?width=10&s=1"]
        );
        assert_eq!(post.media[1].height, Some(20));

        let crosspost = json!({
            "id": "1xy6pm9", "title": "Look at this", "author": "other", "subreddit_name_prefixed": "r/prepper",
            "permalink": "/r/prepper/comments/1xy6pm9/look/", "selftext": "", "url_overridden_by_dest": "https://www.reddit.com/gallery/1xy2btn",
            "crosspost_parent_list": [gallery]
        });
        let (post, _) = read_post(&listing(&crosspost, None), &target("https://redd.it/1xy6pm9"), None).unwrap();
        assert_eq!(post.media.len(), 2, "the shared post's media");
        assert_eq!(post.text, None, "a link to Reddit itself isn't text");
        let shared = post.shared.unwrap();
        assert_eq!(shared.author.as_deref(), Some("u/cat in r/interesting"));
        assert_eq!(shared.title.as_deref(), Some("Firms"), "a headline of its own");
        assert_eq!(shared.url.as_deref(), Some("https://www.reddit.com/r/interesting/comments/1xy2btn/firms/"));
    }

    #[test]
    fn a_text_posts_link_preview_isnt_its_photo() {
        let post = json!({
            "id": "z1c9z", "title": "AMA", "author": "op", "subreddit_name_prefixed": "r/IAmA", "is_self": true,
            "selftext": "Proof: https://example.com/proof", "permalink": "/r/IAmA/comments/z1c9z/ama/", "over_18": false,
            "preview": {
                "images": [{ "source": { "url": "https://external-preview.redd.it/p.jpg?s=1", "width": 640, "height": 480 } }],
                "reddit_video_preview": { "fallback_url": "https://v.redd.it/gif123/CMAF_480.mp4", "width": 640, "height": 480 }
            }
        });
        let link = target("https://www.reddit.com/r/IAmA/comments/z1c9z/");
        let (post, _) = read_post(&listing(&post, None), &link, None).unwrap();
        assert_eq!((post.kind, post.media.len()), (PostKind::TextOnly, 0), "neither its picture nor its clip");
    }

    #[test]
    fn a_comment_answers_the_post() {
        let text_post = json!({
            "id": "z1c9z", "title": "AMA", "author": "op", "subreddit_name_prefixed": "r/IAmA", "is_self": true,
            "selftext": "Ask me **anything**", "permalink": "/r/IAmA/comments/z1c9z/ama/", "over_18": false
        });
        let comment = json!({ "id": "c60nmtc", "author": "someone", "body": "It's hard", "score": 2947,
            "created_utc": 1_346_274_298.0, "permalink": "/r/IAmA/comments/z1c9z/ama/c60nmtc/" });
        let link = target("https://www.reddit.com/r/IAmA/comments/z1c9z/comment/c60nmtc/");
        let (post, _) = read_post(&listing(&text_post, Some(comment)), &link, Some("c60nmtc")).unwrap();
        assert_eq!(post.title.as_deref(), Some("Re: AMA"));
        assert_eq!((post.author.as_deref(), post.text.as_deref()), (Some("u/someone"), Some("It's hard")));
        assert_eq!(post.url, "https://www.reddit.com/r/IAmA/comments/z1c9z/ama/c60nmtc/");
        let answered = post.shared.unwrap();
        assert_eq!(
            (answered.author.as_deref(), answered.text.as_deref()),
            (Some("u/op in r/IAmA"), Some("Ask me **anything**"))
        );
        assert_eq!(post.kind, PostKind::TextOnly);

        let gone = read_post(&listing(&text_post, None), &link, Some("c60nmtc")).unwrap_err();
        assert_eq!(gone.stage, Stage::NotFound);
    }

    #[test]
    fn marks_link_posts_and_refusals() {
        let link = json!({ "id": "a1", "title": "News", "author": "[deleted]", "over_18": true, "is_self": false,
            "selftext": "worth a read", "url_overridden_by_dest": "https://example.com/story",
            "preview": { "images": [{ "source": { "url": "https://external-preview.redd.it/p.jpg?s=1", "width": 640, "height": 320 } }] } });
        let (post, video) = read_post(&listing(&link, None), &target("https://redd.it/a1"), None).unwrap();
        assert_eq!(post.title.as_deref(), Some("NSFW · News"));
        assert_eq!(post.author, None, "a deleted account has no name");
        assert_eq!(post.text.as_deref(), Some("worth a read\n\nhttps://example.com/story"));
        assert_eq!(
            (post.media[0].kind, post.media[0].width, video),
            (crate::extract::MediaKind::Photo, Some(640), None)
        );
        let refused = refusal(&json!({ "reason": "quarantined", "error": 403 })).unwrap();
        assert_eq!(refused.stage, Stage::LoginWall);
    }

    #[test]
    fn communities_and_people() {
        let about = json!({ "kind": "t5", "data": { "display_name_prefixed": "r/rust", "title": "Rust",
            "public_description": "A place for Rust", "subscribers": 350_000, "community_icon": "https://styles.redditmedia.com/icon.png?s=1" } });
        let post = community(&about, &target("https://www.reddit.com/r/rust")).unwrap();
        assert_eq!(post.title.as_deref(), Some("r/rust"), "a title that's just the name isn't repeated");
        assert_eq!(post.text.as_deref(), Some("A place for Rust\n\n350K members"));
        assert_eq!(post.profile.picture.as_deref(), Some("https://styles.redditmedia.com/icon.png?s=1"));
        let about = json!({ "kind": "t2", "data": { "name": "spez", "icon_img": "https://styles.redditmedia.com/p.png",
            "snoovatar_img": "", "total_karma": 941_417, "subreddit": { "public_description": "Reddit CEO" } } });
        let post = person(&about, &target("https://www.reddit.com/u/spez")).unwrap();
        assert_eq!((post.title.as_deref(), post.kind), (Some("u/spez"), PostKind::Profile));
        assert_eq!(post.text.as_deref(), Some("Reddit CEO\n\n941K karma"));
    }

    #[test]
    fn the_crawlers_card() {
        let page = crate::extract::page::parse(
            br#"<meta name="description" content="216K votes, 23K comments. Hi, I'm here. Ask me anything.">
            <meta property="og:image" content="https://share.redd.it/preview/post/z1c9z">
            <meta property="og:title" content="From the IAmA community on Reddit">
            <meta property="og:image:alt" content="From the IAmA community on Reddit: &quot;I am here -- AMA&quot;">
            <meta property="og:url" content="https://www.reddit.com/r/IAmA/comments/z1c9z/ama/">"#,
        )
        .unwrap();
        let post = crawler_post(&page, &target("https://redd.it/z1c9z")).unwrap();
        assert_eq!((post.title.as_deref(), post.group.as_deref()), (Some("I am here -- AMA"), Some("r/IAmA")));
        assert_eq!(post.text.as_deref(), Some("Hi, I'm here. Ask me anything."));
        assert_eq!((post.stats.reactions, post.stats.comments), (Some(216_000), Some(23_000)));
        let (_, text) =
            counted_text("14K votes, 192 comments. 17M subscribers in the interesting community. For anything");
        assert_eq!(text, None, "the community's description isn't the post's text");
    }
}
