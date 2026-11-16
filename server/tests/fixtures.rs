//! Saved Facebook pages (see `docs/development.md`) and what extraction makes of them.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use server::extract::facebook::media::{file_key, original_size};
use server::extract::facebook::url::Target;
use server::extract::facebook::{build_post, page};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("tests/fixtures exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "html"))
        .collect();
    paths.sort();
    paths
}

/// What a preview is made of, in a stable, readable form.
fn summary(path: &Path) -> String {
    let html = std::fs::read(path).unwrap();
    let head = String::from_utf8_lossy(&html[..html.len().min(2000)]).into_owned();
    let url = head
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("<!-- url: ")?.strip_suffix(" -->"))
        .expect("the page starts with its url");
    let target = Target::from_link(url).expect("a facebook.com url");
    let page = page::parse(&html).unwrap();
    let mut out = String::new();
    match build_post(&page, &target, target.key()) {
        Err(error) => writeln!(out, "error: {error}").unwrap(),
        Ok((post, album)) => {
            let published = post.published_at.and_then(|t| jiff::Timestamp::from_second(t).ok());
            writeln!(out, "page type: {} ({:?})", post.page_type.as_str(), post.kind).unwrap();
            writeln!(out, "title: {}", post.title.as_deref().unwrap_or("-")).unwrap();
            writeln!(out, "author: {}", post.author.as_deref().unwrap_or("-")).unwrap();
            writeln!(out, "published: {}", published.map_or("-".to_string(), |t| t.to_string())).unwrap();
            writeln!(out, "url: {}", post.url).unwrap();
            writeln!(out, "text: {}", post.text.as_deref().unwrap_or("-")).unwrap();
            for media in &post.media {
                let size = media.width.zip(media.height).map_or("?".to_string(), |(w, h)| format!("{w}x{h}"));
                let poster = media.thumbnail.as_deref().map_or(String::new(), |poster| {
                    let size = original_size(poster).map_or("?".to_string(), |(w, h)| format!("{w}x{h}"));
                    format!(" +poster {size}")
                });
                writeln!(out, "- {:?} {size} {}{poster}", media.kind, file_key(&media.url)).unwrap();
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
