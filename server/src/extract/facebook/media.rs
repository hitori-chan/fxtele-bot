//! Finding the post's media, per page type (`docs/facebook.md` describes where it sits).

use std::collections::HashSet;

use super::index::{Index, Obj, each, first_text, get_obj, id_of, id_text, number, text, walk_obj};
use crate::extract::SmallerFile;
use crate::extract::{PageType, Target};

/// A media item found in the page's JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: Option<String>,
    pub url: String,
    pub video: bool,
    pub thumbnail: Option<String>,
    pub caption: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Where a story keeps its attachments. Each wrapper is explicit: searching whole subtrees
/// lets feed media leak in.
const STORY_ATTACHMENTS: [&str; 2] = ["node_v2.attachments", "node_v2.comet_sections.content.story.attachments"];

/// A story node's media, in this order: single attachments, the scoped album, then albums
/// inside attachments.
fn story_media_items(node: &Obj) -> Vec<&Obj> {
    let mut items: Vec<&Obj> = Vec::new();
    for path in STORY_ATTACHMENTS {
        items.extend(each(node, path, "styles.attachment.media"));
    }
    items.extend(each(node, "all_subattachments.nodes", "media"));
    for path in STORY_ATTACHMENTS {
        for album in each(node, path, "styles.attachment.all_subattachments") {
            items.extend(each(album, "nodes", "media"));
        }
    }
    items
}

const VIDEO_CAPTION: [&str; 2] =
    ["creation_story.message.text", "creation_story.comet_sections.message.story.message.text"];
/// A video's own files, best first.
const LEGACY_FILES: [&str; 2] =
    ["videoDeliveryLegacyFields.browser_native_hd_url", "videoDeliveryLegacyFields.browser_native_sd_url"];
/// A video's own files, smallest first: what's left when the best is too big for Telegram.
const SMALLER_FILES: [&str; 2] = ["videoDeliveryLegacyFields.browser_native_sd_url", "playable_url"];

/// Where a photo keeps its full-size image, best first.
const PHOTO_IMAGES: [&str; 4] = ["viewer_image", "photo_image", "image", "massive_image"];
const THUMBNAIL: [&str; 5] =
    ["first_frame_thumbnail", "preferred_thumbnail.image.uri", "thumbnailImage.uri", "previewImage.uri", "image.uri"];

/// Media for the target. `story` holds the story's own nodes (story pages only).
pub fn candidates(index: &Index<'_>, target: &Target, story: &[&Obj]) -> Vec<Candidate> {
    let kind = target.page_type;
    let target_id = target.media_id();
    match kind {
        PageType::Post if story.is_empty() => return Vec::new(),
        // Read from meta tags or the album page instead.
        PageType::Profile | PageType::Album | PageType::Other => return Vec::new(),
        PageType::Watch => {
            let Some(id) = target_id else { return Vec::new() };
            return video_playback(index, &id).or_else(|| dash_prefetch(index, &id)).into_iter().collect();
        }
        PageType::Reel | PageType::Video | PageType::Photo | PageType::Story if target_id.is_none() => {
            return Vec::new();
        }
        _ => {}
    }
    let target_id = target_id.as_deref();

    let mut found = Found::default();
    let nodes = if kind == PageType::Post { story } else { &index.nodes };
    for node in nodes {
        match kind {
            PageType::Story if id_of(node).as_deref() != target_id => continue,
            _ => {}
        }
        let check_id = |c: &Candidate| kind == PageType::Story || c.id.as_deref() == target_id;
        match kind {
            PageType::Reel => {
                if let Some(video) = get_obj(node, "video") {
                    found.add_if(reel(video), check_id);
                }
            }
            PageType::Video => {
                for media in get_obj(node, "video.story").into_iter().flat_map(|s| each(s, "attachments", "media")) {
                    found.add_if(video_media(media), check_id);
                }
            }
            PageType::Photo => {
                if let Some(media) = get_obj(node, "currMedia") {
                    found.add_if(photo_media(media), check_id);
                }
            }
            PageType::Story => {
                for media in each(node, "attachments", "media") {
                    found.add_if(story_card_media(media), check_id);
                }
            }
            PageType::Post => {
                for media in story_media_items(node) {
                    found.add_story(media);
                }
            }
            PageType::Watch | PageType::Profile | PageType::Album | PageType::Other => {}
        }
    }

    if found.items.is_empty() {
        match (kind, target_id) {
            (PageType::Reel | PageType::Video, Some(id)) => return video_playback(index, id).into_iter().collect(),
            (PageType::Post, _) => {
                for id in story_video_ids(story) {
                    if let Some(c) = video_playback(index, &id).or_else(|| dash_prefetch(index, &id)) {
                        found.add(c);
                    }
                }
            }
            _ => {}
        }
    }
    if found.items.is_empty() {
        // Videos whose files couldn't be found still have a picture.
        found.items = found.video_posters;
    }
    found.items
}

#[derive(Default)]
struct Found {
    items: Vec<Candidate>,
    seen: HashSet<String>,
    video_posters: Vec<Candidate>,
}

impl Found {
    fn add(&mut self, candidate: Candidate) {
        if self.seen.insert(candidate.url.clone()) {
            self.items.push(candidate);
        }
    }

    fn add_if(&mut self, candidate: Option<Candidate>, check: impl Fn(&Candidate) -> bool) {
        if let Some(candidate) = candidate.filter(|c| check(c)) {
            self.add(candidate);
        }
    }

    /// Story media: a video without a playable URL is kept aside, so the video
    /// fallbacks get a chance to find its file first.
    fn add_story(&mut self, media: &Obj) {
        let Some(candidate) = story_media(media) else { return };
        if !candidate.video && text(media, "__typename") == Some("Video") {
            if !self.video_posters.iter().any(|p| p.url == candidate.url) {
                self.video_posters.push(candidate);
            }
            return;
        }
        self.add(candidate);
    }
}

fn reel(video: &Obj) -> Option<Candidate> {
    let playback = get_obj(video, "creation_story.short_form_video_context.playback_video");
    let url = playback.and_then(|p| first_text(p, &LEGACY_FILES))?;
    let (width, height) = playback.map_or((None, None), dimensions);
    Some(Candidate {
        id: id_of(video).map(|id| id.into_owned()),
        url: clean_url(url),
        video: true,
        thumbnail: poster(
            playback.and_then(|p| text(p, "preferred_thumbnail.image.uri")),
            text(video, "creation_story.short_form_video_context.video.first_frame_thumbnail")
                .or_else(|| playback.and_then(|p| text(p, "first_frame_thumbnail"))),
            width,
            height,
        ),
        caption: text(video, "creation_story.message.text").map(str::to_string),
        width,
        height,
    })
}

fn video_media(media: &Obj) -> Option<Candidate> {
    let url = first_text(media, &LEGACY_FILES)?;
    let (width, height) = dimensions(media);
    Some(Candidate {
        id: id_of(media).map(|id| id.into_owned()),
        url: clean_url(url),
        video: true,
        thumbnail: poster(
            text(media, "preferred_thumbnail.image.uri"),
            text(media, "first_frame_thumbnail"),
            width,
            height,
        ),
        caption: first_text(media, &VIDEO_CAPTION).map(str::to_string),
        width,
        height,
    })
}

fn photo_media(media: &Obj) -> Option<Candidate> {
    let url = text(media, "image.uri")?;
    let (width, height) = get_obj(media, "image").map_or((None, None), dimensions);
    Some(Candidate {
        id: id_of(media).map(|id| id.into_owned()),
        url: clean_url(url),
        video: false,
        thumbnail: Some(clean_url(url)),
        caption: text(media, "creation_story.message.text").map(str::to_string),
        width,
        height,
    })
}

fn story_card_media(media: &Obj) -> Option<Candidate> {
    let video = first_text(media, &["playable_url_quality_hd", "playable_url"]);
    let url = video.or_else(|| text(media, "image.uri"))?;
    Some(Candidate {
        id: id_of(media).map(|id| id.into_owned()),
        url: clean_url(url),
        video: video.is_some(),
        thumbnail: first_text(media, &["previewImage.uri", "image.uri"]).map(clean_url),
        caption: text(media, "message.text").map(str::to_string),
        width: None,
        height: None,
    })
}

fn story_media(media: &Obj) -> Option<Candidate> {
    let video = first_text(
        media,
        &[
            "playable_url_quality_hd",
            "videoDeliveryLegacyFields.browser_native_hd_url",
            "playable_url",
            "videoDeliveryLegacyFields.browser_native_sd_url",
        ],
    );
    let photo = PHOTO_IMAGES.iter().find_map(|p| get_obj(media, p).filter(|o| text(o, "uri").is_some()));
    let url = video.or_else(|| photo.and_then(|p| text(p, "uri")))?;
    let (width, height) = if video.is_some() { dimensions(media) } else { photo.map_or((None, None), dimensions) };
    Some(Candidate {
        id: id_of(media).map(|id| id.into_owned()),
        url: clean_url(url),
        video: video.is_some(),
        thumbnail: first_text(
            media,
            &["previewImage.uri", "preferred_thumbnail.image.uri", "photo_image.uri", "image.uri"],
        )
        .map(clean_url),
        caption: first_text(media, &VIDEO_CAPTION).map(str::to_string),
        width,
        height,
    })
}

/// A video file's size from its `bitrate=` (bits a second) and the video's duration.
pub fn estimated_bytes(url: &str, duration_ms: Option<u64>) -> Option<u64> {
    let query = url.split_once('?')?.1;
    let bitrate: u64 = query.split('&').find_map(|pair| pair.strip_prefix("bitrate="))?.parse().ok()?;
    Some(bitrate.saturating_mul(duration_ms?) / 8000)
}

/// Facebook's smaller file of a video, when it isn't the one picked already.
pub fn smaller_file(index: &Index<'_>, id: &str, picked: &str, duration_ms: Option<u64>) -> Option<SmallerFile> {
    let url = index.with_id(id).find_map(|node| first_text(node, &SMALLER_FILES)).map(clean_url)?;
    (url != picked).then(|| SmallerFile { bytes: estimated_bytes(&url, duration_ms), url })
}

/// IDs of videos attached to the requested story.
fn story_video_ids(story: &[&Obj]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for node in story {
        for media in each(node, "attachments", "media") {
            if text(media, "__typename") == Some("Video")
                && let Some(id) = text(media, "id")
                && !ids.iter().any(|i| i == id)
            {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// The video node whose `id` is `id`: legacy playback fields, then progressive URLs by
/// quality, then the best MP4 representation.
fn video_playback(index: &Index<'_>, id: &str) -> Option<Candidate> {
    index.with_id(id).find_map(|node| playback_from_node(node, id))
}

fn playback_from_node(node: &Obj, id: &str) -> Option<Candidate> {
    let legacy = first_text(node, &LEGACY_FILES);
    let url = legacy.map(clean_url).or_else(|| {
        let mut progressive: Option<(i32, &str)> = None;
        let mut dash: Option<(i64, &str)> = None;
        for child in walk_obj(node) {
            if let Some(url) = text(child, "progressive_url") {
                let score = match text(child, "metadata.quality") {
                    Some("HD") => 2,
                    Some("SD") => 1,
                    _ => 0,
                };
                if progressive.is_none_or(|(best, _)| score > best) {
                    progressive = Some((score, url));
                }
            }
            if text(child, "mime_type") == Some("video/mp4")
                && let Some(url) = text(child, "base_url")
            {
                let bandwidth = number(child, "bandwidth").unwrap_or(0);
                if dash.is_none_or(|(best, _)| bandwidth > best) {
                    dash = Some((bandwidth, url));
                }
            }
        }
        progressive.map(|p| p.1).or(dash.map(|d| d.1)).map(clean_url)
    })?;
    // Logged-in pages wrap the video (`video.creation_story.attachments[].media`); the size
    // sits on the inner node with the same ID.
    let (width, height) = walk_obj(node)
        .filter(|n| id_of(n).as_deref() == Some(id))
        .map(dimensions)
        .find(|(w, h)| w.is_some() && h.is_some())
        .unwrap_or_else(|| dimensions(node));
    Some(Candidate {
        id: Some(id.to_string()),
        url,
        video: true,
        thumbnail: video_thumbnail(node),
        caption: VIDEO_CAPTION.iter().find_map(|path| walk_obj(node).find_map(|n| text(n, path))).map(str::to_string),
        width,
        height,
    })
}

/// The best MP4 in the page's DASH prefetch data for video `id`.
fn dash_prefetch(index: &Index<'_>, id: &str) -> Option<Candidate> {
    let mut best: Option<(i64, &str)> = None;
    let mut thumbnail = None;
    let mut caption = None;
    for node in &index.nodes {
        if id_of(node).as_deref() == Some(id) {
            thumbnail = thumbnail.or_else(|| video_thumbnail(node));
            caption = caption.or_else(|| text(node, "creation_story.comet_sections.message.story.message.text"));
        }
        for prefetch in each(node, "all_video_dash_prefetch_representations", "") {
            if prefetch.get("video_id").is_some_and(|v| !v.is_null())
                && prefetch.get("video_id").and_then(id_text).as_deref() != Some(id)
            {
                continue;
            }
            for representation in each(prefetch, "representations", "") {
                if text(representation, "mime_type") != Some("video/mp4") {
                    continue;
                }
                let Some(url) = text(representation, "base_url") else { continue };
                let bandwidth = number(representation, "bandwidth").unwrap_or(0);
                if best.is_none_or(|(b, _)| bandwidth > b) {
                    best = Some((bandwidth, url));
                }
            }
        }
    }
    let (_, url) = best?;
    Some(Candidate {
        id: Some(id.to_string()),
        url: clean_url(url),
        video: true,
        thumbnail,
        caption: caption.map(str::to_string),
        width: None,
        height: None,
    })
}

/// The cover the creator chose when it has the video's shape, else the video's first frame.
/// Reel covers are cut to 9:16 for Facebook's own player, whatever the video's shape, and a
/// mismatched poster gets cropped or letterboxed in Telegram.
fn poster(cover: Option<&str>, frame: Option<&str>, width: Option<u32>, height: Option<u32>) -> Option<String> {
    let fits = |url: &&str| match (original_size(url), width.zip(height)) {
        (Some((w, h)), Some((vw, vh))) if h > 0 && vh > 0 => {
            let ratio = (f64::from(w) / f64::from(h)) / (f64::from(vw) / f64::from(vh));
            (ratio - 1.0).abs() < 0.05
        }
        _ => true,
    };
    cover.filter(fits).or(frame).or(cover).map(clean_url)
}

/// A thumbnail on the node, else anywhere inside it.
fn video_thumbnail(node: &Obj) -> Option<String> {
    walk_obj(node).find_map(|n| first_text(n, &THUMBNAIL)).map(clean_url)
}

fn dimensions(node: &Obj) -> (Option<u32>, Option<u32>) {
    let pick = |keys: [&str; 2]| keys.iter().find_map(|k| number(node, k)).and_then(|n| u32::try_from(n).ok());
    (pick(["width", "original_width"]), pick(["height", "original_height"]))
}

/// An album that advertises more photos than the story page embeds.
#[derive(Debug, PartialEq, Eq)]
pub struct Album {
    pub token: String,
    pub count: usize,
}

pub fn album(index: &Index<'_>, tokens: &[String]) -> Option<Album> {
    index.nodes.iter().find_map(|node| {
        let token = text(node, "mediaset_token")?;
        let sub = get_obj(node, "all_subattachments")?;
        let url = text(node, "url").unwrap_or_default();
        if !tokens.iter().any(|t| super::scope::names(url, t)) {
            return None;
        }
        let count = usize::try_from(number(sub, "count")?).ok()?;
        let embedded = sub.get("nodes").and_then(|n| n.as_array()).map_or(0, Vec::len);
        (count > embedded).then(|| Album { token: token.to_string(), count })
    })
}

/// Photos on an album page, by ID and URL, at most `count`.
pub fn album_photos(index: &Index<'_>, count: usize) -> Vec<Candidate> {
    let mut ids = HashSet::new();
    let mut found = Found::default();
    for node in &index.nodes {
        if found.items.len() >= count {
            break;
        }
        if text(node, "__typename") != Some("Photo") {
            continue;
        }
        let image =
            PHOTO_IMAGES.iter().find_map(|path| get_obj(node, path).filter(|image| text(image, "uri").is_some()));
        let Some((image, url)) = image.and_then(|image| Some((image, text(image, "uri")?))) else { continue };
        let id = id_of(node).map(|id| id.into_owned());
        if let Some(id) = &id
            && !ids.insert(id.clone())
        {
            continue;
        }
        let url = clean_url(url);
        let (width, height) = dimensions(image);
        found.add(Candidate { id, thumbnail: Some(url.clone()), url, video: false, caption: None, width, height });
    }
    found.items
}

/// A media URL at full size: drops the `dl` (download) flag, and the `ctp` resize when the URL
/// also names the original's size in `cstp`. The CDN's signature (`oh`) covers `cstp` but not
/// `ctp`, so without it the CDN serves the original (`cstp=mx1440x1920`, `ctp=s552x414`: a
/// 310×414 thumbnail becomes the 1440×1920 photo). The rest stays byte for byte.
pub fn clean_url(url: &str) -> String {
    let url = url.trim();
    let Some((base, query)) = url.split_once('?') else { return url.to_string() };
    let (query, fragment) = query.split_once('#').map_or((query, None), |(q, f)| (q, Some(f)));
    let has_original = query.split('&').any(|p| p.starts_with("cstp="));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| *p != "dl" && !p.starts_with("dl="))
        .filter(|p| !(has_original && (*p == "ctp" || p.starts_with("ctp="))))
        .collect();
    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// The original's size from a CDN URL's `cstp=mx{width}x{height}`.
pub fn original_size(url: &str) -> Option<(u32, u32)> {
    let query = url.split_once('?')?.1;
    let value = query.split(['&', '#']).find_map(|p| p.strip_prefix("cstp="))?;
    let (width, height) = value.strip_prefix("mx")?.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// The CDN file name, for spotting two sizes of the same photo.
pub fn file_key(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn video_sizes_from_bitrate_and_duration() {
        assert_eq!(estimated_bytes("https://v.example/a.mp4?x=1&bitrate=891875&oe=1", Some(15_788)), Some(1_760_115));
        assert_eq!(estimated_bytes("https://v.example/a.mp4?x=1", Some(15_788)), None, "no bitrate");
        assert_eq!(estimated_bytes("https://v.example/a.mp4?bitrate=8", None), None, "no duration");
    }

    use serde_json::{Value, json};

    use super::*;
    use crate::extract::facebook::scope::{story_nodes, story_tokens};

    fn run(docs: &[Value], link: &str, route: &[&str]) -> Vec<String> {
        let target = Target::from_link(link).unwrap();
        let route: Vec<String> = route.iter().map(ToString::to_string).collect();
        let tokens = story_tokens(&target, &route);
        let index = Index::new(docs);
        candidates(&index, &target, &story_nodes(&index, &tokens)).into_iter().map(|c| c.url).collect()
    }

    fn story_document(token: &str, media_id: &str) -> Value {
        json!({ "node_v2": {
            "url": format!("https://www.facebook.com/example/posts/{token}"),
            "attachments": [{ "styles": { "attachment": { "media": {
                "id": media_id, "__typename": "Photo", "image": { "uri": format!("https://scontent.example/{media_id}.jpg") },
            }}}}],
        }})
    }

    fn story_video_document(token: &str, video_id: &str) -> Value {
        json!({
            "node_v2": {
                "post_id": token,
                "attachments": [{
                    "media": { "__typename": "Video", "id": video_id },
                    "styles": { "attachment": { "media": {
                        "__typename": "Video", "id": video_id,
                        "thumbnailImage": { "uri": format!("https://scontent.example/{video_id}.jpg") },
                    }}},
                }],
            },
            "all_video_dash_prefetch_representations": [{
                "video_id": video_id,
                "representations": [
                    { "mime_type": "video/mp4", "base_url": format!("https://video.example/{video_id}-low.mp4"), "bandwidth": 100 },
                    { "mime_type": "video/mp4", "base_url": format!("https://video.example/{video_id}-high.mp4"), "bandwidth": 200 },
                ],
            }],
        })
    }

    #[test]
    fn opaque_share_link_does_not_extract_unscoped_feed_media() {
        assert!(
            run(&[story_document("latest-feed-story", "p1")], "https://www.facebook.com/share/v/1JE8AhF9Fj/", &[])
                .is_empty()
        );
    }

    #[test]
    fn story_url_extracts_media_when_its_token_matches() {
        let urls =
            run(&[story_document("pfbid-target", "p1")], "https://www.facebook.com/example/posts/pfbid-target", &[]);
        assert_eq!(urls, ["https://scontent.example/p1.jpg"]);
    }

    #[test]
    fn share_link_extracts_media_when_a_route_token_matches() {
        let urls = run(
            &[story_document("resolved-story", "p1")],
            "https://www.facebook.com/share/v/1JE8AhF9Fj/",
            &["resolved-story"],
        );
        assert_eq!(urls, ["https://scontent.example/p1.jpg"]);
    }

    #[test]
    fn story_video_uses_only_the_video_the_story_references() {
        let docs =
            [story_video_document("target-story", "target-video"), story_video_document("other-story", "other-video")];
        let urls = run(&docs, "https://www.facebook.com/groups/example/permalink/target-story", &[]);
        assert_eq!(urls, ["https://video.example/target-video-high.mp4"]);
    }

    #[test]
    fn reel_requires_the_matching_id() {
        let reel = |id: &str| {
            json!({ "video": { "id": id, "creation_story": {
                "short_form_video_context": { "playback_video": {
                    "videoDeliveryLegacyFields": { "browser_native_sd_url": format!("https://video.example/{id}.mp4?dl=1&a=b") },
                    "preferred_thumbnail": { "image": { "uri": "https://scontent.example/t.jpg" } },
                    "width": 720, "height": 1280,
                }},
                "message": { "text": "Reel caption" },
            }}})
        };
        let docs = [reel("999"), reel("123")];
        let target = Target::from_link("https://www.facebook.com/reel/123").unwrap();
        let found = candidates(&Index::new(&docs), &target, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, "https://video.example/123.mp4?a=b", "the dl flag is dropped");
        assert!(found[0].video);
        assert_eq!((found[0].width, found[0].height), (Some(720), Some(1280)));
        assert_eq!(found[0].caption.as_deref(), Some("Reel caption"));
    }

    #[test]
    fn video_falls_back_to_progressive_then_dash() {
        let docs = [json!({ "id": "55", "playable": [
            { "progressive_url": "https://video.example/sd.mp4", "metadata": { "quality": "SD" } },
            { "progressive_url": "https://video.example/hd.mp4", "metadata": { "quality": "HD" } },
            { "mime_type": "video/mp4", "base_url": "https://video.example/dash.mp4", "bandwidth": 9 },
        ], "preferred_thumbnail": { "image": { "uri": "https://scontent.example/v.jpg" } } })];
        let urls = run(&docs, "https://www.facebook.com/page/videos/55", &[]);
        assert_eq!(urls, ["https://video.example/hd.mp4"]);
    }

    #[test]
    fn watch_uses_dash_prefetch_for_its_video_only() {
        // Prefetch data sits in the page's extensions, apart from the video node.
        let docs = [
            json!({ "video": { "id": "77", "image": { "uri": "https://scontent.example/w.jpg" } } }),
            json!({
                "all_video_dash_prefetch_representations": [
                    { "video_id": "78", "representations": [{ "mime_type": "video/mp4", "base_url": "https://video.example/other.mp4", "bandwidth": 999 }] },
                    { "video_id": 77, "representations": [
                        { "mime_type": "audio/mp4", "base_url": "https://video.example/audio.mp4", "bandwidth": 5000 },
                        { "mime_type": "video/mp4", "base_url": "https://video.example/77.mp4", "bandwidth": 10 },
                    ] },
                ],
            }),
        ];
        let target = Target::from_link("https://www.facebook.com/watch/?v=77").unwrap();
        let found = candidates(&Index::new(&docs), &target, &[]);
        assert_eq!(found[0].url, "https://video.example/77.mp4");
        assert_eq!(found[0].thumbnail.as_deref(), Some("https://scontent.example/w.jpg"));
    }

    #[test]
    fn photo_and_album() {
        let docs = [
            json!({ "currMedia": { "id": "4", "image": { "uri": "https://scontent.example/4.jpg", "width": 800, "height": 600 } } }),
        ];
        let target = Target::from_link("https://www.facebook.com/photo/?fbid=4").unwrap();
        let found = candidates(&Index::new(&docs), &target, &[]);
        assert_eq!((found[0].url.as_str(), found[0].width), ("https://scontent.example/4.jpg", Some(800)));

        let album = json!({ "node_v2": { "post_id": "t1", "attachments": [{ "styles": { "attachment": { "all_subattachments": { "nodes": [
            { "media": { "__typename": "Photo", "id": "a", "viewer_image": { "uri": "https://scontent.example/a.jpg" } } },
            { "media": { "__typename": "Photo", "id": "b", "image": { "uri": "https://scontent.example/b.jpg" } } },
            { "media": { "__typename": "Photo", "id": "a", "viewer_image": { "uri": "https://scontent.example/a.jpg" } } },
        ]}}}}]}});
        let urls = run(&[album], "https://www.facebook.com/groups/1/permalink/t1/", &[]);
        assert_eq!(urls, ["https://scontent.example/a.jpg", "https://scontent.example/b.jpg"]);
    }

    #[test]
    fn video_without_a_file_falls_back_to_its_picture() {
        let doc = json!({ "node_v2": { "post_id": "t2", "attachments": [{ "styles": { "attachment": { "media": {
            "__typename": "Video", "id": "v9", "image": { "uri": "https://scontent.example/poster.jpg" },
        }}}}]}});
        let urls = run(&[doc], "https://www.facebook.com/groups/1/permalink/t2/", &[]);
        assert_eq!(urls, ["https://scontent.example/poster.jpg"]);
    }

    #[test]
    fn story_card_matches_the_card_node() {
        let doc = json!({ "cards": [
            { "id": "other", "attachments": [{ "media": { "image": { "uri": "https://scontent.example/x.jpg" } } }] },
            { "id": "card1", "attachments": [{ "media": { "playable_url": "https://video.example/s.mp4", "previewImage": { "uri": "https://scontent.example/p.jpg" } } }] },
        ]});
        assert_eq!(run(&[doc], "https://www.facebook.com/stories/100/card1/", &[]), ["https://video.example/s.mp4"]);
    }

    #[test]
    fn urls_are_cleaned_minimally() {
        assert_eq!(clean_url(" https://x/a.mp4?dl=1&oh=AB%3D&oe=1 "), "https://x/a.mp4?oh=AB%3D&oe=1");
        assert_eq!(clean_url("https://x/a.mp4?dl=1"), "https://x/a.mp4");
        assert_eq!(clean_url("https://x/a.mp4"), "https://x/a.mp4");
        let thumb =
            "https://x/v/t39.99422-6/1_n.png?stp=dst-jpg_tt6&cstp=mx1440x1920&ctp=s552x414&_nc_cat=101&oh=00_A&oe=6A";
        assert_eq!(
            clean_url(thumb),
            "https://x/v/t39.99422-6/1_n.png?stp=dst-jpg_tt6&cstp=mx1440x1920&_nc_cat=101&oh=00_A&oe=6A"
        );
        assert_eq!(
            clean_url("https://x/a.jpg?ctp=s50x50&oh=1"),
            "https://x/a.jpg?ctp=s50x50&oh=1",
            "no original size named"
        );
        assert_eq!(original_size(thumb), Some((1440, 1920)));
        assert_eq!(original_size("https://x/a.jpg?cstp=s10&oh=1"), None);
        assert_eq!(original_size("https://x/a.jpg"), None);

        let cover = "https://x/cover.jpg?cstp=mx608x1080&ctp=s960x960&oh=1";
        let frame = "https://x/frame.jpg?cstp=mx1080x1080&ctp=s960x960&oh=1";
        let square = poster(Some(cover), Some(frame), Some(1080), Some(1080)).unwrap();
        assert!(square.starts_with("https://x/frame.jpg"), "a 9:16 cover on a square video: {square}");
        let tall = poster(Some(cover), Some(frame), Some(1080), Some(1920)).unwrap();
        assert!(tall.starts_with("https://x/cover.jpg"), "the cover fits a 9:16 video");
        assert!(
            poster(Some(cover), None, Some(1080), Some(1080)).unwrap().starts_with("https://x/cover.jpg"),
            "no frame"
        );
        assert_eq!(file_key("https://x/v/t1/123_456_n.jpg?stp=a"), "123_456_n.jpg");
    }
}
