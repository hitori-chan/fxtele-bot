//! Instagram URLs: mapping request paths onto www.instagram.com, page types, shortcodes and media
//! IDs.

use url::Url;

use crate::extract::{PageType, Source, Target};

pub const ORIGIN: &str = "https://www.instagram.com";

/// First path segments that are Instagram's own sections, not usernames.
const SECTIONS: [&str; 18] = [
    "about",
    "accounts",
    "api",
    "challenge",
    "developer",
    "direct",
    "explore",
    "graphql",
    "legal",
    "p",
    "privacy",
    "reel",
    "reels",
    "s",
    "share",
    "stories",
    "tv",
    "web",
];

/// Maps a request's path and query (`/p/Abc?igsh=x`) onto www.instagram.com. Because the origin
/// is fixed here, the result can only ever point at Instagram.
pub fn request(path_and_query: &str) -> Option<Target> {
    if !path_and_query.starts_with('/') {
        return None;
    }
    let mut original = Url::parse(&format!("{ORIGIN}{path_and_query}")).ok()?;
    original.set_fragment(None);
    if original.host_str() != Some("www.instagram.com") {
        return None;
    }
    let (path, page_type) = key_path(&original);
    let mut url = Url::parse(ORIGIN).expect("valid origin");
    url.set_path(&path);
    Some(Target { source: Source::Instagram, original, url, page_type })
}

/// The cache key's path and the page type. A post is keyed by its shortcode, whichever path named
/// it (`/p/`, `/reel/`, `/tv/`, with or without the username first); a story by its owner and ID;
/// a profile by its username, which Instagram matches without case. Query strings (`igsh`,
/// `img_index`) never name a different post, so none is kept. Highlights are stories too, the
/// short `/s/` links to them included.
fn key_path(url: &Url) -> (String, PageType) {
    let parts: Vec<&str> = url.path().split('/').filter(|p| !p.is_empty()).collect();
    let post = |kind: &str, code: &str| {
        let page_type = if kind == "p" { PageType::Post } else { PageType::Reel };
        (format!("/p/{code}"), page_type)
    };
    let highlight = |id: &str| (format!("/stories/highlights/{id}"), PageType::Story);
    match parts.as_slice() {
        [kind, code, ..] if is_post_kind(kind) && is_shortcode(code) => post(kind, code),
        [user, kind, code, ..]
            if is_username(user) && !SECTIONS.contains(user) && is_post_kind(kind) && is_shortcode(code) =>
        {
            post(kind, code)
        }
        ["stories", "highlights", id, ..] if is_number(id) => highlight(id),
        ["stories", user, id, ..] if is_username(user) && is_number(id) => {
            (format!("/stories/{}/{id}", user.to_ascii_lowercase()), PageType::Story)
        }
        ["stories", user] if *user != "highlights" && is_username(user) => {
            (format!("/stories/{}", user.to_ascii_lowercase()), PageType::Story)
        }
        ["s", token] if let Some(id) = highlight_token(token) => highlight(&id),
        // Share links name a post through a token; the extractor follows them to it.
        ["share", rest @ ..] if !rest.is_empty() => {
            let page_type = if rest.first() == Some(&"reel") { PageType::Reel } else { PageType::Post };
            (format!("/share/{}", rest.join("/")), page_type)
        }
        [user] if is_username(user) && !SECTIONS.contains(user) => {
            (format!("/{}", user.to_ascii_lowercase()), PageType::Profile)
        }
        _ => (format!("/{}", parts.join("/")), PageType::Other),
    }
}

fn is_post_kind(kind: &str) -> bool {
    matches!(kind, "p" | "reel" | "reels" | "tv")
}

fn is_shortcode(code: &str) -> bool {
    (5..=64).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_username(name: &str) -> bool {
    (1..=30).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
}

fn is_number(id: &str) -> bool {
    (1..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// The highlight a short `/s/` link names: base64 of `highlight:<id>`.
fn highlight_token(token: &str) -> Option<String> {
    let token = token.trim_end_matches('=');
    let bytes = data_encoding::BASE64_NOPAD
        .decode(token.as_bytes())
        .or_else(|_| data_encoding::BASE64URL_NOPAD.decode(token.as_bytes()))
        .ok()?;
    let id = std::str::from_utf8(&bytes).ok()?.strip_prefix("highlight:")?;
    is_number(id).then(|| id.to_string())
}

/// What a link names, read from its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link<'a> {
    /// A post or reel, by shortcode.
    Post(&'a str),
    /// One story, by its owner and media ID.
    Story(&'a str, u64),
    /// Whatever stories an account has up now.
    Stories(&'a str),
    Highlight(u64),
    Profile(&'a str),
    /// A share link, which redirects to a post.
    Share,
    Other,
}

pub fn link(target: &Target) -> Link<'_> {
    let parts: Vec<&str> = target.url.path().split('/').filter(|p| !p.is_empty()).collect();
    match (target.page_type, parts.as_slice()) {
        (_, ["p", code]) => Link::Post(code),
        (_, ["stories", "highlights", id]) => id.parse().map_or(Link::Other, Link::Highlight),
        (_, ["stories", user, id]) => id.parse().map_or(Link::Other, |id| Link::Story(user, id)),
        (PageType::Story, ["stories", user]) => Link::Stories(user),
        (_, ["share", ..]) => Link::Share,
        (PageType::Profile, [user]) => Link::Profile(user),
        _ => Link::Other,
    }
}

const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The media ID a shortcode stands for: the shortcode is the ID in base 64, `A` to `_`. Longer
/// codes (private accounts' links) carry more than the ID and give `None`.
pub fn media_id(code: &str) -> Option<u64> {
    if code.len() > 11 {
        return None;
    }
    code.bytes().try_fold(0u64, |id, b| {
        let digit = DIGITS.iter().position(|d| *d == b)?;
        id.checked_mul(64)?.checked_add(u64::try_from(digit).ok()?)
    })
}

/// The shortcode of a media ID; a story's ID gives one the post query accepts.
pub fn shortcode_of(mut id: u64) -> String {
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[usize::try_from(id % 64).unwrap_or_default()]);
        id /= 64;
        if id == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// When a media ID was made (Unix seconds): its top bits are milliseconds since Instagram's
/// epoch, as with Twitter's IDs.
pub fn created_at(id: u64) -> i64 {
    const EPOCH_MS: u64 = 1_314_220_021_721;
    i64::try_from(((id >> 23) + EPOCH_MS) / 1000).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> Target {
        request(path).unwrap()
    }

    #[test]
    fn posts_are_keyed_by_shortcode() {
        let t = target("/reel/DfxQeYNtW-_/?igsh=abc&img_index=2#x");
        assert_eq!(t.original.as_str(), "https://www.instagram.com/reel/DfxQeYNtW-_/?igsh=abc&img_index=2");
        assert_eq!((t.key(), t.page_type), ("https://www.instagram.com/p/DfxQeYNtW-_", PageType::Reel));
        assert_eq!(link(&t), Link::Post("DfxQeYNtW-_"));
        assert_eq!(t.item_index(), Some(1));
        for path in ["/p/DfxQeYNtW-_", "/tv/DfxQeYNtW-_/", "/nasa/p/DfxQeYNtW-_/", "/reels/DfxQeYNtW-_"] {
            assert_eq!(target(path).key(), t.key(), "{path}");
        }
        assert_eq!(target("/p/DfSi56IFwZK/").page_type, PageType::Post);
    }

    #[test]
    fn stories_profiles_and_others() {
        let story = target("/stories/NASA/3700000000000000000/?utm_source=x");
        assert_eq!(story.key(), "https://www.instagram.com/stories/nasa/3700000000000000000");
        assert_eq!((story.page_type, link(&story)), (PageType::Story, Link::Story("nasa", 3_700_000_000_000_000_000)));
        let stories = target("/stories/NASA/");
        assert_eq!((stories.key(), link(&stories)), ("https://www.instagram.com/stories/nasa", Link::Stories("nasa")));
        let highlight = target("/stories/highlights/18003835801720483/");
        assert_eq!(link(&highlight), Link::Highlight(18_003_835_801_720_483));
        // The short link to the same highlight: base64 of `highlight:18003835801720483`.
        let short = target("/s/aGlnaGxpZ2h0OjE4MDAzODM1ODAxNzIwNDgz?story_media_id=1_2&igsh=x");
        assert_eq!((short.key(), short.page_type), (highlight.key(), PageType::Story));
        let profile = target("/NASA/?hl=en");
        assert_eq!((profile.key(), profile.page_type), ("https://www.instagram.com/nasa", PageType::Profile));
        assert_eq!(link(&profile), Link::Profile("nasa"));
        let share = target("/share/reel/BAabc/");
        assert_eq!((share.key(), share.page_type), ("https://www.instagram.com/share/reel/BAabc", PageType::Reel));
        assert_eq!(link(&share), Link::Share);
        for other in [
            "/explore/",
            "/stories/highlights/x/",
            "/s/bm90IGEgaGlnaGxpZ2h0",
            "/accounts/login/",
            "/",
            "/p/",
            "/a/b/c/d",
        ] {
            assert_eq!(target(other).page_type, PageType::Other, "{other}");
        }
    }

    #[test]
    fn redirects_can_only_point_at_instagram() {
        for path in
            ["//evil.example/x", "/\\evil.example", "/@evil.example", "/..//evil.example", "/%2F%2Fevil.example"]
        {
            if let Some(t) = request(path) {
                assert_eq!(t.original.host_str(), Some("www.instagram.com"), "{path}");
            }
        }
        assert!(request("evil.example").is_none());
    }

    #[test]
    fn links_from_any_instagram_host() {
        for link in [
            "https://instagram.com/p/DfSi56IFwZK",
            "http://m.instagram.com/p/DfSi56IFwZK/",
            "https://instagr.am/p/DfSi56IFwZK",
        ] {
            let t = Target::from_link(link).unwrap();
            assert_eq!((t.source, t.key()), (Source::Instagram, "https://www.instagram.com/p/DfSi56IFwZK"), "{link}");
        }
        assert!(Target::from_link("https://instagram.com.evil.example/p/DfSi56IFwZK").is_none());
    }

    #[test]
    fn shortcodes_and_media_ids() {
        // A carousel's shortcode and the `pk` Instagram gives for it.
        assert_eq!(media_id("DfSi56IFwZK"), Some(4_022_430_930_199_578_186));
        assert_eq!(shortcode_of(4_022_430_930_199_578_186), "DfSi56IFwZK");
        assert_eq!(media_id("B"), Some(1));
        assert_eq!(media_id("A!"), None);
        assert_eq!(media_id("DfSi56IFwZKextraextraextra"), None);
        assert_eq!(created_at(media_id("DfSi56IFwZK").unwrap()), 1_793_731_155);
    }
}
