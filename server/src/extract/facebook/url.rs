//! Facebook URLs: mapping request paths, normalizing, page types, IDs and story tokens.

use url::Url;

use crate::extract::{PageType, Source, Target};

pub const ORIGIN: &str = "https://www.facebook.com";

/// Facebook's short links for videos, which the bot sends as `/fb.watch/{code}`: on the preview
/// host, a bare `/{code}` would be a profile.
pub const SHORT_HOST: &str = "fb.watch";

/// Maps a request's path and query (`/reel/1?s=x`) onto www.facebook.com. Because the origin is
/// fixed here, the result can only ever point at Facebook.
pub fn request(path_and_query: &str) -> Option<Target> {
    if !path_and_query.starts_with('/') {
        return None;
    }
    if let Some(rest) = path_and_query.strip_prefix("/fb.watch/") {
        return short_link(rest);
    }
    let mut original = Url::parse(&format!("{ORIGIN}{path_and_query}")).ok()?;
    original.set_fragment(None);
    if original.host_str() != Some("www.facebook.com") {
        return None;
    }
    Some(target(original))
}

/// A `fb.watch/{code}` link, which redirects to the video. Its code is its key.
fn short_link(rest: &str) -> Option<Target> {
    let code = rest.split(['/', '?', '#']).next()?;
    let valid =
        (4..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let url = Url::parse(&format!("https://{SHORT_HOST}/{code}/")).ok().filter(|_| valid)?;
    Some(Target { source: Source::Facebook, original: url.clone(), url, page_type: PageType::Post })
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
            // A card's ID is base64 (`S:_ISC:…`), padded in the page, yet links often drop the
            // `=` (or encode it): without it, nothing in the page matches.
            PageType::Story => parts.get(2).map(|card| {
                let card = card.replace("%3D", "=").replace("%3d", "=");
                let bare = card.trim_end_matches('=');
                let padding = match bare.len() % 4 {
                    2 => "==",
                    3 => "=",
                    _ => "",
                };
                format!("{bare}{padding}")
            }),
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

    /// The comment a link opens on: a reply's own ID, else the comment's.
    pub fn comment_id(&self) -> Option<String> {
        ["reply_comment_id", "comment_id"]
            .into_iter()
            .find_map(|name| self.param(name).filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())))
    }

    /// Share links can be scoped by story tokens from the page's route data.
    pub fn allows_route_tokens(&self) -> bool {
        matches!(self.path_parts().as_slice(), ["share", _, ..])
    }
}

/// Login, checkpoint and two-step pages mean the request needs (working) cookies. By the first
/// part of the path only: a page named "LoginRadius" or "checkpointsoftware" isn't a login wall,
/// and taking it for one would mark working cookies rejected.
pub fn is_login_url(url: &Url) -> bool {
    let first = segments(url).first().map(|s| s.to_ascii_lowercase());
    matches!(first.as_deref(), Some("login" | "login.php" | "checkpoint" | "two_step_verification"))
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

/// A post's token, from the places that hold one: `story_fbid` on `permalink.php` and `story.php`,
/// or the path. Only there: on any other page a query parameter would make it a post, which may
/// be fetched with cookies.
fn story_token_of(url: &Url) -> Option<String> {
    let parts = segments(url);
    let param = match parts.as_slice() {
        [.., "permalink.php" | "story.php"] => query_param(url, "story_fbid"),
        _ => None,
    };
    if let Some(token) = param.filter(|t| !t.is_empty()) {
        return Some(token);
    }
    let token = |t: &&str| t.starts_with("pfbid") || t.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        [.., "permalink", token] => Some((*token).to_string()),
        [_, .., "posts", token] => Some((*token).to_string()),
        // `/{page}/posts/{slug}/{id}`: a slug of the text before the ID.
        [_, .., "posts", _, id] if token(id) => Some((*id).to_string()),
        _ => None,
    }
}

/// The path's non-empty segments.
fn segments(url: &Url) -> Vec<&str> {
    url.path().split('/').filter(|p| !p.is_empty()).collect()
}

/// Every query parameter that can identify a page.
const ID_PARAMS: [&str; 5] = ["story_fbid", "id", "fbid", "v", "set"];

/// The query parameters that identify what `url`'s page shows, by its path; everything else is
/// tracking, and would only split the cache key. A page its path names alone keeps none; one this
/// doesn't know keeps them all. A group's `multi_permalinks` isn't one: the path is rewritten to
/// the post it names.
fn identifying_params(url: &Url) -> &'static [&'static str] {
    match segments(url).as_slice() {
        [.., "permalink.php" | "story.php"] => &["story_fbid", "id"],
        ["profile.php"] => &["id"],
        [.., "photo" | "photo.php"] => &["fbid", "set"],
        ["watch"] | ["watch", "live"] | [.., "video.php"] => &["v"],
        ["media", "set"] => &["set"],
        [_, "posts" | "videos" | "photos" | "permalink", ..]
        | ["reel" | "share" | "stories", ..]
        | ["groups", _, "posts" | "permalink", ..] => &[],
        [name] if !name.ends_with(".php") => &[],
        _ => &ID_PARAMS,
    }
}

/// Copies the identifying query parameters of `url` onto `out`: its page's, and the comment a link
/// opens on.
fn keep_params(url: &Url, mut out: Url) -> Url {
    let digits = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    let own = identifying_params(url);
    let mut kept = url
        .query_pairs()
        .filter(|(k, v)| match k.as_ref() {
            // Read only when they're IDs; anything else would only split the cache key.
            "comment_id" | "reply_comment_id" => digits(v),
            k => own.contains(&k),
        })
        .peekable();
    if kept.peek().is_some() {
        out.query_pairs_mut().extend_pairs(kept);
    }
    out
}

fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// The identifying query parameters only and no fragment; scheme, host and path stay, but for a
/// group's page opened on a post, which becomes the post's. Used for redirects, where Facebook
/// picks the host.
pub fn clean(url: &Url) -> Url {
    let mut out = url.clone();
    out.set_fragment(None);
    out.set_query(None);
    if let Some(path) = group_post_path(url) {
        out.set_path(&path);
    }
    keep_params(url, out)
}

/// [`clean`] without the group rewrite, `multi_permalinks` kept: the page exactly as Facebook sent
/// it, for when the post's own page redirected there.
pub fn clean_as_sent(url: &Url) -> Url {
    let mut out = url.clone();
    out.set_fragment(None);
    out.set_query(None);
    let mut out = keep_params(url, out);
    if let Some(post) = query_param(url, "multi_permalinks") {
        out.query_pairs_mut().append_pair("multi_permalinks", &post);
    }
    out
}

/// A group's page opened on a post (`/groups/{g}/?multi_permalinks={id}`) is that post's own
/// page, `/groups/{g}/posts/{id}`: half the size, and one key with it. The cookies' rule then
/// sees the page that's actually fetched.
fn group_post_path(url: &Url) -> Option<String> {
    let parts = segments(url);
    let ["groups", group] = parts.as_slice() else { return None };
    let post =
        query_param(url, "multi_permalinks").filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?;
    Some(format!("/groups/{group}/posts/{post}"))
}

/// The post's own link, for people: the key, but with the name spelt as in the link it came from.
pub fn link(target: &Target) -> String {
    if target.url.host_str() == Some(SHORT_HOST) {
        return target.url.to_string();
    }
    spelt(&target.original).to_string()
}

/// www.facebook.com, the identifying query parameters only, no fragment, no trailing slash, and
/// the name in lower case.
fn normalize(url: &Url) -> Url {
    let mut out = spelt(url);
    let folded = fold_name(out.path()).into_owned();
    out.set_path(&folded);
    out
}

/// [`normalize`], keeping the name as it's spelt.
fn spelt(url: &Url) -> Url {
    let mut out = Url::parse(ORIGIN).expect("valid origin");
    if let Some(path) = group_post_path(url) {
        out.set_path(&path);
        return keep_params(url, out);
    }
    let path = url.path();
    let trimmed = path.trim_end_matches('/');
    // Both name the same photo page; one cache entry for both.
    let trimmed = if trimmed == "/photo.php" { "/photo" } else { trimmed };
    out.set_path(if trimmed.is_empty() { "/" } else { trimmed });
    keep_params(url, out)
}

/// The path with the page's or person's name in lower case on a post, photo or video page
/// (`/SomePage/posts/…`): Facebook opens those in any spelling, so one cache entry for all. Not a
/// profile's own page, which Facebook redirects to its own spelling, and only the name: post
/// tokens (`pfbid…`) and share codes are case-sensitive.
fn fold_name(path: &str) -> std::borrow::Cow<'_, str> {
    let Some(rest) = path.strip_prefix('/').filter(|rest| !rest.starts_with('/')) else { return path.into() };
    let mut parts = rest.splitn(3, '/');
    let (Some(name), next) = (parts.next(), parts.next()) else { return path.into() };
    let named = matches!(next, Some("posts" | "photos" | "videos" | "permalink"));
    let section = name.ends_with(".php") || SECTIONS.iter().any(|s| s.eq_ignore_ascii_case(name));
    if !named || section || !name.bytes().any(|b| b.is_ascii_uppercase()) {
        return path.into();
    }
    format!("/{}{}", name.to_ascii_lowercase(), &rest[name.len()..]).into()
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
        // `/p/{Name}-{id}`: a profile without a username.
        ["p", name] if name.rsplit_once('-').is_some_and(|(_, id)| digits(&id)) => PageType::Profile,
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
        // Only the parameters the page is read by.
        assert_eq!(
            target("/somepage/posts/pfbid0Ab?id=1&v=2&set=a.3").key(),
            "https://www.facebook.com/somepage/posts/pfbid0Ab"
        );
        assert_eq!(target("/reel/1?fbid=2").key(), "https://www.facebook.com/reel/1");
        assert_eq!(target("/profile.php?id=4&v=info").key(), "https://www.facebook.com/profile.php?id=4");
        assert_eq!(
            target("/somepage/photo.php?fbid=6&id=1").key(),
            "https://www.facebook.com/somepage/photo.php?fbid=6"
        );
        // A page this doesn't know keeps every identifying one, as it may be what names the post.
        assert_eq!(
            target("/somepage/video/video.php?v=7&ref=x").key(),
            "https://www.facebook.com/somepage/video/video.php?v=7"
        );
        assert_eq!(target("/album.php?fbid=8&id=9&aid=1").key(), "https://www.facebook.com/album.php?fbid=8&id=9");
    }

    #[test]
    fn one_key_for_any_spelling_of_a_name() {
        let key = |path: &str| target(path).key().to_string();
        assert_eq!(
            key("/SomePage/posts/pfbid0AbC"),
            "https://www.facebook.com/somepage/posts/pfbid0AbC",
            "the token kept"
        );
        assert_eq!(key("/SomePage/"), "https://www.facebook.com/SomePage", "Facebook would redirect a profile");
        assert_eq!(link(&target("/SomePage/posts/1?ref=x")), "https://www.facebook.com/SomePage/posts/1", "as spelt");
        assert_eq!(key("/Some.Page/photos/a.1/2"), key("/some.page/photos/a.1/2"));
        assert_eq!(key("/SomePage/videos/3"), key("/somepage/videos/3"));
        assert_eq!(key("/share/p/AbC"), "https://www.facebook.com/share/p/AbC", "share codes are case-sensitive");
        assert_eq!(fold_name("//Name/posts/1"), "//Name/posts/1");
        assert_eq!(fold_name("/Name/posts/1"), "/name/posts/1");
        assert_eq!(key("/stories/AbC/UzpfSTE"), "https://www.facebook.com/stories/AbC/UzpfSTE");
        assert_eq!(key("/groups/AbC/posts/1"), "https://www.facebook.com/groups/AbC/posts/1");
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
            ("/stories/100/UzpfSTE/", PageType::Story, Some("UzpfSTE=")),
            ("/somepage", PageType::Profile, None),
            ("/Some.Page.II", PageType::Profile, None),
            ("/profile.php?id=8", PageType::Profile, None),
            ("/people/Some-Name/100075468038908/", PageType::Profile, None),
            ("/people/Some-Name/pfbid0Sc36v/", PageType::Profile, None),
            ("/p/Some-Name-61575651970810/", PageType::Profile, None),
            ("/somepage/posts/some-words-here/1234567890/", PageType::Post, None),
            ("/groups/9/?multi_permalinks=10&ref=share", PageType::Post, None),
            ("/me/about?multi_permalinks=1", PageType::Other, None),
            ("/me/about?story_fbid=1", PageType::Other, None),
            ("/groups/9/members?multi_permalinks=1", PageType::Other, None),
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
    fn story_cards_keep_their_padding() {
        let padded = Some("UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY=");
        for card in [
            "UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY",
            "UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY=",
            "UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY%3D",
        ] {
            assert_eq!(target(&format!("/stories/100/{card}/?v=1")).media_id().as_deref(), padded, "{card}");
        }
        let whole = "UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY3OA";
        assert_eq!(
            target(&format!("/stories/100/{whole}/")).media_id().as_deref(),
            Some("UzpfSVNDOjEyMzQ1Njc4OTAxMjM0NTY3OA==")
        );
    }

    #[test]
    fn video_short_links() {
        let t = target("/fb.watch/B4Qtd0WLlk/?mibextid=x");
        assert_eq!((t.key(), t.original.as_str()), ("https://fb.watch/B4Qtd0WLlk/", "https://fb.watch/B4Qtd0WLlk/"));
        assert_eq!(t.page_type, PageType::Post);
        assert_eq!(Target::from_link("https://fb.watch/B4Qtd0WLlk/").unwrap().key(), t.key());
        assert!(request("/fb.watch/a!b").is_none());
        assert!(request("/fb.watch/").is_none());
    }

    #[test]
    fn story_tokens() {
        assert_eq!(target("/somepage/posts/pfbid02x").story_token().as_deref(), Some("pfbid02x"));
        assert_eq!(target("/groups/9/permalink/10/").story_token().as_deref(), Some("10"));
        assert_eq!(target("/posts/1").story_token(), None);
        assert_eq!(target("/somepage/posts/some-words/123/").story_token().as_deref(), Some("123"));
        assert_eq!(target("/groups/9/?multi_permalinks=10").story_token().as_deref(), Some("10"));
        assert_eq!(target("/somepage/posts/1?comment_id=x1").key(), "https://www.facebook.com/somepage/posts/1");
        let group_link = target("/groups/9/?multi_permalinks=10&comment_id=11&ref=share");
        assert_eq!(group_link.key(), "https://www.facebook.com/groups/9/posts/10?comment_id=11", "the post's own page");
        assert_eq!(group_link.original.query(), Some("multi_permalinks=10&comment_id=11&ref=share"));
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
        assert!(is_login_url(&Url::parse("https://www.facebook.com/login.php?next=x").unwrap()));
        assert!(is_login_url(&Url::parse("https://www.facebook.com/two_step_verification/two_factor/").unwrap()));
        for page in ["/checkpointsoftware/posts/1", "/LoginRadius/videos/1", "/someone/posts/login"] {
            assert!(!is_login_url(&Url::parse(&format!("https://www.facebook.com{page}")).unwrap()), "{page}");
        }
        // A redirect to a group's page opened on a post fetches the post's page.
        let hop = Url::parse("https://web.facebook.com/groups/9/?multi_permalinks=10&ref=share").unwrap();
        assert_eq!(clean(&hop).as_str(), "https://web.facebook.com/groups/9/posts/10");
        assert_eq!(clean_as_sent(&hop).as_str(), "https://web.facebook.com/groups/9/?multi_permalinks=10");

        let story = Url::parse("https://m.facebook.com/story.php?story_fbid=pfbid0x&id=100&x=1").unwrap();
        assert_eq!(share_redirect_target(&story).unwrap().as_str(), "https://www.facebook.com/100/posts/pfbid0x");
        let missing = Url::parse("https://m.facebook.com/permalink.php?story_fbid=pfbid0x").unwrap();
        assert!(share_redirect_target(&missing).is_none());
    }
}
