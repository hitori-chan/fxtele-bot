//! Reading Reddit's answers (`docs/reddit.md`): a post's JSON, with the comment a link names and
//! the post a crosspost shares; a user's or community's `about.json`; and, without cookies, the
//! tags Reddit gives link-preview crawlers and what a post's or a comment's embed page adds.

use std::cell::RefCell;
use std::rc::Rc;

use lol_html::{RewriteStrSettings, element, rewrite_str, text};
use serde_json::Value;

use super::url::ORIGIN;
use crate::extract::page::{Page, decode_entities};
use crate::extract::{
    AuthorProfile, ExtractError, Media, PageType, Post, PostKind, Relation, Shared, Stage, Stats, Target, on_origin,
    origin_path, short_count,
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
        // A crosspost's media are the post it shares, so its marks count too.
        sensitive: [Some(data), parent].into_iter().flatten().any(|d| flag(d, "over_18") || flag(d, "spoiler")),
    };
    if let Some(id) = comment {
        let found = response
            .pointer("/1/data/children/0")
            .filter(|c| text(c, "/kind") == Some("t1") && text(c, "/data/id") == Some(id))
            .and_then(|c| c.get("data"))
            .ok_or_else(|| ExtractError::new(Stage::NotFound, "no such comment: it's gone, or the link is wrong"))?;
        // The comment first, the post below it as what it answers.
        let mut answered = shared(data);
        (answered.title, answered.relation) = (None, Relation::Reply);
        post.shared = Some(answered);
        post.title = title.map(|t| format!("Re: {t}"));
        post.author = user(text(found, "/author"));
        post.profile = profile(text(found, "/author"));
        post.published_at = number(found, "/created_utc");
        post.text = text(found, "/body").map(str::to_string);
        post.url = text(found, "/permalink").and_then(|p| origin_path(ORIGIN, p)).unwrap_or(post.url);
        let hidden = found.get("score_hidden").and_then(Value::as_bool) == Some(true);
        post.stats = Stats { reactions: count(found, "/score").filter(|_| !hidden), ..Stats::default() };
    }
    Ok((post, video))
}

/// Whether Reddit marks the post `name` (`over_18`, `spoiler`).
fn flag(data: &Value, name: &str) -> bool {
    data.get(name).and_then(Value::as_bool) == Some(true)
}

/// The headline, marked when Reddit marks the post.
fn headline(data: &Value) -> Option<String> {
    let title = text(data, "/title")?;
    Some(match (flag(data, "over_18"), flag(data, "spoiler")) {
        (true, _) => format!("NSFW · {title}"),
        (false, true) => format!("Spoiler · {title}"),
        _ => title.to_string(),
    })
}

fn permalink(data: &Value) -> Option<String> {
    text(data, "/permalink").and_then(|p| origin_path(ORIGIN, p))
}

pub(super) fn profile(author: Option<&str>) -> AuthorProfile {
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
        relation: Relation::Share,
        post_owns_media: false,
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
            // Fetched as is, so only v.redd.it's: the server fetches nothing else a page names.
            playlist: text(v, "/dash_url").filter(|u| u.starts_with("https://v.redd.it/")).map(str::to_string),
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
        sensitive: false,
    }
}

/// The card Reddit's tags for link-preview crawlers make: the headline, the start of the text
/// with the counts before it ("216K votes, 23K comments. ..."), and a picture of the post; for a
/// user or community, its name and description.
pub fn crawler_post(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    let missing = || ExtractError::new(Stage::NotFound, "Reddit's page for crawlers had no post");
    let og_title = page.meta("og:title").ok_or_else(missing)?;
    let alt = page.meta("og:image:alt");
    // Reddit's logo, the picture of pages without one of their own (and of NSFW posts), says
    // nothing.
    let picture = page
        .meta("og:image")
        .filter(|url| crate::extract::trusted_media_url(url))
        .filter(|_| alt != Some("Reddit logo"));
    if target.page_type == PageType::Profile {
        let text = page.meta("og:description").map(str::to_string);
        let name = Some(og_title.to_string());
        return Ok(profile_post(target, name.clone(), name, text, picture.map(str::to_string)));
    }
    let (nsfw, og_title) = og_title.strip_prefix("[Mature Content] ").map_or((false, og_title), |t| (true, t));
    let marked = |title: &str| if nsfw { format!("NSFW · {title}") } else { title.to_string() };
    let (stats, text) = page.meta("description").map_or((Stats::default(), None), counted_text);
    let picture = picture.map(|url| Media::photo(url.to_string()));
    let mut post = Post {
        key: target.key().to_string(),
        url: page
            .meta("og:url")
            .filter(|u| on_origin(u, ORIGIN))
            .map_or_else(|| target.url.to_string(), str::to_string),
        page_type: target.page_type,
        title: None,
        author: None,
        profile: AuthorProfile::default(),
        group: None,
        published_at: None,
        text: None,
        kind: if picture.is_some() { PostKind::Media } else { PostKind::TextOnly },
        media: picture.into_iter().collect(),
        stats: Stats::default(),
        shared: None,
        sensitive: nsfw,
    };
    // A comment's: `{name}'s comment on "{the post's title}"`, its community in the picture's
    // description (`{sub} community on Reddit: {name} commented "…"`), and the counts and text of
    // the post it answers.
    if let super::url::Link::Comment(post_id, comment_id) = super::url::link(target)
        && let Some((name, headline)) = og_title.split_once("'s comment on ")
    {
        let community = alt.and_then(|a| a.split_once(" community on Reddit: ")).map(|(c, _)| format!("r/{c}"));
        let name = Some(name).filter(|n| super::url::is_name(n));
        // The comment's page is under its post's, wherever the link said it was.
        let post_url = page
            .meta("og:url")
            .filter(|u| on_origin(u, ORIGIN))
            .and_then(|u| u.trim_end_matches('/').strip_suffix(comment_id.as_str()))
            .map(str::to_string);
        post.title = Some(format!("Re: {}", marked(headline.trim_matches('"'))));
        post.author = name.map(|n| format!("u/{n}"));
        post.profile = profile(name);
        post.shared = Some(Shared {
            url: post_url.or_else(|| Some(format!("{ORIGIN}/comments/{post_id}/"))),
            author: community.clone(),
            text,
            relation: Relation::Reply,
            // Reddit's picture of the comment, until the embed page gives its words.
            post_owns_media: true,
            ..Shared::default()
        });
        post.group = community;
        return Ok(post);
    }
    // The headline is in the title, or for text posts only in the picture's description.
    let title = og_title
        .split_once("community on Reddit: ")
        .map(|(_, t)| t)
        .or_else(|| alt.and_then(|alt| alt.split_once("community on Reddit: ")).map(|(_, t)| t))
        .map(|t| marked(t.trim_matches('"')));
    if title.is_none() && text.is_none() {
        return Err(missing());
    }
    post.group = og_title
        .strip_prefix("From the ")
        .and_then(|t| t.split_once(" community on Reddit"))
        .map(|(c, _)| format!("r/{c}"));
    (post.title, post.text, post.stats) = (title, text, stats);
    Ok(post)
}

/// What a post's or a comment's embed page shows without cookies.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Embed {
    /// `image`, `gallery`, `video`, `gif`, `link`, `text` (a headline alone), `multi_media` (a
    /// text post), `comment`...
    pub kind: String,
    /// What the post links: an `image`'s full-size file.
    pub file: Option<String>,
    /// Where a `link` post leads, unless it's Reddit's own.
    pub link: Option<String>,
    /// A `gif`'s clip: the MP4 Reddit made of it, without sound.
    pub clip: Option<String>,
    pub nsfw: bool,
    pub published_at: Option<i64>,
    pub score: Option<u64>,
    /// The name, without `u/`.
    pub author: Option<String>,
    /// A `gallery`'s pictures, each at the largest size offered.
    pub gallery: Vec<String>,
    pub poster: Option<String>,
    /// A text post's whole text, in paragraphs.
    pub text: Option<String>,
}

/// Reads the embed page of `thing`, a post (`t3_{id}`) or a comment (`t1_{id}`). `None` when it
/// isn't that one's.
pub fn embed(html: &str, thing: &str) -> Option<Embed> {
    let data = RefCell::new(None);
    let found = RefCell::new(Embed::default());
    // Shared with the handlers of links' end tags, which outlive this call's borrows.
    let blocks = Rc::new(RefCell::new(Vec::<Block>::new()));
    let start_block = |mark: &'static str| blocks.borrow_mut().push(Block { mark, pieces: Vec::new() });
    let handlers = vec![
        element!("shreddit-screenview-data[data]", |e| {
            data.borrow_mut().get_or_insert(e.get_attribute("data").unwrap_or_default());
            Ok(())
        }),
        element!(r#"a[href^="https://www.reddit.com/user/"]"#, |e| {
            let href = e.get_attribute("href").unwrap_or_default();
            let name = href.trim_start_matches("https://www.reddit.com/user/").split(['/', '?']).next();
            let mut found = found.borrow_mut();
            if found.author.is_none() {
                found.author = name.filter(|n| super::url::is_name(n)).map(str::to_string);
            }
            Ok(())
        }),
        element!("gallery-carousel li img[srcset]", |e| {
            if let Some(file) = largest(&decode_entities(&e.get_attribute("srcset").unwrap_or_default())) {
                found.borrow_mut().gallery.push(file);
            }
            Ok(())
        }),
        element!("shreddit-player[poster]", |e| {
            let poster = decode_entities(&e.get_attribute("poster").unwrap_or_default());
            found.borrow_mut().poster = Some(poster).filter(|p| crate::extract::trusted_media_url(p));
            Ok(())
        }),
        element!("shreddit-player[gif][src]", |e| {
            let clip = decode_entities(&e.get_attribute("src").unwrap_or_default());
            found.borrow_mut().clip = Some(clip).filter(|c| crate::extract::trusted_media_url(c));
            Ok(())
        }),
        element!(BLOCKS, |_| {
            // A list item's first paragraph is the item's.
            let in_item = blocks.borrow().last().is_some_and(|b| b.mark == "- " && b.pieces.is_empty());
            if !in_item {
                start_block("");
            }
            Ok(())
        }),
        element!(r#"[id$="-post-rtjson-content"] li"#, |_| {
            start_block("- ");
            Ok(())
        }),
        element!(r#"[id$="-post-rtjson-content"] a[href]"#, |e| {
            let href = decode_entities(&e.get_attribute("href").unwrap_or_default());
            if let Some(block) = blocks.borrow_mut().last_mut() {
                block.pieces.push(Piece::Link(href));
            }
            let blocks = Rc::clone(&blocks);
            let close: lol_html::EndTagHandler<'static> = Box::new(move |_| {
                if let Some(block) = blocks.borrow_mut().last_mut() {
                    block.pieces.push(Piece::LinkEnd);
                }
                Ok(())
            });
            e.on_end_tag(close)?;
            Ok(())
        }),
        text!(r#"[id$="-post-rtjson-content"]"#, |chunk| {
            if let Some(block) = blocks.borrow_mut().last_mut() {
                match block.pieces.last_mut() {
                    Some(Piece::Text(text)) => text.push_str(chunk.as_str()),
                    _ => block.pieces.push(Piece::Text(chunk.as_str().to_string())),
                }
            }
            Ok(())
        }),
    ];
    let settings = handlers.into_iter().fold(RewriteStrSettings::new(), |s, h| s.append_element_content_handler(h));
    rewrite_str(html, settings).ok()?;

    let data: Value = serde_json::from_str(&decode_entities(&data.into_inner()?)).ok()?;
    let item = data.get(if thing.starts_with("t1_") { "comment" } else { "post" })?;
    if text(item, "/id")? != thing {
        return None;
    }
    let mut found = found.into_inner();
    found.kind = text(item, "/type").unwrap_or_default().to_string();
    let url = text(item, "/url");
    found.file = url.filter(|u| crate::extract::trusted_media_url(u)).map(str::to_string);
    found.link = url.filter(|u| found.kind == "link" && !is_reddits_own(u)).map(str::to_string);
    found.nsfw = item.get("nsfw").and_then(Value::as_bool).unwrap_or(false);
    found.published_at = number(item, "/created_timestamp").map(|ms| ms / 1000);
    found.score = item.get("score").and_then(Value::as_u64);
    let paragraphs: Vec<String> = blocks.borrow().iter().filter_map(Block::markdown).collect();
    found.text = (!paragraphs.is_empty()).then(|| paragraphs.join("\n\n"));
    Some(found)
}

/// Where a text post's embed page starts a paragraph of its own.
const BLOCKS: &str = r#"[id$="-post-rtjson-content"] p, [id$="-post-rtjson-content"] h1, [id$="-post-rtjson-content"] h2,
    [id$="-post-rtjson-content"] h3, [id$="-post-rtjson-content"] h4, [id$="-post-rtjson-content"] h5,
    [id$="-post-rtjson-content"] h6, [id$="-post-rtjson-content"] pre, [id$="-post-rtjson-content"] blockquote"#;

/// A paragraph of a text post's text as its embed page shows it: `- ` for a list item, and its
/// text, which Reddit already formatted, with its links.
struct Block {
    mark: &'static str,
    pieces: Vec<Piece>,
}

enum Piece {
    Text(String),
    Link(String),
    LinkEnd,
}

impl Block {
    /// As Markdown, as a post's text is read: the text escaped, so a `*` or `1.` in it stays
    /// itself, and the links as links. `None` when it holds no text.
    fn markdown(&self) -> Option<String> {
        let mut out = String::new();
        let mut open: Vec<Option<&str>> = Vec::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(text) => out.push_str(&crate::markdown::escape(&decode_entities(text))),
                // Only a link Markdown can hold whole; another stays its text.
                Piece::Link(href) => {
                    let whole =
                        !href.is_empty() && !href.contains(|c: char| c.is_whitespace() || matches!(c, '<' | '>'));
                    open.push(whole.then_some(href.as_str()));
                    if whole {
                        out.push('[');
                    }
                }
                Piece::LinkEnd => {
                    if let Some(Some(href)) = open.pop() {
                        out.push_str("](<");
                        out.push_str(href);
                        out.push_str(">)");
                    }
                }
            }
        }
        let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()).then(|| format!("{}{text}", self.mark))
    }
}

/// The largest of a `srcset`'s files on Reddit's own hosts.
fn largest(srcset: &str) -> Option<String> {
    srcset
        .split(", ")
        .filter_map(|file| {
            let (url, width) = file.trim().rsplit_once(' ')?;
            Some((width.strip_suffix('w')?.parse::<u32>().ok()?, url))
        })
        .filter(|(_, url)| crate::extract::trusted_media_url(url))
        .max_by_key(|(width, _)| *width)
        .map(|(_, url)| url.to_string())
}

/// "216K votes, 23K comments. The text…": the counts, and the text after them. A community's
/// description ("17M subscribers in the … community.") isn't the post's.
fn counted_text(description: &str) -> (Stats, Option<String>) {
    let (stats, rest) = match description.split_once(". ").and_then(|(c, rest)| Some((counts(c)?, rest))) {
        Some(counted) => counted,
        None => (Stats::default(), description),
    };
    // A post without text gets its community's description ("17M subscribers in the … community.").
    let about_community = rest
        .split_once(' ')
        .is_some_and(|(n, after)| short_count(n).is_some() && after.starts_with("subscribers in the "));
    (stats, Some(rest.trim()).filter(|t| !t.is_empty() && !about_community).map(str::to_string))
}

/// "216K votes, 23K comments".
fn counts(text: &str) -> Option<Stats> {
    let mut stats = Stats::default();
    for part in text.split(", ") {
        match part.split_once(' ')? {
            (n, "votes" | "vote") => stats.reactions = short_count(n),
            (n, "comments" | "comment") => stats.comments = short_count(n),
            _ => return None,
        }
    }
    Some(stats)
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
        assert!(post.sensitive, "its media stay off the card");
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

    /// An embed page, cut to what's read.
    fn embed_page(kind: &str, url: &str, content: &str) -> String {
        let data = json!({ "post": { "id": "t3_1abc2d3", "url": url, "nsfw": false,
            "created_timestamp": 1_794_923_037_387_i64, "type": kind } })
        .to_string()
        .replace('"', "&quot;");
        format!(
            r#"<shreddit-screenview-data data="{data}"></shreddit-screenview-data>
            <span>Posted by</span> <a href="https://www.reddit.com/user/some_one/?utm_source=embedv2">some_one</a>
            {content}"#
        )
    }

    #[test]
    fn a_communitys_description_isnt_the_posts_text() {
        let (stats, text) = counted_text("17M subscribers in the interesting community. For anything interesting");
        assert_eq!((stats, text), (Stats::default(), None));
        let (stats, text) = counted_text("404 votes, 77 comments. The start of the text…");
        assert_eq!(
            (stats.reactions, stats.comments, text.as_deref()),
            (Some(404), Some(77), Some("The start of the text…"))
        );
        let (stats, text) = counted_text("Just one sentence");
        assert_eq!((stats, text.as_deref()), (Stats::default(), Some("Just one sentence")), "no counts, all text");
    }

    #[test]
    fn an_embed_page_says_what_the_post_is() {
        let image = embed_page("image", "https://i.redd.it/picture.jpeg", "");
        let read = embed(&image, "t3_1abc2d3").unwrap();
        assert_eq!((read.kind.as_str(), read.file.as_deref()), ("image", Some("https://i.redd.it/picture.jpeg")));
        assert_eq!((read.author.as_deref(), read.published_at), (Some("some_one"), Some(1_794_923_037)));
        assert_eq!(embed(&image, "t3_9zzz9zz"), None, "another post's page");
        assert_eq!(embed("<p>nothing</p>", "t3_1abc2d3"), None);

        let player = r#"<shreddit-player src="https://preview.redd.it/a.gif?width=480&amp;format=mp4&amp;s=1" gif
            poster="https://preview.redd.it/a.gif?format=png8&amp;s=2"></shreddit-player>"#;
        let gif = embed(&embed_page("gif", "https://i.redd.it/a.gif", player), "t3_1abc2d3").unwrap();
        assert_eq!(
            (gif.clip.as_deref(), gif.poster.as_deref()),
            (
                Some("https://preview.redd.it/a.gif?width=480&format=mp4&s=1"),
                Some("https://preview.redd.it/a.gif?format=png8&s=2")
            )
        );
        let link = embed(&embed_page("link", "https://youtu.be/x", ""), "t3_1abc2d3").unwrap();
        assert_eq!((link.link.as_deref(), link.file), (Some("https://youtu.be/x"), None));
        let own = embed(&embed_page("link", "https://www.reddit.com/r/x/comments/1abc2d3/", ""), "t3_1abc2d3");
        assert_eq!(own.unwrap().link, None, "Reddit's own page");

        let srcset = "https://preview.redd.it/a.jpg?width=320&amp;s=1 320w, https://preview.redd.it/a.jpg?width=1080&amp;s=3 1080w, \
                      https://preview.redd.it/a.jpg?width=640&amp;s=2 640w";
        let gallery = format!(
            r#"<gallery-carousel><ul><li><img srcset="{srcset}"></li>
            <li><img srcset="https://elsewhere.example/b.jpg 1080w"></li></ul></gallery-carousel>"#
        );
        let read =
            embed(&embed_page("gallery", "https://www.reddit.com/gallery/1abc2d3", &gallery), "t3_1abc2d3").unwrap();
        assert_eq!(read.gallery, ["https://preview.redd.it/a.jpg?width=1080&s=3"], "the largest, on Reddit's hosts");

        let body = r#"<div id="t3_1abc2d3-post-rtjson-content" class="md"><p>
              It&#39;s <b>all</b>
              here: <a href="https://example.com/a">a link</a>
            </p><ul><li>one</li><li><p>two</p></li></ul></div><p>Not the post's</p>"#;
        let read =
            embed(&embed_page("multi_media", "https://www.reddit.com/r/x/comments/1abc2d3/", body), "t3_1abc2d3");
        let text = read.unwrap().text.unwrap();
        assert_eq!(text, "It\\'s all here\\: [a link](<https://example.com/a>)\n\n- one\n\n- two");
        assert_eq!(crate::markdown::to_text(&text), "It's all here: a link\n\n• one\n\n• two", "read back as shown");

        let literal =
            r#"<div id="t3_1abc2d3-post-rtjson-content"><h2>1. *Not* a list</h2><p>&gt; nor a quote</p></div>"#;
        let read =
            embed(&embed_page("multi_media", "https://www.reddit.com/r/x/comments/1abc2d3/", literal), "t3_1abc2d3");
        assert_eq!(crate::markdown::to_text(&read.unwrap().text.unwrap()), "1. *Not* a list\n\n> nor a quote");
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
        let (stats, text) =
            counted_text("14K votes, 192 comments. 17M subscribers in the interesting community. For anything");
        assert_eq!(
            (stats.reactions, stats.comments, text),
            (Some(14_000), Some(192), None),
            "the community's description isn't the post's text"
        );
    }

    #[test]
    fn the_crawlers_card_of_an_nsfw_post_and_of_a_comment() {
        let nsfw = crate::extract::page::parse(
            br#"<meta name="description" content="8.1K votes, 3.1K comments. 60M subscribers in the AskReddit community.">
            <meta property="og:image" content="https://i.redd.it/logo.png">
            <meta property="og:image:alt" content="Reddit logo">
            <meta property="og:title" content="[Mature Content] From the AskReddit community on Reddit: What is it?">"#,
        )
        .unwrap();
        let post = crawler_post(&nsfw, &target("https://www.reddit.com/r/AskReddit/comments/1abc2d3/")).unwrap();
        assert_eq!(
            (post.title.as_deref(), post.group.as_deref(), post.sensitive),
            (Some("NSFW · What is it?"), Some("r/AskReddit"), true)
        );
        assert_eq!(
            (post.stats.reactions, post.media.len()),
            (Some(8_100), 0),
            "Reddit's logo isn't the post's picture"
        );

        let comment = crate::extract::page::parse(
            br#"<meta name="description" content="216K votes, 23K comments. Hi, I'm here.">
            <meta property="og:image" content="https://share.redd.it/preview/comment/c60o0iw">
            <meta property="og:title" content="Some_One&#39;s comment on &quot;I am here -- AMA&quot;">
            <meta property="og:url" content="https://www.reddit.com/r/IAmA/comments/z1c9z/ama/c60o0iw/">
            <meta property="og:image:alt" content="IAmA community on Reddit: Some_One commented &quot;Here it is&quot;">"#,
        )
        .unwrap();
        let link = target("https://www.reddit.com/r/IAmA/comments/z1c9z/comment/c60o0iw/");
        let post = crawler_post(&comment, &link).unwrap();
        assert_eq!(
            (post.title.as_deref(), post.author.as_deref(), post.group.as_deref()),
            (Some("Re: I am here -- AMA"), Some("u/Some_One"), Some("r/IAmA"))
        );
        assert_eq!((post.text, post.stats), (None, Stats::default()), "the counts and text are the post's");
        let answered = post.shared.unwrap();
        assert_eq!(
            (answered.relation, answered.url.as_deref(), answered.text.as_deref()),
            (Relation::Reply, Some("https://www.reddit.com/r/IAmA/comments/z1c9z/ama/"), Some("Hi, I'm here."))
        );
        assert!(answered.post_owns_media, "Reddit's picture is of the comment");
        let mut no_url = comment;
        no_url.meta.remove("og:url");
        let answered = crawler_post(&no_url, &link).unwrap().shared.unwrap();
        assert_eq!(answered.url.as_deref(), Some("https://www.reddit.com/comments/z1c9z/"));
    }

    #[test]
    fn a_comments_embed_page() {
        let data = json!({ "comment": { "id": "t1_c60o0iw", "post_id": "t3_z1c9z", "score": 5399, "type": "comment",
            "created_timestamp": 1_346_276_659_660_i64 } })
        .to_string()
        .replace('"', "&quot;");
        let html = format!(
            r#"<shreddit-screenview-data data="{data}"></shreddit-screenview-data>
            <a href="https://www.reddit.com/user/Some_One/?utm_source=embedv2">Some_One</a>
            <div id="t1_c60o0iw-post-rtjson-content"><p>All of it</p></div>"#
        );
        let read = embed(&html, "t1_c60o0iw").unwrap();
        assert_eq!(
            (read.kind.as_str(), read.author.as_deref(), read.score, read.published_at),
            ("comment", Some("Some_One"), Some(5399), Some(1_346_276_659))
        );
        assert_eq!(read.text.as_deref(), Some("All of it"));
        assert_eq!(embed(&html, "t3_z1c9z"), None, "not the post's page");
    }
}
