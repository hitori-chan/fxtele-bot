//! Previews as each site's posts come out.

use super::site::Style;
use super::text::text_to_html;
use super::*;
use crate::extract::fake::FakeExtractor;
use crate::extract::{AuthorProfile, Media};

fn html_of(text: &str) -> String {
    text_to_html(text, &Style::of(Source::Facebook).text, &super::direct)
}

fn direct(post: &Post) -> Vec<MediaUrl> {
    post.media.iter().map(MediaUrl::direct).collect()
}

fn render(post: &Post, settings: &Settings) -> String {
    let target = Target::from_link("https://www.facebook.com/somepage/posts/1?x=1&y=2").unwrap();
    preview(post, &direct(post), settings, &target)
}

fn album() -> Post {
    FakeExtractor::post(&Target::from_request(Source::Facebook, "/somepage/posts/1").unwrap())
}

#[test]
fn album_page() {
    insta::assert_snapshot!(render(&album(), &Settings::default()));
}

#[test]
fn a_share_shows_the_shared_post_after_the_sharers_words() {
    let mut post = album();
    post.text = Some("Look".into());
    post.media.truncate(1);
    post.shared = Some(Shared {
        url: Some("https://www.facebook.com/page/posts/9".into()),
        author: Some("Other Page".into()),
        text: Some("Their words".into()),
        ..Shared::default()
    });
    let html = render(&post, &Settings::default());
    assert!(
        html.contains("content=\"Look&#60;br&#62;&#60;br&#62;↘️ Shared from Other Page:&#60;br&#62;Their words\""),
        "{html}"
    );
    let at = |part: &str| html.find(part).unwrap_or_else(|| panic!("no {part} in {html}"));
    let heading = r#"<h3><a href="https://www.facebook.com/page/posts/9">↘️ Shared from Other Page</a></h3>"#;
    assert!(
        at("<p>Look</p>") < at(heading) && at(heading) < at("<img src") && at("<img src") < at("<p>Their words</p>")
    );

    // Nothing of the sharer's own: the shared post alone.
    post.text = None;
    post.shared.as_mut().unwrap().author = None;
    assert!(render(&post, &Settings::default()).contains("content=\"↘️ Shared post:&#60;br&#62;Their words\""));
}

#[test]
fn reddit_titles_with_the_headline_and_reads_markdown() {
    let target = Target::from_link("https://www.reddit.com/r/rust/comments/1abc2d3/").unwrap();
    let mut post = album();
    (post.title, post.author, post.group) = (Some("Big news".into()), Some("u/someone".into()), Some("r/rust".into()));
    post.text = Some("**Rust 2.0** is out, see r/rust\n\n- fast\n- safe".into());
    post.media.truncate(1);
    let html = preview(&post, &direct(&post), &Settings::default(), &target);
    assert!(html.contains(r#"<meta property="og:title" content="Big news">"#), "{html}");
    assert!(html.contains(r#"<meta property="og:site_name" content="Reddit · u/someone · r/rust">"#), "{html}");
    assert!(
        html.contains("content=\"Rust 2.0 is out, see r/rust&#60;br&#62;&#60;br&#62;• fast&#60;br&#62;• safe\""),
        "{html}"
    );
    assert!(html.contains("<p><i>u/someone in r/rust</i></p>"), "a byline under the headline: {html}");
    assert!(html.contains(r#"<p><b>Rust 2.0</b> is out, see <a href="https://www.reddit.com/r/rust/">r/rust</a></p>"#));
    assert!(html.contains("<li>safe</li>"));

    post.shared =
        Some(Shared { author: Some("u/op in r/rust".into()), relation: Relation::Reply, ..Shared::default() });
    assert!(preview(&post, &direct(&post), &Settings::default(), &target).contains("↘️ Replying to u/op in r/rust"));

    // NSFW or a spoiler: the card leaves the photo out, Instant View has it.
    post.sensitive = true;
    let html = preview(&post, &direct(&post), &Settings::default(), &target);
    let photo = post.media[0].url.replace('&', "&amp;");
    assert!(!html.contains(&format!(r#"og:image" content="{photo}""#)), "{html}");
    assert!(html.contains(&format!(r#"<img src="{photo}">"#)), "{html}");
}

#[test]
fn photos_follow_each_other_without_figures() {
    let html = render(&album(), &Settings::default());
    let article = &html[html.find("<article>").unwrap()..];
    assert!(!article.contains("<figure"), "{article}");
    assert!(article.contains("\"><img src="), "one right after the other: {article}");
}

#[test]
fn video_page_without_instant_view() {
    let mut post = album();
    post.media = vec![Media::video(
        "https://video.xx.fbcdn.net/v/clip.mp4?oe=7FFFFFFF".into(),
        Some("https://scontent.xx.fbcdn.net/v/thumb.jpg".into()),
    )];
    post.media[0].width = Some(720);
    post.media[0].height = Some(1280);
    let mut settings = Settings::default();
    settings.instant_view.video = false;
    insta::assert_snapshot!(render(&post, &settings));
}

#[test]
fn discord_gets_a_player_its_colour_and_no_instant_view() {
    let mut post = album();
    post.media = vec![Media::video("https://video.xx.fbcdn.net/v/clip.mp4".into(), Some("https://x/t.jpg".into()))];
    post.media[0].width = Some(360);
    post.media[0].height = Some(360);
    let target = Target::from_link("https://www.facebook.com/reel/1").unwrap();
    let settings = Settings::default();
    let html = discord_page(&post, &direct(&post), None, &settings, &target, |_| Some("https://o.example/x".into()));
    assert!(html.contains(r#"<meta name="twitter:card" content="player">"#), "{html}");
    assert!(html.contains(r#"<meta name="twitter:player:stream" content="https://video.xx.fbcdn.net/v/clip.mp4">"#));
    assert!(html.contains(r#"<meta property="og:video:width" content="720">"#), "small videos doubled");
    assert!(html.contains(r##"<meta name="theme-color" content="#0866ff">"##));
    assert!(html.contains(r#"<link rel="alternate" type="application/json+oembed" href="https://o.example/x">"#));
    assert!(!html.contains("al:android:app_name") && !html.contains("<article>"), "no Instant View");
    assert!(!html.contains("<br>"), "the description keeps its line breaks");

    // Discord leaves a video's description out, so its text goes above the title.
    let media = direct(&post);
    let summary = summary(&post, &media, None, &settings, &target);
    let (provider, author) = discord_lines(&summary);
    assert_eq!(
        (provider.as_str(), author.as_deref().map(|a| a.starts_with("Hello <world>"))),
        ("Facebook", Some(true))
    );
    // Telegram's page has none of it.
    let telegram = preview(&post, &direct(&post), &settings, &target);
    assert!(!telegram.contains("theme-color") && !telegram.contains("player") && !telegram.contains("oembed"));
}

#[test]
fn text_posts() {
    let mut post = album();
    post.media.clear();
    post.kind = PostKind::TextOnly;
    post.published_at = None;
    post.author = None;
    let html = render(&post, &Settings::default());
    assert!(html.contains("<meta property=\"og:description\" content=\"Hello &#38;lt;world&#38;gt;"), "{html}");
    assert!(html.contains("article:published_time"), "the Medium route needs a time even when unknown");
    assert!(!html.contains("og:image"));

    // With the author's picture, a small card shows it.
    post.profile.avatar = Some("https://scontent.xx.fbcdn.net/v/avatar.jpg".into());
    let html = render(&post, &Settings::default());
    assert!(html.contains(r#"<meta property="og:image" content="https://scontent.xx.fbcdn.net/v/avatar.jpg">"#));
    assert!(html.contains(r#"<meta name="twitter:card" content="summary">"#));
}

/// The same posts on every site, side by side: whatever differs here is meant to.
#[test]
fn every_site_side_by_side() {
    let sites = [
        (Source::Facebook, "https://www.facebook.com/somepage/posts/1", "https://www.facebook.com/somepage"),
        (Source::Instagram, "https://www.instagram.com/p/DfSi56IFwZK/", "https://www.instagram.com/someone/"),
        (Source::Reddit, "https://www.reddit.com/r/rust/comments/1abc2d3/", "https://www.reddit.com/user/someone/"),
        (Source::Threads, "https://www.threads.com/@someone/post/DfaNgswkQPG", "https://www.threads.com/@someone"),
    ];
    let profile = AuthorProfile {
        url: Some("https://www.example.com/someone".into()),
        avatar: Some("https://cdn.example.com/small.jpg".into()),
        picture: Some("https://cdn.example.com/large.jpg".into()),
        bio: Some("Cats & coffee".into()),
        handle: Some("someone".into()),
    };
    let settings = Settings::default();
    let mut pages = String::new();
    for (source, post_link, profile_link) in sites {
        let target = Target::from_link(post_link).unwrap();
        let mut post = FakeExtractor::post(&target);
        post.title = Some("A headline".into());
        post.author = Some("Some One".into());
        post.group = Some("Night Owls".into());
        post.profile = profile.clone();
        post.published_at = Some(1_700_000_000);
        post.text = Some("Hi @some.one #tag r/rust u/someone https://example.com/a?b=1&c=2 <b>\n\n**Bold**".into());
        post.stats = Stats { reactions: Some(1504), comments: Some(12), shares: Some(79), views: Some(3900) };
        let mut write = |shape: &str, html: String| {
            pages.push_str(&format!("===== {} {shape}\n{html}\n", source.as_str()));
        };
        write("album", preview(&post, &direct(&post), &settings, &target));
        write("discord", discord_page(&post, &direct(&post), None, &settings, &target, |_| None));

        let mut shared = post.clone();
        shared.media.truncate(1);
        shared.shared = Some(Shared {
            url: Some("https://www.example.com/shared".into()),
            title: Some("Their headline".into()),
            author: Some("Other One".into()),
            text: Some("Their words".into()),
            relation: Relation::Quote,
            ..Shared::default()
        });
        write("share", preview(&shared, &direct(&shared), &settings, &target));

        let mut text = post.clone();
        text.media.clear();
        text.kind = PostKind::TextOnly;
        write("text", preview(&text, &direct(&text), &settings, &target));

        let page = Target::from_link(profile_link).unwrap();
        let mut account = post;
        account.media.truncate(1);
        (account.kind, account.page_type) = (PostKind::Profile, PageType::Profile);
        write("profile", preview(&account, &direct(&account), &settings, &page));

        let stages = [
            Some(Stage::LoginWall),
            Some(Stage::NotFound),
            Some(Stage::RateLimited),
            Some(Stage::Redirect),
            Some(Stage::Fetch),
            Some(Stage::Timeout),
            None,
        ];
        for stage in stages {
            write(&format!("unavailable {stage:?}"), unavailable(&settings, &target, stage));
        }
    }
    insta::assert_snapshot!(pages);
}

#[test]
fn truncates_by_characters() {
    assert_eq!(truncate("héllo world", 5), "héll…");
    assert_eq!(truncate("héllo", 5), "héllo");
    assert_eq!(truncate("  hi  ", 5), "hi");
    assert_eq!(truncate("ab cd", 4), "ab…");
    // Never half a character as it's seen.
    assert_eq!(truncate("ok 🇻🇳🇯🇵", 5), "ok…", "a flag is two code points");
    assert_eq!(truncate("e\u{301}e\u{301}e\u{301}", 4), "e\u{301}…", "a letter and its accent");
    assert_eq!(truncate("a👩\u{200d}💻 b", 4), "a…", "an emoji joined from three");
}

#[test]
fn text_to_html_escapes_and_links_urls_and_hashtags() {
    assert_eq!(
        html_of("a <b> & c\nline 2 (see https://example.com/a?b=1&c=2).\n\n\nnext"),
        "<p>a &lt;b&gt; &amp; c<br>line 2 (see <a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a>).</p>\n<p>next</p>\n"
    );
    assert_eq!(html_of("xhttps://no"), "<p>xhttps://no</p>\n");
    assert_eq!(html_of("\"><script>"), "<p>&quot;&gt;&lt;script&gt;</p>\n");
    assert_eq!(
        html_of("xhttp://a xhttps://b http://c"),
        "<p>xhttp://a xhttps://b <a href=\"http://c\">http://c</a></p>\n"
    );
    // Any number of links that don't start a word, without a deep stack.
    let many = "xhttp://".repeat(100_000);
    assert_eq!(html_of(&many), format!("<p>{many}</p>\n"));

    // Hashtags link to Facebook's; a link's fragment isn't one.
    assert_eq!(
        html_of("Đà Lạt #dulịch (#cafe_paro) #1 a#b a##b #"),
        "<p>Đà Lạt <a href=\"https://www.facebook.com/hashtag/dul%E1%BB%8Bch\">#dulịch</a> \
         (<a href=\"https://www.facebook.com/hashtag/cafe_paro\">#cafe_paro</a>) #1 a#b a##b #</p>\n"
    );
    assert_eq!(
        html_of("https://example.com/a#frag #tag"),
        "<p><a href=\"https://example.com/a#frag\">https://example.com/a#frag</a> \
         <a href=\"https://www.facebook.com/hashtag/tag\">#tag</a></p>\n"
    );
    // Nor `r/name` inside a word, once the letter before it was passed over.
    let mut reddit = String::new();
    linkify_reddit("ur/rust uu/x r/rust", &super::direct, &mut reddit);
    assert_eq!(reddit, "ur/rust uu/x <a href=\"https://www.reddit.com/r/rust/\">r/rust</a>");
}

#[test]
fn instagram_links_names_and_tags() {
    assert_eq!(
        text_to_html("Hi @nasa.gov, @ me@@example.com #space #1.", &Style::of(Source::Instagram).text, &super::direct),
        "<p>Hi <a href=\"https://www.instagram.com/nasa.gov/\">@nasa.gov</a>, @ me@@example.com \
         <a href=\"https://www.instagram.com/explore/tags/space/\">#space</a> #1.</p>\n"
    );
    assert!(html_of("@nasa").contains("<p>@nasa</p>"), "Facebook doesn't link names that way");
}

#[test]
fn instagram_cards_show_the_item_asked_for() {
    let target = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK/?img_index=2").unwrap();
    let mut post = FakeExtractor::post(&target);
    post.author = Some("NASA".into());
    post.profile.handle = Some("nasa".into());
    let html = preview(&post, &direct(&post), &Settings::default(), &target);
    assert!(html.contains(r#"<meta property="og:site_name" content="Instagram · Photo 2 / 2">"#), "{html}");
    assert!(html.contains(r#"<meta property="og:title" content="NASA (@nasa)">"#));
    assert!(
        html.contains(&format!(r#"<meta property="og:image" content="{}""#, post.media[1].url.replace('&', "&amp;")))
    );
    assert!(html.contains(">View on Instagram</a>"));
    let gone = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK/?img_index=9").unwrap();
    let first = preview(&post, &direct(&post), &Settings::default(), &gone);
    assert!(first.contains("Instagram · Photo 1 / 2"), "an item the post doesn't have shows the first");
    let card = unavailable(&Settings::default(), &target, Some(Stage::RateLimited));
    assert!(card.contains("Instagram asked us to slow down") && card.contains("View on Instagram"));
    let says = |stage| unavailable(&Settings::default(), &target, stage);
    assert!(says(Some(Stage::Redirect)).contains("lead to a post on Instagram."));
    assert!(says(None).contains("Too many Instagram links at once"));
}

#[test]
fn a_mosaic_is_the_cards_picture() {
    let target = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK/").unwrap();
    let post = FakeExtractor::post(&target);
    let card = MediaUrl {
        src: "https://ig.example.com/_/g/0123456789abcdef.jpg".into(),
        poster: None,
        size: Some((2010, 1250)),
    };
    let html = preview_with(&post, &direct(&post), Some(&card), &Settings::default(), &target, &super::direct);
    assert!(html.contains(r#"<meta property="og:site_name" content="Instagram · 2 photos">"#), "{html}");
    assert!(html.contains(r#"<meta property="og:image" content="https://ig.example.com/_/g/0123456789abcdef.jpg">"#));
    assert!(html.contains(r#"<meta property="og:image:width" content="2010">"#));
    assert!(html.contains(r#"content="summary_large_image""#), "a big card, not the author's small one");
}

#[test]
fn counts_read_like_facebooks() {
    assert_eq!(compact(950), "950");
    assert_eq!(compact(1504), "1.5K");
    assert_eq!(compact(5890), "5.8K", "cut, not rounded");
    assert_eq!(compact(12_027), "12K");
    assert_eq!(compact(999_999), "999K");
    assert_eq!(compact(2_380_000), "2.3M");
    assert_eq!(compact(1_000_000_000), "1B");
}

#[test]
fn card_names_the_author_and_counts_go_in_instant_view() {
    let mut post = album();
    post.title = Some("Some Page - Hello".into());
    post.group = Some("Night Owls".into());
    post.stats = Stats { reactions: Some(1504), comments: Some(0), shares: Some(79), views: None };
    let html = render(&post, &Settings::default());
    assert!(html.contains(r#"<meta property="og:title" content="Some Page ▸ Night Owls">"#));
    assert!(html.contains(r#"<meta property="og:site_name" content="Facebook · Photo 1 / 2">"#));
    assert!(html.contains("<p>🔁 79 👍 1.5K</p>"), "zero and unknown counts are left out");
    assert!(!html.contains("og:description\" content=\"1/2"), "the photo count moved to the site name");
    let quiet = Settings { instant_view_counts: false, ..Settings::default() };
    assert!(!render(&post, &quiet).contains("👍"));
}

#[test]
fn line_breaks_survive_in_the_card() {
    let post = album();
    let with_iv = render(&post, &Settings::default());
    assert!(
        with_iv.contains("Hello &#38;lt;world&#38;gt;&#60;br&#62;&#60;br&#62;Second paragraph"),
        "read as HTML, the text's own <world> is escaped: {with_iv}"
    );
    let mut settings = Settings::default();
    settings.instant_view.album = false;
    assert!(render(&post, &settings).contains("Hello &#60;world&#62;\n\nSecond paragraph"));
}

#[test]
fn instant_view_ends_with_the_author() {
    let mut post = album();
    post.profile = AuthorProfile {
        url: Some("https://www.facebook.com/somepage".into()),
        avatar: Some("https://scontent.xx.fbcdn.net/v/small.jpg".into()),
        picture: Some("https://scontent.xx.fbcdn.net/v/large.jpg".into()),
        bio: Some("Cats & coffee".into()),
        handle: None,
    };
    let html = render(&post, &Settings::default());
    let about = &html[html.find("<hr>\n<h2>About the author</h2>").expect("the author block")..];
    assert!(
        about.contains(r#"<img src="https://scontent.xx.fbcdn.net/v/large.jpg" alt="Some Page's profile picture">"#),
        "{about}"
    );
    assert!(about.contains("<h2>Some Page</h2>"), "named even when the heading has it, as FxEmbed does");
    assert!(about.contains(r#"<a href="https://www.facebook.com/somepage">facebook.com/somepage</a>"#));
    assert!(about.contains("<b>Cats &#38; coffee</b>"));
    assert!(about.trim_end().ends_with("View on Facebook</a></p>\n</article>\n</body>\n</html>"), "{about}");
    assert!(!html.contains("<address>") && !html.contains("article:author"), "Telegram prints the date itself");
    post.profile.handle = Some("somepage".into());
    assert!(render(&post, &Settings::default()).contains(">@somepage</a></p>"));
    post.profile.picture = None;
    assert!(!render(&post, &Settings::default()).contains("small.jpg\" alt"), "never the 40 px picture there");
}
