//! Facebook extraction: fetch the page, read its JSON, assemble the post. `docs/facebook.md`
//! describes where the data sits.

pub mod fetch;
pub mod index;
pub mod media;
pub mod page;
pub mod scope;
pub mod text;
pub mod url;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, info, warn};

use self::index::{Index, Obj};
use self::media::{Album, Candidate, file_key};
use self::page::Page;
use self::url::{PageType, Target};
use crate::cookies::CookieStore;
use crate::extract::{
    BoxFuture, CookieChoice, ExtractError, ExtractResult, Extractor, Media, MediaKind, Post, PostKind, Stage, Timings,
};
use crate::settings::{CookiePolicy, Settings};

pub struct Facebook {
    fetcher: fetch::Fetcher,
    cookies: Arc<CookieStore>,
}

impl Facebook {
    pub fn new(cookies: Arc<CookieStore>) -> anyhow::Result<Self> {
        Ok(Self { fetcher: fetch::Fetcher::new()?, cookies })
    }

    async fn run(&self, target: &Target, settings: &Settings, choice: CookieChoice) -> ExtractResult {
        let mut timings = Timings::default();
        // Album expansion is extra; it gets what's left before the extraction deadline, less a
        // moment to build the page, so a slow album never costs the whole preview.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(settings.extract_timeout_seconds)
            - Duration::from_millis(300);
        let jar = self.cookies.header();
        let (first_with_cookies, retry_with_cookies) = match (choice, settings.cookie_policy) {
            (CookieChoice::Force, _) if jar.is_none() => {
                return Err(ExtractError::new(Stage::LoginWall, "no usable Facebook cookies are saved"));
            }
            (CookieChoice::Force, _) => (true, false),
            (CookieChoice::Policy, CookiePolicy::Always) => (jar.is_some(), false),
            (CookieChoice::Policy, CookiePolicy::Never) => (false, false),
            // Story cards always show logged-out visitors a login wall, so trying without
            // cookies first would only cost a round trip.
            (CookieChoice::Policy, CookiePolicy::Fallback) if needs_login(target.page_type) => (jar.is_some(), false),
            (CookieChoice::Policy, CookiePolicy::Fallback) => (false, jar.is_some()),
        };
        let proxy = settings.proxy.as_deref();
        let first_cookies = jar.as_deref().filter(|_| first_with_cookies);
        // A post's full album page needs a login, so it's fetched with cookies whenever the
        // policy allows them, even if the post itself loaded without.
        let album_cookies = jar.as_deref().filter(|_| first_with_cookies || retry_with_cookies);
        let album = (album_cookies, deadline);
        let result = match self.attempt(target, proxy, first_cookies, album, &mut timings).await {
            Err(e) if retry_with_cookies && matches!(e.stage, Stage::LoginWall | Stage::NotFound) => {
                debug!(url = %target.key(), "retrying with cookies after: {e}");
                self.attempt(target, proxy, jar.as_deref(), album, &mut timings).await
            }
            other => other,
        };
        result.map(|post| (post, timings))
    }

    async fn attempt(
        &self,
        target: &Target,
        proxy: Option<&str>,
        cookies: Option<&str>,
        (album_cookies, deadline): (Option<&str>, tokio::time::Instant),
        timings: &mut Timings,
    ) -> Result<Post, ExtractError> {
        let (final_url, page) = match self.fetcher.page(&target.url, proxy, cookies, timings).await {
            Err(e) if e.stage == Stage::LoginWall && cookies.is_some() => {
                self.session_rejected().await;
                return Err(ExtractError::new(Stage::LoginWall, "Facebook rejected the saved cookies"));
            }
            result => result?,
        };
        let started = Instant::now();
        let final_target = Target::from_url(final_url);
        let (mut post, album) = build_post(&page, &final_target, target.key())?;
        timings.extract += started.elapsed();

        if let (Some(album), Some(cookies)) = (album, album_cookies)
            && album.count > post.media.len()
        {
            let embedded = post.media.len();
            let expand = self.expand_album(&mut post, &album, proxy, cookies, timings);
            if tokio::time::timeout_at(deadline, expand).await.is_err() {
                info!(url = %post.key, embedded, of = album.count, "no time left to expand an album; keeping the embedded photos");
            }
        }
        Ok(post)
    }

    /// Fetches the album page when the story embeds only some of its photos (needs cookies).
    async fn expand_album(
        &self,
        post: &mut Post,
        album: &Album,
        proxy: Option<&str>,
        cookies: &str,
        timings: &mut Timings,
    ) {
        let Ok(album_url) = ::url::Url::parse_with_params(
            "https://www.facebook.com/media/set/",
            [("set", album.token.as_str()), ("type", "3")],
        ) else {
            return;
        };
        let page = match self.fetcher.page(&album_url, proxy, Some(cookies), timings).await {
            Ok((_, page)) => page,
            Err(e) if e.stage == Stage::LoginWall => {
                self.session_rejected().await;
                return;
            }
            Err(e) => {
                debug!("album page failed, keeping the embedded photos: {e}");
                return;
            }
        };
        let started = Instant::now();
        let index = Index::new(page.media());
        let photos = media::album_photos(&index, album.count);
        let before = post.media.len();
        let mut seen: Vec<String> = post.media.iter().map(|m| file_key(&m.url).to_string()).collect();
        for photo in photos {
            if post.media.len() >= album.count {
                break;
            }
            let key = file_key(&photo.url).to_string();
            if !seen.contains(&key) {
                seen.push(key);
                post.media.push(to_media(photo, None));
            }
        }
        timings.extract += started.elapsed();
        if post.media.len() > before {
            info!(url = %post.key, from = before, to = post.media.len(), "expanded an album");
        }
    }

    async fn session_rejected(&self) {
        if self.cookies.mark_rejected().await {
            warn!("Facebook rejected the saved cookies; paste fresh ones in the dashboard");
        }
    }

    /// Fetches a page and keeps only what extraction reads, with the logged-in user's ID
    /// replaced, as a test fixture.
    pub async fn record(&self, target: &Target, settings: &Settings, with_cookies: bool) -> anyhow::Result<String> {
        let cookies = if with_cookies {
            Some(self.cookies.header().ok_or_else(|| anyhow::anyhow!("no usable Facebook cookies are saved"))?)
        } else {
            None
        };
        let mut timings = Timings::default();
        let (final_url, page) =
            self.fetcher.page(&target.url, settings.proxy.as_deref(), cookies.as_deref(), &mut timings).await?;
        let mut html = format!("<!-- url: {final_url} -->\n<html><head>\n");
        let mut meta: Vec<_> = page.meta.iter().collect();
        meta.sort();
        for (property, content) in meta {
            html.push_str(&format!("<meta property=\"{property}\" content=\"{}\">\n", escape_attr(content)));
        }
        html.push_str("</head><body>\n");
        for script in &page.scripts {
            let mut json = script.json.clone();
            scrub(&mut json);
            let json = serde_json::to_string(&json)?.replace("</", "<\\/");
            html.push_str(&format!("<script type=\"application/json\" data-sjs>{json}</script>\n"));
        }
        html.push_str("</body></html>\n");
        if let Some(user) = self.cookies.user_id().filter(|_| with_cookies) {
            html = html.replace(&user, "0");
        }
        Ok(html)
    }
}

/// Drops what a saved page must not carry: comments (other people's names and words) and the
/// `viewer` block (the signed-in account, or a logged-out page's feed of other people's posts).
/// Extraction reads neither.
fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| key != "viewer" && !key.to_ascii_lowercase().contains("comment"));
            map.values_mut().for_each(scrub);
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}

impl Extractor for Facebook {
    fn extract<'a>(
        &'a self,
        target: &'a Target,
        settings: &'a Settings,
        cookies: CookieChoice,
    ) -> BoxFuture<'a, ExtractResult> {
        Box::pin(self.run(target, settings, cookies))
    }
}

/// Assembles the post from a fetched page. `target` is the final URL's; `key` the requested one's.
/// Page types Facebook only shows to logged-in visitors.
const fn needs_login(page_type: PageType) -> bool {
    matches!(page_type, PageType::StoryCard)
}

/// Photos an album page's preview shows at most.
const ALBUM_PAGE_PHOTOS: usize = 30;

pub fn build_post(page: &Page, target: &Target, key: &str) -> Result<(Post, Option<Album>), ExtractError> {
    let post = |kind, page_type, title: Option<String>, text: Option<String>, media| Post {
        key: key.to_string(),
        url: target.url.to_string(),
        page_type,
        title,
        author: None,
        published_at: None,
        text,
        media,
        kind,
    };
    match target.page_type {
        PageType::Profile => {
            if let Some(profile) = text::profile(page) {
                let mut post = post(
                    PostKind::Profile,
                    PageType::Profile,
                    profile.name.clone(),
                    profile.text,
                    vec![photo(&profile.picture)],
                );
                post.author = profile.name;
                return Ok((post, None));
            }
            // Not a profile after all (a section of the site, say); its meta tags may still do.
            return meta_post(page, target, key).map(|p| (p, None));
        }
        PageType::Other => return meta_post(page, target, key).map(|p| (p, None)),
        PageType::Album => {
            let index = Index::new(page.media());
            let photos = media::album_photos(&index, ALBUM_PAGE_PHOTOS);
            if photos.is_empty() {
                return meta_post(page, target, key).map(|p| (p, None));
            }
            let title = page.meta("og:title").map(text::clean_title).filter(|t| !text::is_generic_title(t));
            let media = photos.into_iter().map(|c| to_media(c, None)).collect();
            let mut post =
                post(PostKind::Media, PageType::Album, title, page.meta("og:description").map(str::to_string), media);
            // The album node's ID is the set token without its `a.`; its owner may be named elsewhere.
            post.author = target
                .album_set()
                .and_then(|set| set.strip_prefix("a.").map(str::to_string))
                .and_then(|album| index.with_id(&album).find_map(|n| index::text(n, "owner.id")).map(str::to_string))
                .and_then(|owner| index.with_id(&owner).find_map(|n| index::text(n, "name")).map(str::to_string));
            return Ok((post, None));
        }
        _ => {}
    }

    let documents: Vec<&Value> = page.media().collect();
    let index = Index::new(documents.iter().copied());
    let route = Index::new(page.route());
    let route_tokens = scope::route_tokens(&route);
    let tokens =
        if target.page_type == PageType::Story { scope::story_tokens(target, &route_tokens) } else { Vec::new() };
    let candidates = media::candidates(&index, target, &tokens);
    let media_id = target.media_id();
    let is_video = matches!(target.page_type, PageType::Reel | PageType::Video | PageType::Watch);
    // The post's own nodes: those naming the story, or carrying the media's ID, led by the story
    // a video belongs to (where logged-in pages keep the caption, post time and author).
    let video_story = match media_id.as_deref() {
        Some(id) if is_video => text::video_story(&index, id),
        _ => Vec::new(),
    };
    let own_nodes: Vec<&Obj> = match (target.page_type, media_id.as_deref()) {
        (PageType::Story, _) => index.nodes.iter().copied().filter(|n| scope::names_any(n, &tokens)).collect(),
        (_, Some(id)) => video_story.into_iter().chain(index.with_id(id)).collect(),
        _ => Vec::new(),
    };
    let route_title = text::route_title(&route).map(text::clean_title);
    let author = text::author(&own_nodes)
        .or_else(|| target.story_bucket().and_then(|bucket| text::story_owner(&index, &bucket)))
        .map(str::to_string);
    let base = |kind, media, title: Option<String>, text: Option<&str>| Post {
        key: key.to_string(),
        url: target.url.to_string(),
        page_type: target.page_type,
        title,
        author: author.clone(),
        published_at: text::published_at(&own_nodes),
        text: text.map(str::to_string),
        media,
        kind,
    };

    if candidates.is_empty() {
        if target.page_type == PageType::Story
            && let Some(body) = text::text_only(&index, &tokens)
        {
            let title = route_title.or_else(|| page.meta("og:title").map(text::clean_title));
            return Ok((base(PostKind::TextOnly, Vec::new(), title, Some(body)), None));
        }
        let message = if documents.is_empty() {
            "the page had no post data (a login wall, or Facebook changed its pages)".to_string()
        } else {
            format!("none of the page's {} post-data blocks hold this {}", documents.len(), target.page_type.as_str())
        };
        return Err(ExtractError::new(Stage::NotFound, message));
    }

    let group = media_id.as_deref().filter(|_| is_video).and_then(|id| text::group_post(&index, id));
    let thumbnail =
        candidates.iter().find_map(|c| c.thumbnail.clone()).or_else(|| page.meta("og:image").map(str::to_string));
    let title = group
        .as_ref()
        .and_then(|g| g.title.clone())
        .or(route_title)
        .or_else(|| page.meta("og:title").map(text::clean_title))
        .filter(|t| !text::is_generic_title(t));
    // Captions come only from the post's own data, so another post on the page can't leak in.
    let story_nodes: Vec<&Obj> = match target.page_type {
        PageType::Story => own_nodes.iter().flat_map(|node| index::walk_obj(node)).collect(),
        _ => Vec::new(),
    };
    let caption = candidates
        .iter()
        .find_map(|c| c.caption.as_deref())
        .or_else(|| text::own_caption(&own_nodes))
        .or_else(|| text::caption(&story_nodes))
        .or_else(|| group.as_ref().and_then(|g| g.caption.as_deref()))
        .or(page.meta("og:description"))
        .map(str::to_string);
    let media = candidates.into_iter().map(|c| to_media(c, thumbnail.as_deref())).collect();
    let mut post = base(PostKind::Media, media, title, caption.as_deref());
    if let Some(group) = group {
        post.url = group.url;
    }
    let album = (target.page_type == PageType::Story).then(|| media::album(&index, &tokens)).flatten();
    Ok((post, album))
}

/// A preview from the page's meta tags alone, for pages without post data of their own
/// (events, groups, marketplace...). Refuses login walls and Facebook's placeholder images.
fn meta_post(page: &Page, target: &Target, key: &str) -> Result<Post, ExtractError> {
    let not_found =
        || ExtractError::new(Stage::NotFound, "the page has no preview of its own (a login wall, or nothing public)");
    let title =
        page.meta("og:title").map(text::clean_title).filter(|t| !text::is_generic_title(t)).ok_or_else(not_found)?;
    let image = page.meta("og:image").filter(|url| !url.contains("/rsrc.php/")).map(photo);
    let description = page.meta("og:description").map(str::to_string);
    if image.is_none() && description.is_none() {
        return Err(not_found());
    }
    Ok(Post {
        key: key.to_string(),
        url: target.url.to_string(),
        page_type: target.page_type,
        title: Some(title),
        author: None,
        published_at: None,
        text: description,
        kind: if image.is_some() { PostKind::Media } else { PostKind::TextOnly },
        media: image.into_iter().collect(),
    })
}

/// A photo from a meta tag, at full size.
fn photo(url: &str) -> Media {
    let url = media::clean_url(url);
    let mut media = Media::photo(url);
    (media.width, media.height) = media::original_size(&media.url).unzip();
    media
}

fn to_media(candidate: Candidate, fallback_thumbnail: Option<&str>) -> Media {
    let mut media = if candidate.video {
        let poster = candidate.thumbnail.or_else(|| fallback_thumbnail.map(str::to_string));
        Media::video(candidate.url, poster.as_deref().map(media::clean_url))
    } else {
        Media::photo(candidate.url)
    };
    debug_assert!(media.kind == MediaKind::Video || media.thumbnail.is_none());
    // A photo served at its original size has the original's dimensions.
    let original = (media.kind == MediaKind::Photo).then(|| media::original_size(&media.url)).flatten();
    (media.width, media.height) = original.unzip();
    if original.is_none() {
        media.width = candidate.width;
        media.height = candidate.height;
    }
    media
}

fn escape_attr(value: &str) -> String {
    let mut out = String::new();
    crate::render::escape_into(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const GROUP_POST: &str = "https://www.facebook.com/groups/example.group/permalink/3880626025437561/";

    /// A Facebook-shaped page whose JSON blocks hold the given documents.
    fn page(documents: &[Value], title: &str) -> Page {
        let scripts: String = documents
            .iter()
            .map(|d| {
                format!(
                    r#"<script type="application/json" data-sjs>{}</script>"#,
                    json!({ "__bbox": { "result": { "data": d } } })
                )
            })
            .collect();
        let html = format!(
            "<html><head><title>{title}</title><meta property='og:title' content='{title}'></head><body>{scripts}</body></html>"
        );
        page::parse(html.as_bytes()).unwrap()
    }

    fn group_story(token: &str, attachments: Option<Value>, text: Option<&str>) -> Value {
        let mut story = json!({ "post_id": token });
        if let Some(a) = attachments {
            story["attachments"] = a;
        }
        if let Some(t) = text {
            story["comet_sections"] = json!({ "content": { "story": { "message": { "text": t } } } });
        }
        json!({ "node_v2": story })
    }

    fn build(page: &Page, link: &str) -> Result<Post, ExtractError> {
        let target = Target::from_link(link).unwrap();
        build_post(page, &target, target.key()).map(|(post, _)| post)
    }

    #[test]
    fn post_without_attachments_is_text_only() {
        let p = page(
            &[group_story("3880626025437561", Some(json!([])), Some("Hiring: security researcher"))],
            "Example Group | Hiring",
        );
        let post = build(&p, GROUP_POST).unwrap();
        assert_eq!(post.kind, PostKind::TextOnly);
        assert_eq!(post.text.as_deref(), Some("Hiring: security researcher"));
        assert_eq!(post.title.as_deref(), Some("Example Group | Hiring"));
        assert_eq!(post.url, "https://www.facebook.com/groups/example.group/permalink/3880626025437561");
    }

    #[test]
    fn post_with_media_is_a_media_post() {
        let doc = json!({ "node_v2": {
            "url": "https://www.facebook.com/example/posts/3880626025437561",
            "attachments": [{ "styles": { "attachment": { "media": {
                "id": "photo-1", "__typename": "Photo", "image": { "uri": "https://scontent.example/photo-1.jpg" } } } } }],
        }});
        let post = build(&page(&[doc], "t"), "https://www.facebook.com/example/posts/3880626025437561").unwrap();
        assert_eq!(post.kind, PostKind::Media);
        assert_eq!(
            post.media.iter().map(|m| m.url.as_str()).collect::<Vec<_>>(),
            ["https://scontent.example/photo-1.jpg"]
        );
    }

    #[test]
    fn missing_or_foreign_data_is_a_failure_not_a_text_post() {
        for p in [
            page(&[group_story("3880626025437561", None, Some("t"))], "t"),
            page(&[group_story("3880626025437561", Some(json!([])), None)], "t"),
            page(&[group_story("9999999999999999", Some(json!([])), Some("someone else's post"))], "t"),
            page::parse(b"<html><body>Log in to continue</body></html>").unwrap(),
        ] {
            assert_eq!(build(&p, GROUP_POST).unwrap_err().stage, Stage::NotFound);
        }
    }

    #[test]
    fn missing_media_on_a_reel_stays_a_failure() {
        let doc = json!({ "node_v2": { "post_id": "123", "attachments": [],
            "comet_sections": { "content": { "story": { "message": { "text": "t" } } } } } });
        assert_eq!(build(&page(&[doc], "t"), "https://www.facebook.com/reel/123").unwrap_err().stage, Stage::NotFound);
    }

    #[test]
    fn reel_in_a_group_gets_the_group_post() {
        let docs = [
            json!({ "video": { "id": "123", "creation_story": { "short_form_video_context": { "playback_video": {
                "videoDeliveryLegacyFields": { "browser_native_hd_url": "https://video.example/123.mp4" } } } } } }),
            json!({ "post_id": "456", "to": { "id": "77", "name": "Example Group" },
                "attachments": [{ "media": { "__typename": "Video", "id": "123" } }],
                "message": { "text": "Look at this\nmore" } }),
        ];
        let post = build(&page(&docs, "Reel | Facebook"), "https://www.facebook.com/reel/123").unwrap();
        assert_eq!(post.url, "https://www.facebook.com/groups/77/permalink/456/");
        assert_eq!(post.title.as_deref(), Some("Example Group | Look at this"));
        assert_eq!(post.text.as_deref(), Some("Look at this\nmore"));
        assert_eq!(post.media[0].kind, MediaKind::Video);
    }

    #[test]
    fn albums_needing_expansion_are_spotted() {
        let doc = json!({ "node_v2": { "post_id": "t1" }, "url": "https://www.facebook.com/x/posts/t1",
            "mediaset_token": "pcb.123", "all_subattachments": { "count": 5, "nodes": [
                { "media": { "__typename": "Photo", "id": "a", "image": { "uri": "https://scontent.example/a.jpg" } } } ] } });
        let target = Target::from_link("https://www.facebook.com/x/posts/t1").unwrap();
        let (post, album) = build_post(&page(&[doc], "t"), &target, target.key()).unwrap();
        assert_eq!(post.media.len(), 1);
        assert_eq!(album, Some(Album { token: "pcb.123".into(), count: 5 }));
    }

    #[test]
    fn logged_in_reel_reads_its_own_story() {
        // The layout logged-in reel pages use: the file and size on an inner media node, and the
        // caption, post time and author on the story the video is attached to.
        let docs = [
            json!({ "video": { "id": "42", "creation_story": { "attachments": [{ "media": {
                "__typename": "Video", "id": "42", "width": 1080, "height": 1920,
                "first_frame_thumbnail": "https://scontent.example/42.jpg",
                "videoDeliveryResponseFragment": { "videoDeliveryResponseResult": { "progressive_urls": [
                    { "progressive_url": "https://video.example/42-sd.mp4", "metadata": { "quality": "SD" } },
                    { "progressive_url": "https://video.example/42-hd.mp4", "metadata": { "quality": "HD" } } ] } } } }] } } }),
            json!({ "post_id": "9", "creation_time": 1_700_000_000, "message": { "text": "Caption #tag" },
                "actors": [{ "name": "Creator" }], "attachments": [{ "media": { "__typename": "Video", "id": "42" } }] }),
            json!({ "post_id": "10", "creation_time": 1, "message": { "text": "the next reel in the feed" },
                "actors": [{ "name": "Someone else" }], "attachments": [{ "media": { "__typename": "Video", "id": "43" } }] }),
        ];
        let post = build(&page(&docs, "t"), "https://www.facebook.com/reel/42").unwrap();
        assert_eq!(post.media[0].url, "https://video.example/42-hd.mp4");
        assert_eq!((post.media[0].width, post.media[0].height), (Some(1080), Some(1920)));
        assert_eq!(post.text.as_deref(), Some("Caption #tag"));
        assert_eq!(post.author.as_deref(), Some("Creator"));
        assert_eq!(post.published_at, Some(1_700_000_000));
    }

    #[test]
    fn story_card_author_is_the_bucket_owner() {
        let doc = json!({ "bucket": { "id": "100", "story_bucket_owner": { "name": "Bucket Owner" },
            "unified_stories": { "edges": [{ "node": { "id": "CARD", "creation_time": 1_700_000_000,
                "attachments": [{ "media": { "image": { "uri": "https://scontent.example/card.jpg" } } }] } }] } } });
        let post = build(&page(&[doc], "t"), "https://www.facebook.com/stories/100/CARD/").unwrap();
        assert_eq!(post.author.as_deref(), Some("Bucket Owner"));
        assert_eq!(post.media[0].url, "https://scontent.example/card.jpg");
    }

    #[test]
    fn other_pages_use_their_meta_tags() {
        let event = page::parse(
            br#"<meta property="og:title" content="Night Market"><meta property="og:description" content="Saturday 8 pm">
            <meta property="og:image" content="https://scontent.example/event.jpg">"#,
        )
        .unwrap();
        let post = build(&event, "https://www.facebook.com/events/17/").unwrap();
        assert_eq!((post.page_type, post.kind), (PageType::Other, PostKind::Media));
        assert_eq!(post.title.as_deref(), Some("Night Market"));
        assert_eq!(post.media[0].url, "https://scontent.example/event.jpg");

        let wall = page::parse(
            br#"<meta property="og:title" content="Log in or sign up to view">
            <meta property="og:image" content="https://static.xx.fbcdn.net/rsrc.php/v4/y1/r/logo.png">"#,
        )
        .unwrap();
        assert_eq!(build(&wall, "https://www.facebook.com/events/17/").unwrap_err().stage, Stage::NotFound);
        let placeholder = page::parse(
            br#"<meta property="og:title" content="Group"><meta property="og:image" content="https://static.xx.fbcdn.net/rsrc.php/v4/a.png">"#,
        )
        .unwrap();
        assert!(build(&placeholder, "https://www.facebook.com/groups/18").is_err(), "no real image or text");
    }

    #[test]
    fn profile_page() {
        let p = page::parse(
            br#"<meta property="al:android:url" content="fb://profile/100"><meta property="og:title" content="Jane">
            <meta property="og:image" content="https://scontent.example/jane.jpg">"#,
        )
        .unwrap();
        let post = build(&p, "https://www.facebook.com/jane.doe").unwrap();
        assert_eq!(post.kind, PostKind::Profile);
        assert_eq!(post.media[0].url, "https://scontent.example/jane.jpg");
    }
}
