//! Facebook URLs: mapping request paths, normalizing, page types, IDs and story tokens.

use url::Url;

use crate::extract::{PageType, Source, Target};

pub const ORIGIN: &str = "https://www.facebook.com";

/// Query parameters that identify a post; everything else is tracking.
const KEPT_PARAMS: [&str; 5] = ["story_fbid", "id", "fbid", "v", "set"];

/// Maps a request's path and query (`/reel/1?s=x`) onto www.facebook.com. Because the origin is
/// fixed here, the result can only ever point at Facebook.
pub fn request(path_and_query: &str) -> Option<Target> {
    if !path_and_query.starts_with('/') {
        return None;
    }
    let mut original = Url::parse(&format!("{ORIGIN}{path_and_query}")).ok()?;
    original.set_fragment(None);
    if original.host_str() != Some("www.facebook.com") {
        return None;
    }
    Some(target(original))
}

/// Any facebook.com URL (for example a redirect target); the host becomes www.facebook.com.
pub fn target(original: Url) -> Target {
    let url = normalize(&original);
    let page_type = page_type(&url);
    Target { source: Source::Facebook, original, url, page_type }
}

/// Facebook's reading of a target's link.
impl Target {
    fn path_parts(&self) -> Vec<&str> {
        segments(&self.url)
    }

    fn param(&self, name: &str) -> Option<String> {
        query_param(&self.url, name)
    }

    /// The media ID a reel, video, photo, watch or story-card page must match.
    pub fn media_id(&self) -> Option<String> {
        let parts = self.path_parts();
        match self.page_type {
            PageType::Reel | PageType::Video => parts.last().map(ToString::to_string),
            PageType::Photo => self.param("fbid").or_else(|| parts.last().map(ToString::to_string)),
            PageType::Watch => self.param("v"),
            PageType::Story => parts.get(2).map(ToString::to_string),
            PageType::Profile | PageType::Post | PageType::Album | PageType::Other => None,
        }
    }

    /// The bucket (whose stories) of a story card: `/stories/{bucket}/{card}`.
    pub fn story_bucket(&self) -> Option<String> {
        (self.page_type == PageType::Story).then(|| self.path_parts().get(1).map(ToString::to_string)).flatten()
    }

    /// The album token of an album page: `set=a.…`.
    pub fn album_set(&self) -> Option<String> {
        (self.page_type == PageType::Album).then(|| self.param("set")).flatten()
    }

    /// The story token in the URL: `story_fbid`, or the part after `permalink` or `posts`.
    pub fn story_token(&self) -> Option<String> {
        story_token_of(&self.url)
    }

    /// Share links can be scoped by story tokens from the page's route data.
    pub fn allows_route_tokens(&self) -> bool {
        matches!(self.path_parts().as_slice(), ["share", _, ..])
    }
}

/// Login, checkpoint and two-step pages mean the request needs (working) cookies.
pub fn is_login_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains("/login") || path.contains("/checkpoint") || path.contains("/two_step_verification")
}

/// Share links redirect through `story.php` or `permalink.php?story_fbid=..&id=..` before
/// Facebook puts them behind a login; that pair names the post at `/{id}/posts/{story_fbid}`.
pub fn share_redirect_target(url: &Url) -> Option<Url> {
    let last = url.path().trim_end_matches('/').rsplit('/').next()?;
    if !matches!(last, "story.php" | "permalink.php") {
        return None;
    }
    let story = query_param(url, "story_fbid").filter(|s| !s.is_empty())?;
    let id = query_param(url, "id").filter(|s| !s.is_empty())?;
    let mut target = Url::parse(ORIGIN).ok()?;
    target.path_segments_mut().ok()?.extend([id.as_str(), "posts", story.as_str()]);
    Some(target)
}

fn story_token_of(url: &Url) -> Option<String> {
    if let Some(token) = query_param(url, "story_fbid").filter(|t| !t.is_empty()) {
        return Some(token);
    }
    match segments(url).as_slice() {
        [.., "permalink", token] => Some((*token).to_string()),
        [_, .., "posts", token] => Some((*token).to_string()),
        _ => None,
    }
}

/// The path's non-empty segments.
fn segments(url: &Url) -> Vec<&str> {
    url.path().split('/').filter(|p| !p.is_empty()).collect()
}

/// Copies the identifying query parameters of `url` onto `out`.
fn keep_params(url: &Url, mut out: Url) -> Url {
    let mut kept = url.query_pairs().filter(|(k, _)| KEPT_PARAMS.contains(&k.as_ref())).peekable();
    if kept.peek().is_some() {
        out.query_pairs_mut().extend_pairs(kept);
    }
    out
}

fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// The identifying query parameters only and no fragment; scheme, host and path stay.
/// Used for redirects, where Facebook picks the host.
pub fn clean(url: &Url) -> Url {
    let mut out = url.clone();
    out.set_fragment(None);
    out.set_query(None);
    keep_params(url, out)
}

/// www.facebook.com, the identifying query parameters only, no fragment, no trailing slash.
fn normalize(url: &Url) -> Url {
    let mut out = Url::parse(ORIGIN).expect("valid origin");
    let path = url.path();
    let trimmed = path.trim_end_matches('/');
    // Both name the same photo page; one cache entry for both.
    let trimmed = if trimmed == "/photo.php" { "/photo" } else { trimmed };
    out.set_path(if trimmed.is_empty() { "/" } else { trimmed });
    keep_params(url, out)
}

/// First path segments that are Facebook sections, not usernames.
const SECTIONS: [&str; 14] = [
    "events",
    "gaming",
    "groups",
    "hashtag",
    "marketplace",
    "media",
    "pages",
    "people",
    "photo",
    "reel",
    "share",
    "stories",
    "watch",
    "videos",
];

fn page_type(url: &Url) -> PageType {
    let parts = segments(url);
    let digits = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let has = |name: &str| query_param(url, name).is_some_and(|v| !v.is_empty());
    match parts.as_slice() {
        // Facebook's own login pages, whatever follows: fetched, with cookies, they'd be taken for
        // a refusal of them, and anyone can post such a link.
        _ if is_login_url(url) => PageType::Other,
        [.., "reel", id] if digits(id) => PageType::Reel,
        [.., "videos", id] | [.., "videos", _, id] if digits(id) => PageType::Video,
        [.., "photo" | "photo.php"] if has("fbid") => PageType::Photo,
        // `/{page}/photos/a.{album}/{photo}/` and the older `/{page}/photos/{photo}/`.
        [_, "photos", _, id] | [_, "photos", id] if digits(id) => PageType::Photo,
        ["watch"] | ["watch", "live"] | ["video.php"] if has("v") => PageType::Watch,
        ["media", "set"] if has("set") => PageType::Album,
        ["stories", _, _, ..] => PageType::Story,
        ["profile.php"] if has("id") => PageType::Profile,
        ["people", _, _] => PageType::Profile,
        // Usernames may have dots (`/Some.Page.II`), but `.php` is a Facebook script.
        [name] if !name.ends_with(".php") && !SECTIONS.contains(name) => PageType::Profile,
        ["share", _, ..] => PageType::Post,
        _ if story_token_of(url).is_some() => PageType::Post,
        _ => PageType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> Target {
        request(path).unwrap()
    }

    #[test]
    fn maps_requests_onto_facebook() {
        let t = target("/reel/123/?s=abc&mibextid=x#frag");
        assert_eq!(t.original.as_str(), "https://www.facebook.com/reel/123/?s=abc&mibextid=x");
        assert_eq!(t.key(), "https://www.facebook.com/reel/123");
        assert_eq!(t.page_type, PageType::Reel);
        assert_eq!(t.media_id().as_deref(), Some("123"));
    }

    #[test]
    fn redirects_can_only_point_at_facebook() {
        for path in
            ["//evil.example/x", "/\\evil.example", "/@evil.example", "/..//evil.example", "/%2F%2Fevil.example"]
        {
            if let Some(t) = request(path) {
                assert_eq!(t.original.host_str(), Some("www.facebook.com"), "{path}");
            }
        }
        assert!(request("evil.example").is_none());
    }

    #[test]
    fn keeps_identifying_params_only() {
        let t = target("/permalink.php?story_fbid=pfbid0abc&id=100&rdid=x&share_url=y");
        assert_eq!(t.key(), "https://www.facebook.com/permalink.php?story_fbid=pfbid0abc&id=100");
        assert_eq!(t.page_type, PageType::Post);
        assert_eq!(t.story_token().as_deref(), Some("pfbid0abc"));
        assert_eq!(target("/watch/?v=42&t=1").key(), "https://www.facebook.com/watch?v=42");
        assert_eq!(target("/photo.php?fbid=1&set=a.2&type=3").key(), target("/photo/?fbid=1&set=a.2").key());
        assert_eq!(target("/stories/100/UzpfSTE/").story_bucket().as_deref(), Some("100"));
        assert_eq!(target("/media/set/?set=a.16&type=3").album_set().as_deref(), Some("a.16"));
    }

    #[test]
    fn page_types_and_ids() {
        let cases = [
            ("/reel/1", PageType::Reel, Some("1")),
            ("/somepage/videos/2", PageType::Video, Some("2")),
            ("/somepage/videos/some-slug/3", PageType::Video, Some("3")),
            ("/photo/?fbid=4&set=a.1", PageType::Photo, Some("4")),
            ("/photo.php?fbid=5", PageType::Photo, Some("5")),
            ("/somepage/photo.php?fbid=6", PageType::Photo, Some("6")),
            ("/somepage/photos/a.10/11/", PageType::Photo, Some("11")),
            ("/somepage/photos/12", PageType::Photo, Some("12")),
            ("/watch?v=7", PageType::Watch, Some("7")),
            ("/watch/live/?v=13", PageType::Watch, Some("13")),
            ("/video.php?v=14", PageType::Watch, Some("14")),
            ("/stories/100/UzpfSTE/", PageType::Story, Some("UzpfSTE")),
            ("/somepage", PageType::Profile, None),
            ("/Some.Page.II", PageType::Profile, None),
            ("/profile.php?id=8", PageType::Profile, None),
            ("/people/Some-Name/100075468038908/", PageType::Profile, None),
            ("/people/Some-Name/pfbid0Sc36v/", PageType::Profile, None),
            ("/somepage/posts/pfbid02x", PageType::Post, None),
            ("/groups/9/permalink/10/", PageType::Post, None),
            ("/groups/9/posts/10/", PageType::Post, None),
            ("/permalink.php?story_fbid=pfbid0a&id=1", PageType::Post, None),
            ("/story.php?story_fbid=15&id=1", PageType::Post, None),
            ("/share/p/AbC/", PageType::Post, None),
            ("/share/r/AbC/", PageType::Post, None),
            ("/media/set/?set=a.16", PageType::Album, None),
            ("/events/17/", PageType::Other, None),
            ("/login/posts/1", PageType::Other, None),
            ("/checkpoint/permalink/1", PageType::Other, None),
            ("/groups/18", PageType::Other, None),
            ("/marketplace/item/19/", PageType::Other, None),
            ("/somepage/about", PageType::Other, None),
            ("/watch", PageType::Other, None),
            ("/photo", PageType::Other, None),
            ("/profile.php", PageType::Other, None),
            ("/stories/100", PageType::Other, None),
            ("/", PageType::Other, None),
        ];
        for (path, page_type, id) in cases {
            let t = target(path);
            assert_eq!(t.page_type, page_type, "{path}");
            assert_eq!(t.media_id().as_deref(), id, "{path}");
        }
    }

    #[test]
    fn story_tokens() {
        assert_eq!(target("/somepage/posts/pfbid02x").story_token().as_deref(), Some("pfbid02x"));
        assert_eq!(target("/groups/9/permalink/10/").story_token().as_deref(), Some("10"));
        assert_eq!(target("/posts/1").story_token(), None);
        assert_eq!(target("/share/p/AbC/").story_token(), None);
        assert!(target("/share/p/AbC/").allows_route_tokens());
        assert!(!target("/somepage/posts/1").allows_route_tokens());
    }

    #[test]
    fn links_must_be_facebook() {
        assert!(Target::from_link("https://m.facebook.com/reel/1").is_some());
        assert!(Target::from_link("http://facebook.com/reel/1").is_some());
        assert!(Target::from_link("https://facebook.com.evil.example/reel/1").is_none());
        assert!(Target::from_link("https://notfacebook.com/reel/1").is_none());
        assert!(Target::from_link("ftp://facebook.com/reel/1").is_none());
        assert_eq!(Target::from_link("https://web.facebook.com/reel/1?a=b").unwrap().key(), target("/reel/1").key());
    }

    #[test]
    fn login_and_share_redirects() {
        assert!(is_login_url(&Url::parse("https://www.facebook.com/login/?next=x").unwrap()));
        assert!(is_login_url(&Url::parse("https://m.facebook.com/checkpoint/1").unwrap()));
        assert!(!is_login_url(&Url::parse("https://www.facebook.com/reel/1").unwrap()));

        let story = Url::parse("https://m.facebook.com/story.php?story_fbid=pfbid0x&id=100&x=1").unwrap();
        assert_eq!(share_redirect_target(&story).unwrap().as_str(), "https://www.facebook.com/100/posts/pfbid0x");
        let missing = Url::parse("https://m.facebook.com/permalink.php?story_fbid=pfbid0x").unwrap();
        assert!(share_redirect_target(&missing).is_none());
    }
}
