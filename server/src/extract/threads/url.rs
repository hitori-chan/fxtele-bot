//! Threads URLs: mapping request paths onto www.threads.com, and what a link names. Threads moved
//! from threads.net, whose links redirect there; a post is found by its code alone, as Threads
//! redirects a link with someone else's name in it to the right one.

use url::Url;

use crate::extract::{PageType, Source, Target};

pub const ORIGIN: &str = "https://www.threads.com";

/// What a link names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link<'a> {
    /// A post, by its code.
    Post(&'a str),
    /// An account, by its username in lower case.
    Profile(&'a str),
    /// A share link (`/share/{code}`), which redirects to the post.
    Share,
}

/// Maps a request's path and query (`/@zuck/post/Abc?xmt=x`) onto www.threads.com. Because the
/// origin is fixed here, the result can only ever point at Threads. Only posts and profiles are
/// read; any other page (search, feeds) is left to Threads.
pub fn request(path_and_query: &str) -> Option<Target> {
    if !path_and_query.starts_with('/') {
        return None;
    }
    let mut original = Url::parse(&format!("{ORIGIN}{path_and_query}")).ok()?;
    original.set_fragment(None);
    if original.host_str() != Some("www.threads.com") {
        return None;
    }
    let parts: Vec<&str> = original.path().split('/').filter(|p| !p.is_empty()).collect();
    let (path, page_type) = match parts.as_slice() {
        // After the code: `/media`, which opens its pictures full screen, or a slug of its text.
        [user, "post", code, ..] => (format!("/@{}/post/{}", username(user)?, code_of(code)?), PageType::Post),
        ["t", code] => (format!("/t/{}", code_of(code)?), PageType::Post),
        ["share", code] => (format!("/share/{}", code_of(code)?), PageType::Post),
        // Its tabs: threads, replies, media and reposts.
        [user] | [user, "replies" | "media" | "reposts"] => (format!("/@{}", username(user)?), PageType::Profile),
        _ => return None,
    };
    let mut url = Url::parse(ORIGIN).expect("valid origin");
    url.set_path(&path);
    Some(Target { source: Source::Threads, original, url, page_type })
}

/// What a target names, from its normalized URL.
pub fn link(target: &Target) -> Link<'_> {
    let path = target.url.path();
    if let Some(rest) = path.strip_prefix("/t/") {
        return Link::Post(rest);
    }
    if path.starts_with("/share/") {
        return Link::Share;
    }
    let rest = path.strip_prefix("/@").unwrap_or_default();
    match rest.split_once("/post/") {
        Some((_, code)) => Link::Post(code),
        None => Link::Profile(rest),
    }
}

/// A username from `@name` (or `%40name`), in lower case, as Threads matches them.
fn username(part: &str) -> Option<String> {
    let name = part.strip_prefix('@').or_else(|| part.strip_prefix("%40"))?;
    let valid = (1..=30).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
        && !name.starts_with('.');
    valid.then(|| name.to_ascii_lowercase())
}

/// A post's code: Instagram's base-64 letters, case kept.
fn code_of(part: &str) -> Option<&str> {
    let valid =
        (5..=64).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    valid.then_some(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> Option<(String, PageType)> {
        request(path).map(|t| (t.url.to_string(), t.page_type))
    }

    #[test]
    fn posts_are_keyed_without_their_query() {
        let post = |url: &str| Some((url.to_string(), PageType::Post));
        assert_eq!(key("/@Zuck/post/DfaNgswkQPG?xmt=AQF0"), post("https://www.threads.com/@zuck/post/DfaNgswkQPG"));
        assert_eq!(key("/%40zuck/post/DfaNgswkQPG/media"), post("https://www.threads.com/@zuck/post/DfaNgswkQPG"));
        assert_eq!(
            key("/@zuck/post/DfaNgswkQPG/using-our-new-spaces"),
            post("https://www.threads.com/@zuck/post/DfaNgswkQPG")
        );
        assert_eq!(key("/t/DfaNgswkQPG"), post("https://www.threads.com/t/DfaNgswkQPG"));
        let target = request("/@zuck/post/DfaNgswkQPG?xmt=AQF0").unwrap();
        assert_eq!(target.original.as_str(), "https://www.threads.com/@zuck/post/DfaNgswkQPG?xmt=AQF0");
        assert_eq!(link(&target), Link::Post("DfaNgswkQPG"));
        assert_eq!(link(&request("/t/DfaNgswkQPG").unwrap()), Link::Post("DfaNgswkQPG"));
        let share = request("/share/AbCdEf123/").unwrap();
        assert_eq!((share.url.as_str(), link(&share)), ("https://www.threads.com/share/AbCdEf123", Link::Share));
    }

    #[test]
    fn profiles_and_what_isnt_read() {
        let profile = Some(("https://www.threads.com/@zuck".to_string(), PageType::Profile));
        assert_eq!(key("/@Zuck"), profile);
        assert_eq!(key("/@zuck/replies"), profile);
        assert_eq!(link(&request("/@zuck").unwrap()), Link::Profile("zuck"));
        for other in ["/", "/search?q=x", "/zuck", "/@zuck/post", "/@zuck/post/a!b", "/@.x", "//evil.example/x", "x"] {
            assert_eq!(key(other), None, "{other}");
        }
    }
}
