//! Saved Facebook pages and other sites' answers (see `docs/development.md`) and what extraction
//! makes of them.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use server::extract::facebook::build_post;
use server::extract::facebook::media::{file_key, original_size};
use server::extract::{ExtractError, Post, Target, page};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("tests/fixtures exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "html" || e == "json"))
        .collect();
    paths.sort();
    paths
}

/// What a preview is made of, in a stable, readable form.
fn summary(path: &Path) -> String {
    let saved = std::fs::read(path).unwrap();
    let (result, album): (Result<Post, ExtractError>, _) = if path.extension().is_some_and(|e| e == "json") {
        let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        let target = Target::from_link(saved["url"].as_str().expect("the answers hold their url")).unwrap();
        (server::extract::read_fixture(&saved, &target), None)
    } else {
        let url = page::saved_url(&saved).expect("the page starts with its url");
        let target = Target::from_link(url).expect("a facebook.com url");
        match build_post(&page::parse(&saved).unwrap(), &target, target.key()) {
            Ok((post, album)) => (Ok(post), album),
            Err(e) => (Err(e), None),
        }
    };
    let mut out = String::new();
    match result {
        Err(error) => writeln!(out, "error: {error}").unwrap(),
        Ok(post) => {
            let published = post.published_at.and_then(|t| jiff::Timestamp::from_second(t).ok());
            writeln!(out, "page type: {} ({:?})", post.page_type.as_str(), post.kind).unwrap();
            writeln!(out, "title: {}", post.title.as_deref().unwrap_or("-")).unwrap();
            writeln!(out, "author: {}", post.author.as_deref().unwrap_or("-")).unwrap();
            let profile = &post.profile;
            writeln!(out, "profile: {}", profile.url.as_deref().unwrap_or("-")).unwrap();
            if let Some(handle) = &profile.handle {
                writeln!(out, "handle: @{handle}").unwrap();
            }
            writeln!(out, "avatar: {}", profile.avatar.as_deref().map_or("-", file_key)).unwrap();
            writeln!(out, "group: {}", post.group.as_deref().unwrap_or("-")).unwrap();
            let count = |n: Option<u64>| n.map_or("-".to_string(), |n| n.to_string());
            let stats = &post.stats;
            writeln!(
                out,
                "stats: reactions {}, comments {}, shares {}, views {}",
                count(stats.reactions),
                count(stats.comments),
                count(stats.shares),
                count(stats.views)
            )
            .unwrap();
            writeln!(out, "published: {}", published.map_or("-".to_string(), |t| t.to_string())).unwrap();
            writeln!(out, "url: {}", post.url).unwrap();
            writeln!(out, "text: {}", post.text.as_deref().unwrap_or("-")).unwrap();
            for media in &post.media {
                let size = media.width.zip(media.height).map_or("?".to_string(), |(w, h)| format!("{w}x{h}"));
                let poster = media.thumbnail.as_deref().map_or(String::new(), |poster| {
                    let size = original_size(poster).map_or("?".to_string(), |(w, h)| format!("{w}x{h}"));
                    format!(" +poster {size}")
                });
                let hd = media.hd.as_ref().map_or(String::new(), |hd| {
                    let codec = hd.codec.split('.').next().unwrap_or("?");
                    let bytes = hd.bytes.map_or("?".to_string(), |b| format!("{:.1} MB", b as f64 / 1e6));
                    let audio = if hd.audio.is_some() { " with audio" } else { "" };
                    format!(" +hd {codec} {}x{} {bytes}{audio}", hd.width, hd.height)
                });
                let mb = |b: u64| format!("{:.1} MB", b as f64 / 1e6);
                let estimate = media.bytes.map_or(String::new(), |b| format!(" ~{}", mb(b)));
                let smaller = media.smaller.as_ref().map_or(String::new(), |s| {
                    format!(
                        " (smaller: {}{})",
                        file_key(&s.url),
                        s.bytes.map_or(String::new(), |b| format!(" ~{}", mb(b)))
                    )
                });
                writeln!(out, "- {:?} {size} {}{estimate}{poster}{hd}{smaller}", media.kind, file_key(&media.url))
                    .unwrap();
            }
            if let Some(album) = album {
                writeln!(out, "album to expand: {} photos", album.count).unwrap();
            }
        }
    }
    out
}

#[test]
fn saved_pages() {
    let paths = fixtures();
    assert!(!paths.is_empty(), "no saved pages");
    // Check every page before failing, so one changed snapshot doesn't hide the others.
    let failed: Vec<String> = paths
        .iter()
        .filter_map(|path| {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let summary = summary(path);
            std::panic::catch_unwind(|| insta::assert_snapshot!(name.clone(), summary)).is_err().then_some(name)
        })
        .collect();
    assert!(failed.is_empty(), "changed snapshots: {failed:?} (review them with `cargo insta review`)");
}
