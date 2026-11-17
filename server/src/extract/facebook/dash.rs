//! Facebook's DASH streams for a video (`docs/facebook.md`, "Video quality"): the separate video and
//! audio files behind its player. Instagram's manifests read the same way ([`hd_from_manifest`]).
//! They go above the 720p file with sound, but only as AV1 or VP9, so the server joins a video and
//! an audio file into one MP4 (`crate::video`).
//!
//! The streams are listed twice per page: as an MPD manifest next to the progressive URLs
//! (`dash_manifest_xml_string`), and as JSON in `all_video_dash_prefetch_representations`.

use data_encoding::{BASE64, BASE64URL};

use super::index::{Index, each, id_text, number, text};

/// The best quality joined.
const MAX_LINES: u32 = 1080;
use super::media::clean_url;
use crate::extract::HdVideo;
use crate::extract::page::decode_entities;

/// The progressive file's height when its URL doesn't say.
const PROGRESSIVE_HEIGHT: u32 = 720;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stream {
    url: String,
    audio: bool,
    width: u32,
    height: u32,
    bandwidth: u64,
    codecs: String,
}

impl Stream {
    /// The quality label's number ("1080p" is the short side).
    fn lines(&self) -> u32 {
        self.width.min(self.height)
    }

    /// Audio, or video with a known size.
    const fn usable(&self) -> bool {
        self.audio || (self.width > 0 && self.height > 0)
    }
}

/// The best video stream above the progressive file's quality, with the best audio stream.
/// `None` when the progressive file is already as good.
pub fn hd(index: &Index<'_>, id: &str, progressive_url: &str) -> Option<HdVideo> {
    let mut streams = manifest_streams(index, id);
    for stream in prefetch_streams(index, id) {
        if !streams.iter().any(|s| s.url == stream.url) {
            streams.push(stream);
        }
    }
    let floor = progressive_height(progressive_url).unwrap_or(PROGRESSIVE_HEIGHT);
    best(&streams, floor, duration_ms(index, id))
}

/// The best video stream of an MPD manifest above `floor` lines, with its best audio stream.
/// Instagram's manifests have the same shape as Facebook's.
pub fn hd_from_manifest(xml: &str, floor: u32) -> Option<HdVideo> {
    best(&parse_manifest(xml), floor, manifest_duration_ms(xml))
}

fn best(streams: &[Stream], floor: u32, duration_ms: Option<u64>) -> Option<HdVideo> {
    // At most 1080p: a 4K stream would rarely fit Telegram's limit, and passing over it for the
    // progressive file would lose the 1080p one that does.
    let video = streams
        .iter()
        .filter(|s| !s.audio && s.lines() > floor && s.lines() <= MAX_LINES)
        .max_by_key(|s| (s.lines(), s.bandwidth))?;
    let audio = streams.iter().filter(|s| s.audio).max_by_key(|s| s.bandwidth);
    // Unknown without the video's bitrate: the download's own limit decides then.
    let bytes = duration_ms.filter(|_| video.bandwidth > 0).map(|ms| {
        let bits_per_second = video.bandwidth.saturating_add(audio.map_or(0, |a| a.bandwidth));
        bits_per_second.saturating_mul(ms) / 8000
    });
    Some(HdVideo {
        video: video.url.clone(),
        audio: audio.map(|a| a.url.clone()),
        width: video.width,
        height: video.height,
        codec: video.codecs.clone(),
        bytes,
    })
}

/// Streams from the JSON prefetch lists for this video.
fn prefetch_streams(index: &Index<'_>, id: &str) -> Vec<Stream> {
    let mut streams = Vec::new();
    for node in &index.nodes {
        for item in each(node, "all_video_dash_prefetch_representations", "") {
            if item.get("video_id").and_then(id_text).as_deref() != Some(id) {
                continue;
            }
            for r in each(item, "representations", "") {
                let (Some(mime), Some(url)) = (text(r, "mime_type"), text(r, "base_url")) else { continue };
                let size = |key| number(r, key).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                streams.push(Stream {
                    url: clean_url(url),
                    audio: mime.starts_with("audio/"),
                    width: size("width"),
                    height: size("height"),
                    bandwidth: number(r, "bandwidth").and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
                    codecs: text(r, "codecs").unwrap_or_default().to_string(),
                });
            }
        }
    }
    streams.retain(Stream::usable);
    streams
}

/// Streams from the MPD manifest on this video's own node.
fn manifest_streams(index: &Index<'_>, id: &str) -> Vec<Stream> {
    index
        .with_id(id)
        .find_map(|node| {
            text(node, "dash_manifest_xml_string")
                .or_else(|| text(node, "videoDeliveryLegacyFields.dash_manifest_xml_string"))
        })
        .map(parse_manifest)
        .unwrap_or_default()
}

/// The `Representation`s of an MPD. Facebook's manifests are machine-made and regular, so a
/// small scanner does instead of an XML parser.
fn parse_manifest(xml: &str) -> Vec<Stream> {
    let mut streams = Vec::new();
    for set in xml.split("<AdaptationSet").skip(1) {
        let set = set.split("</AdaptationSet>").next().unwrap_or(set);
        let set_tag = set.split('>').next().unwrap_or_default();
        let set_type = attribute(set_tag, "contentType").or_else(|| attribute(set_tag, "mimeType")).unwrap_or_default();
        for representation in set.split("<Representation").skip(1) {
            let representation = representation.split("</Representation>").next().unwrap_or(representation);
            let tag = representation.split('>').next().unwrap_or_default();
            let Some(url) = representation
                .split_once("<BaseURL>")
                .and_then(|(_, rest)| rest.split_once("</BaseURL>"))
                .map(|(url, _)| decode_entities(url.trim()))
            else {
                continue;
            };
            let kind = attribute(tag, "mimeType").unwrap_or(set_type);
            let size = |name| attribute(tag, name).and_then(|v| v.parse().ok()).unwrap_or(0);
            let stream = Stream {
                url: clean_url(&url),
                audio: kind.starts_with("audio"),
                width: size("width"),
                height: size("height"),
                bandwidth: attribute(tag, "bandwidth").and_then(|v| v.parse().ok()).unwrap_or(0),
                codecs: attribute(tag, "codecs").unwrap_or_default().to_string(),
            };
            if stream.usable() {
                streams.push(stream);
            }
        }
    }
    streams
}

/// The manifest's `mediaPresentationDuration` (`PT1M2.5S`), in milliseconds.
fn manifest_duration_ms(xml: &str) -> Option<u64> {
    let tag = xml.split_once("<MPD")?.1.split('>').next()?;
    let mut rest = attribute(tag, "mediaPresentationDuration")?.strip_prefix("PT")?;
    let mut ms = 0.0;
    for (unit, scale) in [('H', 3_600_000.0), ('M', 60_000.0), ('S', 1000.0)] {
        if let Some((n, after)) = rest.split_once(unit) {
            ms += n.parse::<f64>().ok()? * scale;
            rest = after;
        }
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a video's length")]
    (ms > 0.0).then_some(ms as u64)
}

/// The value of `name="..."` in an opening tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + name.len()..];
        if before.is_some_and(char::is_whitespace)
            && let Some(value) = after.strip_prefix("=\"")
        {
            return value.split('"').next();
        }
        rest = after;
    }
    None
}

/// How long the video is, from its own nodes.
pub fn duration_ms(index: &Index<'_>, id: &str) -> Option<u64> {
    index.with_id(id).find_map(|node| number(node, "playable_duration_in_ms")).and_then(|ms| u64::try_from(ms).ok())
}

/// The progressive file's height, from the encoding tag in its `efg` parameter
/// (base64 JSON with `"vencode_tag":"..._720p"`).
fn progressive_height(url: &str) -> Option<u32> {
    let parsed = url::Url::parse(url).ok()?;
    let efg = parsed.query_pairs().find(|(k, _)| k == "efg")?.1;
    let json = BASE64.decode(efg.as_bytes()).or_else(|_| BASE64URL.decode(efg.as_bytes())).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let tag = value.get("vencode_tag")?.as_str()?;
    let label = tag.rsplit('_').next()?.strip_suffix('p')?;
    label.parse().ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const MPD: &str = r#"<?xml version="1.0"?><MPD mediaPresentationDuration="PT10S"><Period>
<AdaptationSet id="0" contentType="video"><Representation id="1v" bandwidth="800000" codecs="avc1.64001f" mimeType="video/mp4" width="720" height="1280"><BaseURL>https://video.example/720.mp4?a=1&amp;b=2</BaseURL><SegmentBase indexRange="1-2"><Initialization range="0-1"/></SegmentBase></Representation>
<Representation id="2v" bandwidth="1600000" codecs="av01.0.08M.08" mimeType="video/mp4" width="1080" height="1920"><BaseURL>https://video.example/1080.mp4?a=1&amp;dl=1</BaseURL></Representation></AdaptationSet>
<AdaptationSet id="1" contentType="audio"><Representation id="3a" bandwidth="64000" codecs="mp4a.40.5" mimeType="audio/mp4"><BaseURL>https://video.example/audio.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;

    fn efg(tag: &str) -> String {
        let json = format!(r#"{{"vencode_tag":"{tag}"}}"#);
        url::form_urlencoded::byte_serialize(BASE64.encode(json.as_ref()).as_bytes()).collect()
    }

    #[test]
    fn reads_the_manifest() {
        let streams = parse_manifest(MPD);
        assert_eq!(streams.len(), 3);
        assert_eq!(streams[0].url, "https://video.example/720.mp4?a=1&b=2", "entities decoded");
        assert_eq!(streams[1].url, "https://video.example/1080.mp4?a=1", "cleaned like other media URLs");
        assert_eq!((streams[1].width, streams[1].height, streams[1].lines()), (1080, 1920, 1080));
        assert!(streams[2].audio && streams[2].bandwidth == 64000);
        assert_eq!(attribute(r#"<R xwidth="1" width="2">"#, "width"), Some("2"));
    }

    #[test]
    fn picks_the_best_stream_above_the_progressive_file() {
        let doc = json!({ "id": "9", "playable_duration_in_ms": 10_000, "videoDeliveryLegacyFields": { "dash_manifest_xml_string": MPD } });
        let index = Index::new([&doc]);
        let progressive = format!(
            "https://video.example/p.mp4?efg={}&oe=1",
            efg("xpv_progressive.FACEBOOK..C3.720.dash_h264-basic-gen2_720p")
        );
        assert_eq!(progressive_height(&progressive), Some(720));
        let hd = hd(&index, "9", &progressive).unwrap();
        assert_eq!(hd.video, "https://video.example/1080.mp4?a=1");
        assert_eq!(hd.audio.as_deref(), Some("https://video.example/audio.mp4"));
        assert_eq!((hd.width, hd.height, hd.codec.as_str()), (1080, 1920, "av01.0.08M.08"));
        assert_eq!(hd.bytes, Some((1_600_000 + 64_000) * 10 / 8));

        // Never above 1080p.
        let with_4k = MPD.replace(r#"width="1080" height="1920""#, r#"width="2160" height="3840""#);
        let streams: Vec<Stream> = [parse_manifest(&with_4k), parse_manifest(MPD)].concat();
        assert_eq!(best(&streams, 720, None).unwrap().height, 1920);
        let mut no_rate = streams;
        no_rate.iter_mut().for_each(|s| s.bandwidth = 0);
        assert_eq!(best(&no_rate, 720, Some(10_000)).unwrap().bytes, None, "a size unknown, not 0");

        // A 1080p progressive file leaves nothing better.
        let full = format!("https://video.example/p.mp4?efg={}", efg("x_1080p"));
        assert!(super::hd(&index, "9", &full).is_none());
        assert!(super::hd(&index, "8", &progressive).is_none(), "another video's streams");
    }

    #[test]
    fn falls_back_to_the_prefetch_list() {
        let doc = json!({ "extensions": { "all_video_dash_prefetch_representations": [
            { "video_id": "7", "representations": [
                { "mime_type": "video/mp4", "codecs": "vp09", "base_url": "https://video.example/v.mp4", "bandwidth": 2_000_000, "width": 1444, "height": 1078 },
                { "mime_type": "audio/mp4", "codecs": "mp4a.40.5", "base_url": "https://video.example/a.mp4", "bandwidth": 75_000 }
            ] },
            { "video_id": "8", "representations": [
                { "mime_type": "video/mp4", "base_url": "https://video.example/other.mp4", "bandwidth": 9, "width": 1920, "height": 1080 }
            ] }
        ] } });
        let index = Index::new([&doc]);
        let hd = hd(&index, "7", "https://video.example/p.mp4").unwrap();
        assert_eq!((hd.video.as_str(), hd.width, hd.height), ("https://video.example/v.mp4", 1444, 1078));
        assert_eq!(hd.audio.as_deref(), Some("https://video.example/a.mp4"));
        assert_eq!(hd.bytes, None, "no duration on the page");
    }
}
