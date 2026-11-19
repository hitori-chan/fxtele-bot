//! Reading Instagram's answers into a [`Post`] (`docs/instagram.md`): the media item its post
//! query and logged-in API give, the embed page's older `shortcode_media`, the profile query, the
//! stories a logged-in page preloads, and the tags it gives link-preview crawlers.

use serde_json::Value;

use super::url::{ORIGIN, created_at, media_id};
use crate::extract::facebook::dash;
use crate::extract::page::Page;
use crate::extract::{AuthorProfile, ExtractError, Media, PageType, Post, PostKind, Stage, Stats, Target, on_origin};

/// GraphQL's error for a throttled client.
const RATE_LIMITED: i64 = 1_675_004;
/// GraphQL's error for a post it won't show logged out: private, age-restricted, limited to some
/// countries, or gone. It can't tell which.
const NOT_SHOWN: i64 = 1_675_030;

fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// An ID, which the query gives as a string and the logged-in API as a number.
fn id(value: &Value, pointer: &str) -> Option<String> {
    match value.pointer(pointer)? {
        Value::String(id) => Some(id.trim().to_string()).filter(|id| !id.is_empty()),
        Value::Number(id) => id.as_u64().map(|id| id.to_string()),
        _ => None,
    }
}

fn number(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

/// What a GraphQL answer's `errors` say, if anything.
pub fn query_error(response: &Value) -> Option<ExtractError> {
    let error = response.pointer("/errors/0")?;
    let code = error.get("code").and_then(Value::as_i64).unwrap_or_default();
    let message = text(error, "/message").unwrap_or("no message");
    Some(match code {
        RATE_LIMITED => ExtractError::new(Stage::RateLimited, "Instagram asked to wait before more requests"),
        _ if message.contains("Please wait") => ExtractError::new(Stage::RateLimited, message),
        NOT_SHOWN => ExtractError::new(
            Stage::NotFound,
            "Instagram doesn't show this post logged out: it's private, age-restricted, limited to some countries, or gone",
        ),
        _ => ExtractError::new(Stage::Fetch, format!("Instagram's query failed ({code}): {message}")),
    })
}

/// The post query's answer.
pub fn query_post(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    if let Some(error) = query_error(response) {
        return Err(error);
    }
    let items = response.pointer("/data/xdt_api__v1__media__shortcode__web_info/items");
    match items.and_then(Value::as_array).map(Vec::as_slice) {
        Some([item, ..]) => media_item(item, target),
        // Only an empty list says the post is gone; any other shape means Instagram changed it.
        Some([]) => Err(ExtractError::new(Stage::NotFound, "Instagram has no such post; it may have been deleted")),
        None => Err(ExtractError::new(Stage::Fetch, "Instagram's answer held no post; its format may have changed")),
    }
}

/// The logged-in API's answer (`/api/v1/media/{id}/info/`): the same item as the query's.
pub fn api_post(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    match response.pointer("/items/0") {
        Some(item) => media_item(item, target),
        None => Err(ExtractError::new(Stage::NotFound, "Instagram's API had no such post for the account")),
    }
}

/// A post, reel, carousel or story as Instagram's current API shapes it.
fn media_item(item: &Value, target: &Target) -> Result<Post, ExtractError> {
    let username = text(item, "/user/username").ok_or_else(|| {
        ExtractError::new(Stage::Fetch, "Instagram's post had no author; its format may have changed")
    })?;
    let children: Vec<&Value> = match item.get("carousel_media").and_then(Value::as_array) {
        Some(children) if !children.is_empty() => children.iter().collect(),
        _ => vec![item],
    };
    // Dropping one would shift every later `img_index`, so a broken item fails the post.
    let media = children.into_iter().map(api_media).collect::<Option<Vec<_>>>().ok_or_else(|| {
        ExtractError::new(Stage::Fetch, "a carousel item had no picture; Instagram's format may have changed")
    })?;
    let hidden = item.get("like_and_view_counts_disabled").and_then(Value::as_bool) == Some(true);
    let views =
        ["/play_count", "/ig_play_count", "/view_count"].iter().find_map(|p| number(item, p).filter(|n| *n > 0));
    let stats = Stats {
        reactions: number(item, "/like_count").filter(|_| !hidden),
        comments: number(item, "/comment_count"),
        shares: number(item, "/media_repost_count"),
        views: views.filter(|_| !hidden),
    };
    let code = text(item, "/code").unwrap_or_default();
    let page_type = match text(item, "/product_type") {
        Some("clips") => PageType::Reel,
        Some("story") => PageType::Story,
        _ => PageType::Post,
    };
    let url = match page_type {
        PageType::Story => format!("{ORIGIN}/stories/{username}/{}/", id(item, "/pk").unwrap_or_default()),
        PageType::Reel => format!("{ORIGIN}/reel/{code}/"),
        _ => format!("{ORIGIN}/p/{code}/"),
    };
    let published_at = number(item, "/taken_at").and_then(|t| i64::try_from(t).ok());
    let author = Author::at(item, "/user", username);
    Ok(post(target, url, page_type, &author, text(item, "/caption/text"), media, stats, published_at))
}

/// The stories a logged-in story or highlight page preloads: an account's live ones
/// (`xdt_api__v1__feed__reels_media`) or a highlight's (`…__connection`).
pub fn reel_page(html: &str, target: &Target) -> Result<Post, ExtractError> {
    let read = |key: &str| {
        let start = html.find(key)? + key.len();
        serde_json::Deserializer::from_str(&html[start..]).into_iter::<Value>().next()?.ok()
    };
    let connection = read("\"xdt_api__v1__feed__reels_media__connection\":");
    let feed = read("\"xdt_api__v1__feed__reels_media\":");
    // Not taken for a login wall: a private account's page, or a changed one, lacks them too, and
    // cookies that no longer log in are sent to the login page instead (`is_login`).
    if connection.is_none() && feed.is_none() {
        return Err(ExtractError::new(
            Stage::NotFound,
            "the page held no stories: the account may be private, or Instagram changed its page",
        ));
    }
    // A highlight's page has its reel in the connection, an account's in the feed; a page may
    // preload both, so the one the link names is taken.
    let highlight_link = target.url.path().contains("/highlights/");
    let from_connection = || connection.as_ref().and_then(|c| c.pointer("/edges/0/node")).cloned();
    let from_feed = || feed.as_ref().and_then(|f| f.pointer("/reels_media/0")).cloned();
    let reel = if highlight_link { from_connection().or_else(from_feed) } else { from_feed().or_else(from_connection) };
    let gone =
        || ExtractError::new(Stage::NotFound, "no stories up now: they last a day, and highlights can be removed");
    let reel = reel.filter(|r| !r.is_null()).ok_or_else(gone)?;
    let items = reel.get("items").and_then(Value::as_array).filter(|i| !i.is_empty()).ok_or_else(gone)?;
    let username = text(&reel, "/user/username").or_else(|| text(&items[0], "/user/username")).ok_or_else(gone)?;
    let media: Vec<Media> = items.iter().filter_map(api_media).collect();
    let highlight = text(&reel, "/reel_type") == Some("highlight_reel");
    let url = match (highlight, text(&reel, "/id")) {
        (true, Some(id)) => format!("{ORIGIN}/stories/highlights/{}/", id.trim_start_matches("highlight:")),
        _ => format!("{ORIGIN}/stories/{username}/"),
    };
    // The reel the link asked for, not another the page also holds.
    let path =
        |url: &str| url.split_once("://").map_or(url, |(_, rest)| rest).trim_end_matches('/').to_ascii_lowercase();
    if path(&url) != path(target.url.as_str()) {
        return Err(ExtractError::new(Stage::NotFound, "the page held another account's or highlight's stories"));
    }
    // A highlight spans years; an account's stories are dated by the newest.
    let newest = items.iter().filter_map(|i| number(i, "/taken_at")).max().and_then(|t| i64::try_from(t).ok());
    let author = Author::at(&reel, "/user", username);
    let mut post =
        post(target, url, PageType::Story, &author, None, media, Stats::default(), newest.filter(|_| !highlight));
    post.group = text(&reel, "/title").filter(|_| highlight).map(str::to_string);
    Ok(post)
}

/// An account's stories, logged out: Instagram shows them only to accounts, so the preview is the
/// account's own.
pub fn stories_profile(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    let mut post = profile(response, target)?;
    post.page_type = PageType::Story;
    post.url = target.url.to_string() + "/";
    Ok(post)
}

/// A highlight's tags for crawlers: its cover, and `<title> = @<owner>`.
pub fn highlight_tags(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    let none = || ExtractError::new(Stage::NotFound, "no such highlight, or Instagram gave it no preview tags");
    let cover = page.meta("og:image").filter(|u| crate::extract::trusted_media_url(u)).ok_or_else(none)?;
    let og_title = page.meta("og:title").ok_or_else(none)?;
    let (title, username) = match og_title.rsplit_once(" = @") {
        Some((title, user)) if !user.is_empty() && !user.contains(' ') => (title.trim(), user),
        _ => return Err(none()),
    };
    let author = Author { username, full_name: None, picture: None, full_size: None };
    let url = page
        .meta("og:url")
        .filter(|u| on_origin(u, ORIGIN))
        .map_or_else(|| target.url.to_string() + "/", str::to_string);
    let mut post = post(
        target,
        url,
        PageType::Story,
        &author,
        None,
        vec![Media::photo(cover.to_string())],
        Stats::default(),
        None,
    );
    post.group = Some(title.to_string()).filter(|t| !t.is_empty());
    Ok(post)
}

/// One photo or video of the current API's item.
pub(crate) fn api_media(node: &Value) -> Option<Media> {
    let (image, width, height) = best(node.pointer("/image_versions2/candidates")?)?;
    let size = |key| number(node, key).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    let (width, height) = (size("/original_width").or(width), size("/original_height").or(height));
    let video = node.get("video_versions").and_then(best).filter(|_| number(node, "/media_type") == Some(2));
    let mut media = match video {
        Some((url, video_width, video_height)) => {
            let mut media = Media::video(url, Some(image));
            let lines = video_width.zip(video_height).map(|(w, h)| w.min(h));
            media.hd =
                text(node, "/video_dash_manifest").and_then(|xml| dash::hd_from_manifest(xml, lines.unwrap_or(720)));
            media
        }
        None => Media::photo(image),
    };
    (media.width, media.height) = (width, height);
    Some(media)
}

/// The biggest of a list of `{url, width, height}`, with its size.
pub(crate) fn best(candidates: &Value) -> Option<(String, Option<u32>, Option<u32>)> {
    let size = |c: &Value, key: &str| c.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    candidates
        .as_array()?
        .iter()
        .filter(|c| text(c, "/url").is_some())
        .max_by_key(|c| u64::from(size(c, "width").unwrap_or(0)) * u64::from(size(c, "height").unwrap_or(0)))
        .map(|c| (text(c, "/url").unwrap_or_default().to_string(), size(c, "width"), size(c, "height")))
}

/// The embed page's `gql_data.shortcode_media`: Instagram's older GraphQL shape, which has no full
/// name or date (the date comes from the shortcode).
pub fn embed_post(media: &Value, target: &Target) -> Result<Post, ExtractError> {
    let username = text(media, "/owner/username")
        .ok_or_else(|| ExtractError::new(Stage::Fetch, "the embed page had no post; its format may have changed"))?;
    let broken =
        || ExtractError::new(Stage::Fetch, "an embed item had no picture; Instagram's format may have changed");
    // As with the query's carousel, an item left out would shift every later `img_index`.
    let children: Vec<&Value> = match media.pointer("/edge_sidecar_to_children/edges").and_then(Value::as_array) {
        Some(edges) if !edges.is_empty() => {
            edges.iter().map(|e| e.get("node")).collect::<Option<_>>().ok_or_else(broken)?
        }
        _ => vec![media],
    };
    let items = children
        .into_iter()
        .map(|node| {
            let image = text(node, "/display_url")?.to_string();
            let mut item = match text(node, "/video_url").filter(|_| node.get("is_video") == Some(&Value::Bool(true))) {
                Some(video) => Media::video(video.to_string(), Some(image)),
                None => Media::photo(image),
            };
            let size = |key| number(node, key).and_then(|n| u32::try_from(n).ok());
            (item.width, item.height) = (size("/dimensions/width"), size("/dimensions/height"));
            Some(item)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(broken)?;
    let code = text(media, "/shortcode").unwrap_or_default();
    let stats = Stats {
        reactions: number(media, "/edge_liked_by/count").or_else(|| number(media, "/edge_media_preview_like/count")),
        comments: number(media, "/edge_media_to_comment/count"),
        shares: None,
        views: number(media, "/video_view_count").filter(|n| *n > 0),
    };
    let published_at = number(media, "/taken_at_timestamp")
        .and_then(|t| i64::try_from(t).ok())
        .or_else(|| media_id(code).map(created_at));
    let page_type = if target.page_type == PageType::Reel { PageType::Reel } else { PageType::Post };
    let url =
        if page_type == PageType::Reel { format!("{ORIGIN}/reel/{code}/") } else { format!("{ORIGIN}/p/{code}/") };
    let caption = text(media, "/edge_media_to_caption/edges/0/node/text");
    let author = Author::at(media, "/owner", username);
    Ok(post(target, url, page_type, &author, caption, items, stats, published_at))
}

/// The profile query's answer: the name, bio and picture.
pub fn profile(response: &Value, target: &Target) -> Result<Post, ExtractError> {
    if let Some(error) = query_error(response) {
        return Err(error);
    }
    let user = response.pointer("/data/xig_user_by_username").filter(|u| !u.is_null());
    let Some(username) = user.and_then(|u| text(u, "/username")) else {
        return Err(ExtractError::new(Stage::NotFound, "Instagram has no such account"));
    };
    let user = user.unwrap_or(&Value::Null);
    let name = text(user, "/full_name").unwrap_or(username).to_string();
    let bio = text(user, "/biography").map(str::to_string);
    let picture = text(user, "/profile_pic_url").map(str::to_string);
    Ok(Post {
        key: target.key().to_string(),
        url: format!("{ORIGIN}/{username}/"),
        page_type: PageType::Profile,
        title: Some(name.clone()),
        author: Some(name),
        profile: AuthorProfile {
            url: Some(format!("{ORIGIN}/{username}/")),
            handle: Some(username.to_string()),
            avatar: picture.clone(),
            picture: None,
            bio: bio.clone(),
        },
        group: None,
        published_at: None,
        text: bio,
        media: picture.into_iter().map(Media::photo).collect(),
        kind: PostKind::Profile,
        stats: Stats::default(),
        shared: None,
        sensitive: false,
    })
}

/// The tags Instagram gives link-preview crawlers on a post's page: a cover picture, the author
/// and the caption. They survive most gates, so a post Instagram won't show logged out still gets
/// a one-picture preview.
pub fn crawler_post(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    let none = || ExtractError::new(Stage::NotFound, "Instagram's page had no preview tags either");
    let image = page.meta("og:image").filter(|u| crate::extract::trusted_media_url(u)).ok_or_else(none)?;
    // `og:url` is `/{username}/p/{code}/`; the titles start with the display name, free text.
    let og_url = page.meta("og:url").and_then(|u| url::Url::parse(u).ok()).ok_or_else(none)?;
    let parts: Vec<&str> = og_url.path().split('/').filter(|p| !p.is_empty()).collect();
    let (username, code) = match parts.as_slice() {
        [user, "p" | "reel", code] => (*user, *code),
        _ => return Err(none()),
    };
    let full_name =
        page.meta("twitter:title").and_then(|t| t.split_once(&format!(" (@{username})"))).map(|(n, _)| n.trim());
    // Either copy of the caption may be the cut one.
    let quoted = |text: &str, marker: &str| {
        let rest = text.split_once(marker)?.1;
        Some(rest[..rest.rfind('"')?].to_string())
    };
    let from_title = page.meta("og:title").and_then(|t| quoted(t, " on Instagram: \""));
    let from_description = page.meta("og:description").and_then(|d| quoted(d, ": \""));
    let caption = [from_title, from_description].into_iter().flatten().max_by_key(String::len);
    let page_type = if target.page_type == PageType::Reel { PageType::Reel } else { PageType::Post };
    let url =
        if page_type == PageType::Reel { format!("{ORIGIN}/reel/{code}/") } else { format!("{ORIGIN}/p/{code}/") };
    let author = Author { username, full_name, picture: None, full_size: None };
    let published_at = media_id(code).map(created_at);
    // "30K likes, 129 comments - sejinming on April 17, 2023: …", rounded as Instagram shows them.
    let counts = page.meta("og:description").and_then(|d| d.split_once(" - ")).map_or("", |(counts, _)| counts);
    let count_of =
        |what: &str| counts.split(", ").find_map(|part| crate::extract::short_count(part.strip_suffix(what)?));
    let stats = Stats {
        reactions: count_of("likes").or_else(|| count_of("like")),
        comments: count_of("comments").or_else(|| count_of("comment")),
        ..Stats::default()
    };
    Ok(post(
        target,
        url,
        page_type,
        &author,
        caption.as_deref(),
        vec![Media::photo(image.to_string())],
        stats,
        published_at,
    ))
}

/// Any other page (Instagram's own sections, highlights): its preview tags as they are.
pub fn other_page(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    let image = page.meta("og:image").filter(|u| crate::extract::trusted_media_url(u));
    let title = page.meta("og:title");
    if image.is_none() && title.is_none() {
        return Err(ExtractError::new(Stage::NotFound, "the page had no preview tags"));
    }
    Ok(Post {
        key: target.key().to_string(),
        url: target.url.to_string(),
        page_type: PageType::Other,
        title: title.map(str::to_string),
        author: None,
        profile: AuthorProfile::default(),
        group: None,
        published_at: None,
        text: page.meta("og:description").map(str::to_string),
        media: image.into_iter().map(|u| Media::photo(u.to_string())).collect(),
        kind: if image.is_some() { PostKind::Media } else { PostKind::TextOnly },
        stats: Stats::default(),
        shared: None,
        sensitive: false,
    })
}

struct Author<'a> {
    username: &'a str,
    full_name: Option<&'a str>,
    /// 150 px: fine for a card, a blur at Instant View's width.
    picture: Option<&'a str>,
    /// The picture as it was uploaded, which the post's answer has too, for "About the author".
    full_size: Option<&'a str>,
}

impl<'a> Author<'a> {
    /// The author under `user` (`/user`, `/owner`) in an answer.
    fn at(value: &'a Value, user: &str, username: &'a str) -> Self {
        let field = |name: &str| text(value, &format!("{user}/{name}"));
        Self {
            username,
            full_name: field("full_name"),
            picture: field("profile_pic_url"),
            full_size: field("hd_profile_pic_url_info/url"),
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "the parts every reading of a post has")]
fn post(
    target: &Target,
    url: String,
    page_type: PageType,
    author: &Author<'_>,
    caption: Option<&str>,
    media: Vec<Media>,
    stats: Stats,
    published_at: Option<i64>,
) -> Post {
    let name = author.full_name.unwrap_or(author.username).to_string();
    Post {
        key: target.key().to_string(),
        url,
        page_type,
        title: None,
        author: Some(name),
        profile: AuthorProfile {
            url: Some(format!("{ORIGIN}/{}/", author.username)),
            handle: Some(author.username.to_string()),
            avatar: author.picture.map(str::to_string),
            picture: author.full_size.map(str::to_string),
            bio: None,
        },
        group: None,
        published_at,
        text: caption.map(str::to_string),
        kind: if media.is_empty() { PostKind::TextOnly } else { PostKind::Media },
        media,
        stats,
        shared: None,
        sensitive: false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn target(link: &str) -> Target {
        Target::from_link(link).unwrap()
    }

    fn item(pk: &str, taken_at: u64) -> Value {
        json!({
            "pk": pk, "media_type": 1, "product_type": "story", "taken_at": taken_at,
            "image_versions2": {"candidates": [
                {"url": format!("https://scontent.cdninstagram.com/{pk}-small.jpg"), "width": 320, "height": 568},
                {"url": format!("https://scontent.cdninstagram.com/{pk}.jpg"), "width": 1080, "height": 1920}
            ]},
            "original_width": 1080, "original_height": 1920
        })
    }

    fn script(key: &str, value: &Value) -> String {
        format!(
            r#"<html><script type="application/json" data-sjs>{{"require":[["x",{{"{key}":{value}}}]]}}</script></html>"#
        )
    }

    #[test]
    fn an_accounts_stories_from_a_logged_in_page() {
        let reel = json!({"reels_media": [{
            "id": "528817151", "reel_type": "user_reel",
            "user": {"username": "nasa", "full_name": "NASA", "profile_pic_url": "https://scontent.cdninstagram.com/p.jpg",
                     "hd_profile_pic_url_info": {"url": "https://scontent.cdninstagram.com/hd.jpg"}},
            "items": [item("1", 1_794_800_000), item("2", 1_794_826_807)]
        }]});
        let target = target("https://www.instagram.com/stories/nasa/");
        let post = reel_page(&script("xdt_api__v1__feed__reels_media", &reel), &target).unwrap();
        let other = Target::from_link("https://www.instagram.com/stories/esa/").unwrap();
        let error = reel_page(&script("xdt_api__v1__feed__reels_media", &reel), &other).unwrap_err();
        assert_eq!(error.stage, Stage::NotFound, "another account's stories");
        assert_eq!((post.page_type, post.url.as_str()), (PageType::Story, "https://www.instagram.com/stories/nasa/"));
        assert_eq!((post.author.as_deref(), post.profile.handle.as_deref()), (Some("NASA"), Some("nasa")));
        assert_eq!(
            (post.profile.avatar.as_deref(), post.profile.picture.as_deref()),
            (Some("https://scontent.cdninstagram.com/p.jpg"), Some("https://scontent.cdninstagram.com/hd.jpg")),
            "the small picture for cards, the full-size one for Instant View"
        );
        assert_eq!(post.published_at, Some(1_794_826_807), "the newest story");
        assert_eq!(post.media.len(), 2);
        assert_eq!(post.media[0].url, "https://scontent.cdninstagram.com/1.jpg", "the biggest picture");

        let none = json!({"reels_media": [], "unviewable_authors_infos": []});
        let error = reel_page(&script("xdt_api__v1__feed__reels_media", &none), &target).unwrap_err();
        assert_eq!(error.stage, Stage::NotFound, "no stories up now");
        let bare = reel_page("<html>Log in</html>", &target).unwrap_err();
        assert_eq!(bare.stage, Stage::NotFound, "not taken for dead cookies: a private account's page lacks them too");
    }

    #[test]
    fn a_story_from_the_logged_in_api_numbers_its_id() {
        let mut story = item("1", 1_794_800_000);
        story["pk"] = json!(3_412_345_678_901_234_567_u64);
        story["user"] = json!({"username": "nasa"});
        let target = target("https://www.instagram.com/stories/nasa/3412345678901234567/");
        let post = api_post(&json!({"items": [story]}), &target).unwrap();
        assert_eq!(post.url, "https://www.instagram.com/stories/nasa/3412345678901234567/");
    }

    #[test]
    fn a_highlight_from_a_logged_in_page() {
        let connection = json!({"edges": [{"node": {
            "id": "highlight:18003835801720483", "reel_type": "highlight_reel", "title": "Moon",
            "user": {"username": "nasa"}, "items": [item("1", 1_574_681_079), item("2", 1_576_225_207)]
        }}]});
        let target = target("https://www.instagram.com/stories/highlights/18003835801720483/");
        let post = reel_page(&script("xdt_api__v1__feed__reels_media__connection", &connection), &target).unwrap();
        assert_eq!(post.url, "https://www.instagram.com/stories/highlights/18003835801720483/");
        assert_eq!((post.group.as_deref(), post.media.len(), post.published_at), (Some("Moon"), 2, None));
    }

    #[test]
    fn crawler_tags_for_highlights_and_gated_posts() {
        let page = |tags: &[(&'static str, &str)]| Page {
            scripts: Vec::new(),
            meta: tags.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
        };
        let highlight = page(&[
            ("og:title", "Moon = @nasa"),
            ("og:image", "https://scontent.cdninstagram.com/cover.jpg"),
            ("og:url", "https://www.instagram.com/stories/highlights/1/"),
        ]);
        let post = highlight_tags(&highlight, &target("https://www.instagram.com/stories/highlights/1/")).unwrap();
        assert_eq!((post.group.as_deref(), post.profile.handle.as_deref()), (Some("Moon"), Some("nasa")));
        let elsewhere = page(&[("og:title", "Moon = @nasa"), ("og:image", "https://example.com/cover.jpg")]);
        assert!(highlight_tags(&elsewhere, &target("https://www.instagram.com/stories/highlights/1/")).is_err());

        // A post Instagram won't show logged out still has its tags.
        let gated = page(&[
            ("og:title", "Some One on Instagram: \"Moving day\""),
            ("og:description", "30K likes, 1,129 comments - someone on April 17, 2023: \"Moving day\". "),
            ("og:image", "https://scontent.cdninstagram.com/a.jpg"),
            ("og:url", "https://www.instagram.com/someone/p/CrKmzZzJjQB/"),
            ("twitter:title", "Some One (@someone) • Instagram photos and videos"),
        ]);
        let post = crawler_post(&gated, &target("https://www.instagram.com/p/CrKmzZzJjQB/")).unwrap();
        assert_eq!((post.author.as_deref(), post.text.as_deref()), (Some("Some One"), Some("Moving day")));
        assert_eq!((post.stats.reactions, post.stats.comments), (Some(30_000), Some(1_129)));
        assert_eq!(post.published_at, Some(1_681_795_613), "from the shortcode");
    }

    #[test]
    fn query_errors_say_what_happened() {
        let error =
            |code: i64, message: &str| query_error(&json!({"errors": [{"code": code, "message": message}]})).unwrap();
        assert_eq!(error(1_675_004, "x").stage, Stage::RateLimited);
        assert_eq!(error(1, "Please wait a few minutes").stage, Stage::RateLimited);
        assert_eq!(error(1_675_030, "field_exception").stage, Stage::NotFound);
        assert_eq!(error(2, "other").stage, Stage::Fetch);
        assert!(query_error(&json!({"data": {}})).is_none());
    }
}
