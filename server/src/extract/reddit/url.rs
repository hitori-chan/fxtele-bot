//! Reddit URLs: mapping request paths onto Reddit, page types, and what a link names. The bot
//! swaps whole hosts, so `redd.it/{post}`, `v.redd.it/{video}` and `i.redd.it/{file}` all arrive
//! as a bare `/{something}`, told apart by their shape.

use url::Url;

use crate::extract::{PageType, Source, Target};

pub const ORIGIN: &str = "https://www.reddit.com";

/// Single path segments that are Reddit's own pages, not a short link's post or video ID.
const SECTIONS: [&str; 24] = [
    "all",
    "answers",
    "best",
    "chat",
    "coins",
    "comments",
    "controversial",
    "explore",
    "gallery",
    "help",
    "hot",
    "login",
    "media",
    "message",
    "new",
    "notifications",
    "policies",
    "popular",
    "premium",
    "prefs",
    "rising",
    "search",
    "settings",
    "submit",
];

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A post, by its base-36 ID.
    Post(String),
    /// One comment under a post: the post's ID, the comment's.
    Comment(String, String),
    /// A share link (`/r/{sub}/s/{code}`), followed once to the post it names.
    Share,
    /// A `v.redd.it` video, which leads to its post.
    Video(String),
    /// An `i.redd.it` picture on its own.
    Image(String),
    /// A user's profile, by name in lower case.
    User(String),
    /// A community (subreddit), by name in lower case.
    Community(String),
    Other,
}

/// Maps a request's path and query onto Reddit. Because the origins are fixed here, the result
/// can only ever point at Reddit.
pub fn request(path_and_query: &str) -> Option<Target> {
    if !path_and_query.starts_with('/') {
        return None;
    }
    let mut asked = Url::parse(&format!("{ORIGIN}{path_and_query}")).ok()?;
    asked.set_fragment(None);
    if asked.host_str() != Some("www.reddit.com") {
        return None;
    }
    let link = read(&asked);
    let at = |url: String| Url::parse(&url).ok();
    let (original, url, page_type) = match &link {
        Link::Post(id) => {
            // A short link's `/{id}` is no page on www.reddit.com.
            let key = at(format!("{ORIGIN}/comments/{id}/"))?;
            let original = if is_bare(&asked) { key.clone() } else { asked };
            (original, key, PageType::Post)
        }
        Link::Comment(post, comment) => {
            (asked, at(format!("{ORIGIN}/comments/{post}/comment/{comment}/"))?, PageType::Post)
        }
        Link::Share => {
            let mut key = asked.clone();
            key.set_query(None);
            (asked, key, PageType::Post)
        }
        Link::Video(id) => {
            let url = at(format!("https://v.redd.it/{id}"))?;
            (url.clone(), url, PageType::Post)
        }
        Link::Image(file) => {
            let url = at(format!("https://i.redd.it/{file}"))?;
            (url.clone(), url, PageType::Photo)
        }
        Link::User(name) => (asked, at(format!("{ORIGIN}/user/{name}/"))?, PageType::Profile),
        Link::Community(name) => (asked, at(format!("{ORIGIN}/r/{name}/"))?, PageType::Profile),
        Link::Other => {
            let mut key = asked.clone();
            key.set_query(None);
            (asked, key, PageType::Other)
        }
    };
    Some(Target { source: Source::Reddit, original, url, page_type })
}

/// What a target's key names.
pub fn link(target: &Target) -> Link {
    match target.url.host_str() {
        Some("v.redd.it") | Some("i.redd.it") => {
            read(&Url::parse(&format!("{ORIGIN}{}", target.url.path())).expect("a path on the origin parses"))
        }
        _ => read(&target.url),
    }
}

/// A path that's one segment: a short link's.
fn is_bare(url: &Url) -> bool {
    url.path().trim_matches('/').split('/').count() == 1
}

fn read(url: &Url) -> Link {
    let parts: Vec<&str> = url.path().split('/').filter(|p| !p.is_empty()).collect();
    // A post's data link (`/comments/{id}.json`) names the post too.
    let id = |s: &str| {
        let s = s.strip_suffix(".json").unwrap_or(s);
        is_id(s).then(|| s.to_ascii_lowercase())
    };
    // `/comments/{id}/...`, or after `/r/{sub}` or `/user/{name}`; a comment's ID follows the
    // title's slug, or `comment` in newer links.
    let comments = match parts.as_slice() {
        ["comments", ..] => Some(0),
        ["r" | "u" | "user", _, "comments", ..] => Some(2),
        _ => None,
    };
    if let Some(at) = comments
        && let Some(post) = parts.get(at + 1).and_then(|p| id(p))
    {
        return match parts.get(at + 3).and_then(|c| id(c)) {
            Some(comment) => Link::Comment(post, comment),
            None => Link::Post(post),
        };
    }
    match parts.as_slice() {
        ["gallery", post, ..] if let Some(post) = id(post) => Link::Post(post),
        ["r" | "u" | "user", _, "s", code, ..] if is_code(code) => Link::Share,
        ["u" | "user", name, ..] if is_name(name) => Link::User(name.to_ascii_lowercase()),
        ["r", name, ..] if is_name(name) => Link::Community(name.to_ascii_lowercase()),
        ["media"] => media_param(url),
        [file] if let Some(file) = image_file(file) => Link::Image(file),
        [one] if !SECTIONS.contains(one) && is_id(one) && one.bytes().any(|b| b.is_ascii_digit()) => {
            let one = one.to_ascii_lowercase();
            if one.len() >= 10 { Link::Video(one) } else { Link::Post(one) }
        }
        _ => Link::Other,
    }
}

/// `/media?url=https://i.redd.it/{file}` (or `preview.redd.it`): Reddit's page around one
/// picture.
fn media_param(url: &Url) -> Link {
    let Some((_, inner)) = url.query_pairs().find(|(k, _)| k == "url") else { return Link::Other };
    match Url::parse(&inner) {
        Ok(inner) if matches!(inner.host_str(), Some("i.redd.it" | "preview.redd.it")) => {
            image_file(inner.path().trim_start_matches('/')).map_or(Link::Other, Link::Image)
        }
        _ => Link::Other,
    }
}

/// The picture's file on `i.redd.it`: its name there, or from a `preview.redd.it` name with the
/// post's title in front (`some-title-v0-{id}.png`), whose `i.redd.it` copy needs no signature.
fn image_file(file: &str) -> Option<String> {
    if is_image(file) {
        return Some(file.to_string());
    }
    let (name, extension) = file.rsplit_once('.')?;
    let (_, id) = name.rsplit_once("-v0-")?;
    let file = format!("{id}.{extension}");
    is_image(&file).then_some(file)
}

/// A base-36 ID: a post's, a comment's or a video's.
fn is_id(s: &str) -> bool {
    (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A user's or community's name.
pub(super) fn is_name(s: &str) -> bool {
    (2..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A share link's code.
fn is_code(s: &str) -> bool {
    (4..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A picture's file name on `i.redd.it`.
fn is_image(file: &str) -> bool {
    let Some((name, extension)) = file.rsplit_once('.') else { return false };
    is_id(name) && matches!(extension.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(link: &str) -> (String, String, PageType) {
        let t = Target::from_link(link).unwrap();
        (t.url.to_string(), t.original.to_string(), t.page_type)
    }

    #[test]
    fn posts_and_comments() {
        let post = "https://www.reddit.com/comments/1abc2d3/";
        for link in [
            "https://www.reddit.com/r/Rust/comments/1ABC2D3/some_title/?utm_source=share",
            "https://old.reddit.com/r/rust/comments/1abc2d3/",
            "https://reddit.com/comments/1abc2d3",
            "https://www.reddit.com/user/Someone/comments/1abc2d3/hi/",
            "https://www.reddit.com/gallery/1abc2d3",
        ] {
            assert_eq!(key(link).0, post, "{link}");
        }
        let short = key("https://redd.it/1abc2d3");
        assert_eq!((short.0.as_str(), short.1.as_str()), (post, post), "people go to the post, not /1abc2d3");
        let comment = "https://www.reddit.com/comments/1abc2d3/comment/kx9y8z7/";
        assert_eq!(key("https://www.reddit.com/r/rust/comments/1abc2d3/title/kx9y8z7/").0, comment);
        assert_eq!(key("https://www.reddit.com/r/rust/comments/1abc2d3/comment/kx9y8z7/?context=3").0, comment);
        let round_trip = Target::from_link(comment).unwrap();
        assert_eq!(round_trip.url.as_str(), comment, "a key reads back as itself");
    }

    #[test]
    fn short_hosts_by_shape() {
        let video = key("https://v.redd.it/dhulnxj8u2ec1");
        assert_eq!((video.0.as_str(), video.2), ("https://v.redd.it/dhulnxj8u2ec1", PageType::Post));
        let target = Target::from_link("https://v.redd.it/dhulnxj8u2ec1").unwrap();
        assert_eq!(link(&target), Link::Video("dhulnxj8u2ec1".into()));
        let image = key("https://i.redd.it/abc123xyz.jpeg");
        assert_eq!((image.0.as_str(), image.2), ("https://i.redd.it/abc123xyz.jpeg", PageType::Photo));
        assert_eq!(
            key("https://www.reddit.com/media?url=https%3A%2F%2Fi.redd.it%2Fabc123xyz.jpeg").0,
            "https://i.redd.it/abc123xyz.jpeg"
        );
        let preview = "https://preview.redd.it/some-title-here-v0-rfihlq0jxgla1.png?width=640&s=abc";
        assert_eq!(key(preview).0, "https://i.redd.it/rfihlq0jxgla1.png", "the copy that needs no signature");
        let around =
            "https://www.reddit.com/media?url=https%3A%2F%2Fpreview.redd.it%2Fa-b-v0-rfihlq0jxgla1.png%3Fwidth%3D640";
        assert_eq!(key(around).0, "https://i.redd.it/rfihlq0jxgla1.png");
        assert_eq!(
            key("https://www.reddit.com/r/rust/comments/1abc2d3.json").0,
            "https://www.reddit.com/comments/1abc2d3/"
        );
        assert_eq!(key("https://www.reddit.com/popular/").2, PageType::Other, "sections aren't IDs");
        assert_eq!(key("https://www.reddit.com/controversial").2, PageType::Other);
    }

    #[test]
    fn people_communities_and_shares() {
        assert_eq!(key("https://www.reddit.com/u/Some_One").0, "https://www.reddit.com/user/some_one/");
        assert_eq!(key("https://www.reddit.com/user/Some_One/submitted/").2, PageType::Profile);
        assert_eq!(key("https://www.reddit.com/r/Rust/top/?t=week").0, "https://www.reddit.com/r/rust/");
        let share = key("https://www.reddit.com/r/rust/s/AbCdEf123?utm=x");
        assert_eq!((share.0.as_str(), share.2), ("https://www.reddit.com/r/rust/s/AbCdEf123", PageType::Post));
        let target = Target::from_link("https://www.reddit.com/r/rust/s/AbCdEf123").unwrap();
        assert_eq!(link(&target), Link::Share);
    }
}
