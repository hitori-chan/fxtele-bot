//! Parse and extraction speed. Uses saved pages from tests/fixtures when there are any,
//! plus a synthetic page shaped like a real story page (about 2 MB, mostly feed noise).

use std::hint::black_box;

use server::extract::facebook::url::Target;
use server::extract::facebook::{build_post, page};

fn main() {
    divan::main();
}

fn synthetic_page() -> String {
    let noise: String = (0..400)
        .map(|i| {
            format!(
                r#"<script type="application/json" data-sjs>{{"__bbox":{{"result":{{"data":{{"node_v2":{{"post_id":"feed{i}","attachments":[{{"styles":{{"attachment":{{"media":{{"__typename":"Photo","id":"f{i}","image":{{"uri":"https://scontent.example/f{i}.jpg"}}}}}}}}}}],"comet_sections":{{"content":{{"story":{{"message":{{"text":"{}"}}}}}}}}}}}}}}}}}}</script>"#,
                "lorem ipsum ".repeat(400)
            )
        })
        .collect();
    let photos: String = (0..20)
        .map(|i| format!(r#"{{"media":{{"__typename":"Photo","id":"p{i}","viewer_image":{{"uri":"https://scontent.example/p{i}.jpg","width":1080,"height":1350}}}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let target = format!(
        r#"<script type="application/json" data-sjs>{{"__bbox":{{"result":{{"data":{{"node_v2":{{"post_id":"target","url":"https://www.facebook.com/page/posts/target","attachments":[{{"styles":{{"attachment":{{"all_subattachments":{{"count":20,"nodes":[{photos}]}}}}}}}}]}}}}}}}}}}</script>"#
    );
    format!("<html><head><meta property=\"og:title\" content=\"Page\"></head><body>{noise}{target}</body></html>")
}

#[divan::bench]
fn parse_and_extract_story(bencher: divan::Bencher) {
    let html = synthetic_page();
    let target = Target::from_request("/page/posts/target").unwrap();
    bencher.counter(divan::counter::BytesCount::new(html.len())).bench(|| {
        let page = page::parse(black_box(html.as_bytes())).unwrap();
        let (post, _) = build_post(&page, &target, target.key()).unwrap();
        assert_eq!(post.media.len(), 20);
        post
    });
}

#[divan::bench]
fn saved_pages(bencher: divan::Bencher) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let pages: Vec<(Target, Vec<u8>)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let html = std::fs::read(entry.path()).ok()?;
            let target = Target::from_link(page::saved_url(&html)?)?;
            Some((target, html))
        })
        .collect();
    bencher.bench(|| {
        for (target, html) in &pages {
            let page = page::parse(black_box(html)).unwrap();
            let _ = black_box(build_post(&page, target, target.key()));
        }
    });
}
