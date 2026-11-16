//! The page Telegram gets: preview tags in `<head>`, the Instant View article in `<body>`.

use std::borrow::Cow;

use askama::Template;

use crate::extract::{Media, MediaKind, PageType, Post, PostKind, Source, Stage, Stats, Target};
use crate::settings::Settings;

/// Where each media item's file and thumbnail are linked: CDN URLs, `/_/m/` redirects, or a
/// joined `/_/v/` video.
#[derive(Debug, Clone)]
pub struct MediaUrl {
    pub src: String,
    pub poster: Option<String>,
    /// The file's size when it isn't the media's own (a joined 1080p video).
    pub size: Option<(u32, u32)>,
}

impl MediaUrl {
    /// The CDN URLs as they are.
    pub fn direct(media: &Media) -> Self {
        Self { src: media.url.clone(), poster: media.thumbnail.clone(), size: None }
    }
}

pub struct VideoTag<'a> {
    src: &'a str,
    width: Option<u32>,
    height: Option<u32>,
}

pub struct ImageTag<'a> {
    src: &'a str,
    width: Option<u32>,
    height: Option<u32>,
    /// The author's picture: Telegram shows it small, beside the text.
    small: bool,
}

pub struct Figure<'a> {
    video: bool,
    src: &'a str,
    poster: Option<&'a str>,
}

pub struct Article<'a> {
    published: String,
    author: Option<&'a str>,
    heading: &'a str,
    byline: Option<String>,
    /// Escaped HTML built by [`text_to_html`].
    body: String,
    figures: Vec<Figure<'a>>,
    stats: Option<String>,
    about: Option<About<'a>>,
}

/// "About the author" at the end of Instant View, as FxEmbed has.
pub struct About<'a> {
    name: &'a str,
    /// The heading names them already, unless it's a profile, an album or another page's name.
    show_name: bool,
    /// Only the full-size picture: the post's small one (Facebook's is 40 px) would be a blur.
    picture: Option<&'a str>,
    url: Option<&'a str>,
    /// The link without `https://www.`: `facebook.com/someone`.
    url_text: Option<&'a str>,
    bio: Option<&'a str>,
}

/// Writes a link in the post's text: the URL as it is, or a `/_/l/` link on the preview host.
pub type LinkFn<'a> = &'a dyn Fn(&str) -> String;

/// Links as they are, for the playground and the command line.
pub fn direct(url: &str) -> String {
    url.to_string()
}

#[derive(Template)]
#[template(path = "preview.html")]
struct PreviewPage<'a> {
    site_name: &'a str,
    /// "View on Instagram": the site's own name, whatever `og:site_name` says.
    site: &'a str,
    title: &'a str,
    description: String,
    og_type: &'a str,
    image: Option<ImageTag<'a>>,
    video: Option<VideoTag<'a>>,
    original: &'a str,
    article: Option<Article<'a>>,
}

#[derive(Template)]
#[template(path = "unavailable.html")]
struct UnavailablePage<'a> {
    site_name: &'a str,
    site: &'a str,
    original: &'a str,
    reason: &'a str,
}

/// Whether Instant View is on for this post's type.
pub fn instant_view_enabled(post: &Post, settings: &Settings) -> bool {
    let iv = settings.instant_view;
    match post.kind {
        PostKind::Profile => iv.profile,
        PostKind::TextOnly => iv.text,
        PostKind::Media if post.first_video().is_some() => iv.video,
        PostKind::Media if post.photo_count() > 1 => iv.album,
        PostKind::Media => iv.photo,
    }
}

/// What the preview shows; also the playground's mock preview.
#[derive(Debug, Clone)]
pub struct Summary<'a> {
    /// The site, and which item the card shows when there are several: "Instagram · Photo 2 / 5".
    pub site: String,
    pub title: Cow<'a, str>,
    /// The text with its line breaks.
    pub description: String,
    pub image: Option<&'a str>,
    pub image_size: Option<(u32, u32)>,
    /// The image is the author's picture, shown small.
    pub avatar: bool,
    /// Index of the video in `post.media`, if any.
    pub video: Option<usize>,
    pub instant_view: bool,
    /// Counts for Instant View: "👍 1.5K · 💬 12 · 🔁 79".
    pub stats: Option<String>,
}

/// Which item the card shows: the one the link asks for (`img_index`), else the first video, else
/// the first photo.
pub fn shown_item(post: &Post, target: &Target) -> Option<usize> {
    let asked = target.item_index().filter(|i| *i < post.media.len());
    asked
        .or_else(|| post.media.iter().position(|m| m.kind == MediaKind::Video))
        .or_else(|| (!post.media.is_empty()).then_some(0))
}

/// What the card shows; `card` is a picture made for it, which wins (a mosaic of the photos).
pub fn summary<'a>(
    post: &'a Post,
    media: &'a [MediaUrl],
    card: Option<&'a MediaUrl>,
    settings: &'a Settings,
    target: &Target,
) -> Summary<'a> {
    let shown = shown_item(post, target).filter(|_| card.is_none());
    let video = shown.filter(|i| post.media[*i].kind == MediaKind::Video);
    let site_name = settings.site_name(target.source);
    let site = match shown {
        _ if card.is_some() => format!("{site_name} · {} photos", post.media.len()),
        Some(i) if post.media.len() > 1 => {
            let kind = if post.media[i].kind == MediaKind::Video { "Video" } else { "Photo" };
            format!("{site_name} · {kind} {} / {}", i + 1, post.media.len())
        }
        _ => site_name.to_string(),
    };
    let (image, image_size) = match (card, shown) {
        (Some(card), _) => (Some(card.src.as_str()), card.size),
        (None, Some(i)) if post.media[i].kind == MediaKind::Video => (media[i].poster.as_deref(), None),
        (None, Some(i)) => (Some(media[i].src.as_str()), post.media[i].width.zip(post.media[i].height)),
        (None, None) => (post.profile.best_picture(), None),
    };
    Summary {
        site,
        title: title(post, site_name),
        description: truncate(
            &post.text.as_deref().unwrap_or_default().replace("\r\n", "\n"),
            settings.description_length,
        ),
        image,
        image_size,
        avatar: card.is_none() && shown.is_none() && image.is_some(),
        video,
        instant_view: instant_view_enabled(post, settings),
        stats: settings.instant_view_counts.then(|| stats_line(&post.stats)).flatten(),
    }
}

/// Telegram's bold line: who posted ("Author ▸ Group", "Name (@handle)"), or the page's own name
/// for profiles, albums and pages without a post (events, groups).
fn title<'a>(post: &'a Post, site_name: &'a str) -> Cow<'a, str> {
    let named = post.title.as_deref().or(post.author.as_deref()).unwrap_or(site_name);
    let with_handle = |name: &'a str| match post.profile.handle.as_deref() {
        Some(handle) if handle != name => Cow::Owned(format!("{name} (@{handle})")),
        _ => Cow::Borrowed(name),
    };
    if post.kind == PostKind::Profile {
        return with_handle(named);
    }
    if post.page_type == PageType::Album {
        return Cow::Borrowed(named);
    }
    match (post.author.as_deref(), post.group.as_deref()) {
        (Some(author), Some(group)) => Cow::Owned(format!("{author} ▸ {group}")),
        (Some(author), None) => with_handle(author),
        (None, _) => Cow::Borrowed(named),
    }
}

/// "👍 1.5K · 💬 12 · 🔁 79 · 👁️ 3.9K", leaving out what's unknown or zero.
fn stats_line(stats: &Stats) -> Option<String> {
    let counts = [("👍", stats.reactions), ("💬", stats.comments), ("🔁", stats.shares), ("👁️", stats.views)];
    let parts: Vec<String> = counts
        .into_iter()
        .filter_map(|(icon, count)| count.filter(|n| *n > 0).map(|n| format!("{icon} {}", compact(n))))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A count as Facebook and Instagram write it: 950, 1.5K, 12K, 3.9M (cut, not rounded).
pub fn compact(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")];
    let Some(&(size, unit)) = UNITS.iter().find(|(size, _)| n >= *size) else { return n.to_string() };
    let tenths = n / (size / 10);
    let (whole, tenth) = (tenths / 10, tenths % 10);
    if tenth == 0 || whole >= 10 { format!("{whole}{unit}") } else { format!("{whole}.{tenth}{unit}") }
}

/// Renders the preview page for `target`. `media` holds one entry per `post.media` item.
pub fn preview(post: &Post, media: &[MediaUrl], settings: &Settings, target: &Target) -> String {
    preview_with(post, media, None, settings, target, &direct)
}

/// [`preview`], with the card's picture given (a mosaic of the photos), and writing the text's
/// links with `link`.
pub fn preview_with(
    post: &Post,
    media: &[MediaUrl],
    card: Option<&MediaUrl>,
    settings: &Settings,
    target: &Target,
    link: LinkFn<'_>,
) -> String {
    let summary = summary(post, media, card, settings, target);
    let original = target.original.as_str();
    let video = summary.video.map(|i| VideoTag {
        src: &media[i].src,
        width: media[i].size.map(|s| s.0).or(post.media[i].width),
        height: media[i].size.map(|s| s.1).or(post.media[i].height),
    });
    let image = summary.image.map(|src| ImageTag {
        src,
        width: summary.image_size.map(|s| s.0),
        height: summary.image_size.map(|s| s.1),
        small: summary.avatar,
    });
    // With Instant View on, Telegram drops the description's line breaks unless they're `<br>`.
    let description =
        if summary.instant_view { summary.description.replace('\n', "<br>") } else { summary.description.clone() };

    let article = summary.instant_view.then(|| {
        let published_at = post.published_at.unwrap_or_else(|| jiff::Timestamp::now().as_second());
        let published = jiff::Timestamp::from_second(published_at).unwrap_or_default();
        let date = post
            .published_at
            .and_then(|t| jiff::Timestamp::from_second(t).ok())
            .map(|t| t.strftime("%-d %B %Y").to_string());
        // The heading names the author already, except on profiles, albums and other pages.
        let author = post.author.as_deref().filter(|a| !summary.title.contains(a));
        let byline = [author.map(str::to_string), date].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        Article {
            published: published.to_string(),
            author: post.author.as_deref(),
            heading: &summary.title,
            byline: (!byline.is_empty()).then_some(byline),
            body: text_to_html(post.text.as_deref().unwrap_or_default(), target.source, link),
            figures: post
                .media
                .iter()
                .zip(media)
                .map(|(m, url)| Figure {
                    video: m.kind == MediaKind::Video,
                    src: &url.src,
                    poster: url.poster.as_deref(),
                })
                .collect(),
            stats: summary.stats.clone(),
            // Only with something to add to the name the heading shows.
            about: post
                .author
                .as_deref()
                .map(|name| {
                    let profile = &post.profile;
                    About {
                        name,
                        show_name: !summary.title.contains(name),
                        picture: profile.picture.as_deref(),
                        url: profile.url.as_deref(),
                        url_text: profile.url.as_deref().map(|u| u.trim_start_matches("https://www.")),
                        bio: profile.bio.as_deref(),
                    }
                })
                .filter(|about| about.picture.is_some() || about.url.is_some() || about.bio.is_some()),
        }
    });

    PreviewPage {
        site_name: &summary.site,
        site: target.source.name(),
        title: &summary.title,
        description,
        og_type: if video.is_some() { "video.other" } else { "article" },
        image,
        video,
        original,
        article,
    }
    .render()
    .expect("rendering to a String can't fail")
}

/// The "couldn't load" card, saying why: Telegram keeps it, so it may as well be useful.
/// `stage` is `None` when the site's budget ran out.
pub fn unavailable(settings: &Settings, target: &Target, stage: Option<Stage>) -> String {
    let site = target.source.name();
    let reason = match stage {
        Some(Stage::LoginWall) => format!("{site} shows this only to people who are logged in."),
        Some(Stage::NotFound) => match target.source {
            Source::Facebook => {
                "Facebook doesn't show this post publicly: it may be private, deleted or for friends only.".into()
            }
            Source::Instagram => {
                "Instagram doesn't show this publicly: it may be private, age-restricted or deleted.".into()
            }
        },
        Some(Stage::RateLimited) => format!("{site} asked us to slow down for a while."),
        _ => format!("{site} didn't answer in time."),
    };
    UnavailablePage {
        site_name: settings.site_name(target.source),
        site,
        original: target.original.as_str(),
        reason: &reason,
    }
    .render()
    .expect("rendering to a String can't fail")
}

/// Cuts text to `max` characters, ending with an ellipsis when cut.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max.saturating_sub(1)) {
        Some((index, _)) if text[index..].chars().nth(1).is_some() => format!("{}…", text[..index].trim_end()),
        _ => text.to_string(),
    }
}

/// Paragraphs on blank lines, line breaks within them, clickable http(s) links, hashtags and (on
/// Instagram) `@names`; all escaped.
pub fn text_to_html(text: &str, source: Source, link: LinkFn<'_>) -> String {
    let mut html = String::new();
    let normalized = text.replace("\r\n", "\n");
    for paragraph in normalized.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        html.push_str("<p>");
        for (i, line) in paragraph.lines().enumerate() {
            if i > 0 {
                html.push_str("<br>");
            }
            linkify(line, source, link, &mut html);
        }
        html.push_str("</p>\n");
    }
    html
}

fn linkify(line: &str, source: Source, link_to: LinkFn<'_>, out: &mut String) {
    let mut rest = line;
    while let Some(start) = find_link(rest) {
        tags_into(&rest[..start], source, link_to, out);
        let link_end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |e| start + e);
        let mut link = &rest[start..link_end];
        // Trailing punctuation belongs to the sentence, not the link.
        while let Some(last) = link.chars().last()
            && matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'')
        {
            link = &link[..link.len() - last.len_utf8()];
        }
        out.push_str("<a href=\"");
        escape_into(&link_to(link), out);
        out.push_str("\">");
        escape_into(link, out);
        out.push_str("</a>");
        rest = &rest[start + link.len()..];
    }
    tags_into(rest, source, link_to, out);
}

/// `#tag` at the start of a word links to the site's page for it, and on Instagram `@name` to
/// that account; `#1`, `a#b` and `me@example.com` stay text.
fn tags_into(text: &str, source: Source, link_to: LinkFn<'_>, out: &mut String) {
    let marks: &[char] = if source == Source::Instagram { &['#', '@'] } else { &['#'] };
    let mut rest = text;
    while let Some(at) = rest.find(marks) {
        let mark = rest[at..].chars().next().unwrap_or('#');
        let after = &rest[at + 1..];
        let allowed = |c: char| c.is_alphanumeric() || c == '_' || (mark == '@' && c == '.');
        let name = after[..after.find(|c: char| !allowed(c)).unwrap_or(after.len())].trim_end_matches('.');
        let starts_word = rest[..at].chars().last().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '['));
        if !starts_word || name.is_empty() || (mark == '#' && name.chars().all(|c| c.is_ascii_digit())) {
            escape_into(&rest[..=at], out);
            rest = after;
            continue;
        }
        escape_into(&rest[..at], out);
        let escaped: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
        let page = match (source, mark) {
            (Source::Instagram, '@') => format!("https://www.instagram.com/{escaped}/"),
            (Source::Instagram, _) => format!("https://www.instagram.com/explore/tags/{escaped}/"),
            (Source::Facebook, _) => format!("https://www.facebook.com/hashtag/{escaped}"),
        };
        out.push_str("<a href=\"");
        escape_into(&link_to(&page), out);
        out.push_str("\">");
        out.push(mark);
        escape_into(name, out);
        out.push_str("</a>");
        rest = &after[name.len()..];
    }
    escape_into(rest, out);
}

/// Where the first `http://` or `https://` starting a word begins. One pass, whatever the post's
/// author put in it.
fn find_link(text: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let start = from + text[from..].find("http")?;
        let after = &text[start + 4..];
        let starts_word = start == 0 || text[..start].ends_with(|c: char| c.is_whitespace() || c == '(');
        if starts_word && (after.starts_with("://") || after.starts_with("s://")) {
            return Some(start);
        }
        from = start + 4;
    }
}

pub fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::fake::FakeExtractor;
    use crate::extract::{AuthorProfile, Media};

    fn html_of(text: &str) -> String {
        text_to_html(text, Source::Facebook, &super::direct)
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
    fn text_posts() {
        let mut post = album();
        post.media.clear();
        post.kind = PostKind::TextOnly;
        post.published_at = None;
        post.author = None;
        let html = render(&post, &Settings::default());
        assert!(html.contains("<meta property=\"og:description\" content=\"Hello &#60;world&#62;"));
        assert!(html.contains("article:published_time"), "the Medium route needs a time even when unknown");
        assert!(!html.contains("og:image"));

        // With the author's picture, a small card shows it.
        post.profile.avatar = Some("https://scontent.xx.fbcdn.net/v/avatar.jpg".into());
        let html = render(&post, &Settings::default());
        assert!(html.contains(r#"<meta property="og:image" content="https://scontent.xx.fbcdn.net/v/avatar.jpg">"#));
        assert!(html.contains(r#"<meta name="twitter:card" content="summary">"#));
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("héllo world", 5), "héll…");
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("  hi  ", 5), "hi");
        assert_eq!(truncate("ab cd", 4), "ab…");
    }

    #[test]
    fn text_to_html_escapes_and_links_urls_and_hashtags() {
        assert_eq!(
            html_of("a <b> & c\nline 2 (see https://x.com/a?b=1&c=2).\n\n\nnext"),
            "<p>a &lt;b&gt; &amp; c<br>line 2 (see <a href=\"https://x.com/a?b=1&amp;c=2\">https://x.com/a?b=1&amp;c=2</a>).</p>\n<p>next</p>\n"
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
            html_of("Đà Lạt #dulịch (#cafe_paro) #1 a#b #"),
            "<p>Đà Lạt <a href=\"https://www.facebook.com/hashtag/dul%E1%BB%8Bch\">#dulịch</a> \
             (<a href=\"https://www.facebook.com/hashtag/cafe_paro\">#cafe_paro</a>) #1 a#b #</p>\n"
        );
        assert_eq!(
            html_of("https://x.com/a#frag #tag"),
            "<p><a href=\"https://x.com/a#frag\">https://x.com/a#frag</a> \
             <a href=\"https://www.facebook.com/hashtag/tag\">#tag</a></p>\n"
        );
    }

    #[test]
    fn instagram_links_names_and_tags() {
        assert_eq!(
            text_to_html("Hi @nasa.gov, @ me@example.com #space #1.", Source::Instagram, &super::direct),
            "<p>Hi <a href=\"https://www.instagram.com/nasa.gov/\">@nasa.gov</a>, @ me@example.com \
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
            html.contains(&format!(
                r#"<meta property="og:image" content="{}""#,
                post.media[1].url.replace('&', "&amp;")
            ))
        );
        assert!(html.contains(">View on Instagram</a>"));
        let gone = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK/?img_index=9").unwrap();
        let first = preview(&post, &direct(&post), &Settings::default(), &gone);
        assert!(first.contains("Instagram · Photo 1 / 2"), "an item the post doesn't have shows the first");
        let card = unavailable(&Settings::default(), &target, Some(Stage::RateLimited));
        assert!(card.contains("Instagram asked us to slow down") && card.contains("View on Instagram"));
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
        assert!(
            html.contains(r#"<meta property="og:image" content="https://ig.example.com/_/g/0123456789abcdef.jpg">"#)
        );
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
        assert!(html.contains("<p>👍 1.5K · 🔁 79</p>"), "zero and unknown counts are left out");
        assert!(!html.contains("og:description\" content=\"1/2"), "the photo count moved to the site name");
        let quiet = Settings { instant_view_counts: false, ..Settings::default() };
        assert!(!render(&post, &quiet).contains("👍"));
    }

    #[test]
    fn line_breaks_survive_in_the_card() {
        let post = album();
        let with_iv = render(&post, &Settings::default());
        assert!(with_iv.contains("Hello &#60;world&#62;&#60;br&#62;&#60;br&#62;Second paragraph"));
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
        let about = &html[html.find("<h2>About the author</h2>").expect("the author block")..];
        assert!(
            about
                .contains(r#"<img src="https://scontent.xx.fbcdn.net/v/large.jpg" alt="Some Page's profile picture">"#),
            "{about}"
        );
        assert!(about.contains(r#"<a href="https://www.facebook.com/somepage">facebook.com/somepage</a>"#));
        assert!(about.contains("<b>Cats &#38; coffee</b>"));
        post.profile.picture = None;
        assert!(!render(&post, &Settings::default()).contains("small.jpg\" alt"), "never the 40 px picture there");
    }
}
