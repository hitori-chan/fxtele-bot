//! The page Telegram gets: preview tags in `<head>`, the Instant View article in `<body>`.

use std::borrow::Cow;

use askama::Template;

use crate::extract::facebook::url::PageType;
use crate::extract::{Media, MediaKind, Post, PostKind, Stage, Stats};
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
    /// Facebook's CDN URLs as they are.
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
    /// Only the full-size picture: Facebook's 40 px one would be a blur at that width.
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
    /// The site, and which item the card shows when there are several: "Facebook · Photo 1 / 5".
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

pub fn summary<'a>(post: &'a Post, media: &'a [MediaUrl], settings: &'a Settings) -> Summary<'a> {
    let video = post.media.iter().position(|m| m.kind == MediaKind::Video);
    // The card shows the first video, else the first photo.
    let shown = video.or_else(|| (!post.media.is_empty()).then_some(0));
    let site = match shown {
        Some(i) if post.media.len() > 1 => {
            let kind = if post.media[i].kind == MediaKind::Video { "Video" } else { "Photo" };
            format!("{} · {kind} {} / {}", settings.site_name, i + 1, post.media.len())
        }
        _ => settings.site_name.clone(),
    };
    let (image, image_size) = match shown {
        Some(i) if post.media[i].kind == MediaKind::Video => (media[i].poster.as_deref(), None),
        Some(i) => (Some(media[i].src.as_str()), post.media[i].width.zip(post.media[i].height)),
        None => (post.profile.best_picture(), None),
    };
    Summary {
        site,
        title: title(post, settings),
        description: truncate(
            &post.text.as_deref().unwrap_or_default().replace("\r\n", "\n"),
            settings.description_length,
        ),
        image,
        image_size,
        avatar: shown.is_none() && image.is_some(),
        video,
        instant_view: instant_view_enabled(post, settings),
        stats: settings.instant_view_counts.then(|| stats_line(&post.stats)).flatten(),
    }
}

/// Telegram's bold line: who posted ("Author ▸ Group"), or the page's own name for profiles,
/// albums and pages without a post (events, groups).
fn title<'a>(post: &'a Post, settings: &'a Settings) -> Cow<'a, str> {
    let named = || Cow::Borrowed(post.title.as_deref().or(post.author.as_deref()).unwrap_or(&settings.site_name));
    if post.kind == PostKind::Profile || post.page_type == PageType::Album {
        return named();
    }
    match (post.author.as_deref(), post.group.as_deref()) {
        (Some(author), Some(group)) => Cow::Owned(format!("{author} ▸ {group}")),
        (Some(author), None) => Cow::Borrowed(author),
        (None, _) => named(),
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

/// A count as Facebook writes it: 950, 1.5K, 12K, 3.9M (cut, not rounded, as Facebook does).
pub fn compact(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")];
    let Some(&(size, unit)) = UNITS.iter().find(|(size, _)| n >= *size) else { return n.to_string() };
    let tenths = n / (size / 10);
    let (whole, tenth) = (tenths / 10, tenths % 10);
    if tenth == 0 || whole >= 10 { format!("{whole}{unit}") } else { format!("{whole}.{tenth}{unit}") }
}

/// Renders the preview page. `media` holds one entry per `post.media` item.
pub fn preview(post: &Post, media: &[MediaUrl], settings: &Settings, original: &str) -> String {
    preview_with(post, media, settings, original, &direct)
}

/// [`preview`], writing the text's links with `link`.
pub fn preview_with(post: &Post, media: &[MediaUrl], settings: &Settings, original: &str, link: LinkFn<'_>) -> String {
    let summary = summary(post, media, settings);
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
            body: text_to_html(post.text.as_deref().unwrap_or_default(), link),
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
/// `stage` is `None` when Facebook's budget ran out.
pub fn unavailable(settings: &Settings, original: &str, stage: Option<Stage>) -> String {
    let reason = match stage {
        Some(Stage::LoginWall) => "Facebook shows this post only to people who are logged in.",
        Some(Stage::NotFound) => {
            "Facebook doesn't show this post publicly: it may be private, deleted or for friends only."
        }
        _ => "Facebook didn't answer in time.",
    };
    UnavailablePage { site_name: &settings.site_name, original, reason }
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

/// Paragraphs on blank lines, line breaks within them, clickable http(s) links and hashtags; all
/// escaped.
pub fn text_to_html(text: &str, link: LinkFn<'_>) -> String {
    let mut html = String::new();
    let normalized = text.replace("\r\n", "\n");
    for paragraph in normalized.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        html.push_str("<p>");
        for (i, line) in paragraph.lines().enumerate() {
            if i > 0 {
                html.push_str("<br>");
            }
            linkify(line, link, &mut html);
        }
        html.push_str("</p>\n");
    }
    html
}

fn linkify(line: &str, link_to: LinkFn<'_>, out: &mut String) {
    let mut rest = line;
    while let Some(start) = find_link(rest) {
        hashtags_into(&rest[..start], link_to, out);
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
    hashtags_into(rest, link_to, out);
}

/// `#tag` at the start of a word links to Facebook's page for it; `#1` and `a#b` stay text.
fn hashtags_into(text: &str, link_to: LinkFn<'_>, out: &mut String) {
    let mut rest = text;
    while let Some(hash) = rest.find('#') {
        let after = &rest[hash + 1..];
        let tag = &after[..after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len())];
        let starts_word = rest[..hash].chars().last().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '['));
        if !starts_word || tag.is_empty() || tag.chars().all(|c| c.is_ascii_digit()) {
            escape_into(&rest[..=hash], out);
            rest = after;
            continue;
        }
        escape_into(&rest[..hash], out);
        let page: String = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect();
        out.push_str("<a href=\"");
        escape_into(&link_to(&format!("https://www.facebook.com/hashtag/{page}")), out);
        out.push_str("\">#");
        escape_into(tag, out);
        out.push_str("</a>");
        rest = &after[tag.len()..];
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
    use crate::extract::facebook::url::Target;
    use crate::extract::fake::FakeExtractor;
    use crate::extract::{AuthorProfile, Media};

    fn html_of(text: &str) -> String {
        text_to_html(text, &super::direct)
    }

    fn direct(post: &Post) -> Vec<MediaUrl> {
        post.media.iter().map(MediaUrl::direct).collect()
    }

    fn render(post: &Post, settings: &Settings) -> String {
        preview(post, &direct(post), settings, "https://www.facebook.com/somepage/posts/1?x=1&y=2")
    }

    fn album() -> Post {
        FakeExtractor::post(&Target::from_request("/somepage/posts/1").unwrap())
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
    fn text_only_page() {
        let mut post = album();
        post.media.clear();
        post.kind = PostKind::TextOnly;
        post.published_at = None;
        post.author = None;
        let html = render(&post, &Settings::default());
        assert!(html.contains("<meta property=\"og:description\" content=\"Hello &#60;world&#62;"));
        assert!(html.contains("article:published_time"), "the Medium route needs a time even when unknown");
        assert!(!html.contains("og:image"));
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("héllo world", 5), "héll…");
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("  hi  ", 5), "hi");
        assert_eq!(truncate("ab cd", 4), "ab…");
    }

    #[test]
    fn text_to_html_escapes_and_links() {
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
    }

    #[test]
    fn hashtags_link_to_facebook() {
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

    #[test]
    fn text_posts_show_the_author_small() {
        let mut post = album();
        post.media.clear();
        post.kind = PostKind::TextOnly;
        post.profile.avatar = Some("https://scontent.xx.fbcdn.net/v/avatar.jpg".into());
        let html = render(&post, &Settings::default());
        assert!(html.contains(r#"<meta property="og:image" content="https://scontent.xx.fbcdn.net/v/avatar.jpg">"#));
        assert!(html.contains(r#"<meta name="twitter:card" content="summary">"#));
    }
}
