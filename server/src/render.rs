//! The page Telegram gets: preview tags in `<head>`, the Instant View article in `<body>`.

use askama::Template;

use crate::extract::{MediaKind, Post, PostKind};
use crate::settings::Settings;

/// Where each media item's file and thumbnail are linked: CDN URLs or `/_/m/` redirects.
#[derive(Debug, Clone)]
pub struct MediaUrl {
    pub src: String,
    pub poster: Option<String>,
}

pub struct VideoTag<'a> {
    src: &'a str,
    width: Option<u32>,
    height: Option<u32>,
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
}

#[derive(Template)]
#[template(path = "preview.html")]
struct PreviewPage<'a> {
    site_name: &'a str,
    title: &'a str,
    description: String,
    og_type: &'a str,
    image: Option<&'a str>,
    video: Option<VideoTag<'a>>,
    original: &'a str,
    article: Option<Article<'a>>,
}

#[derive(Template)]
#[template(path = "unavailable.html")]
struct UnavailablePage<'a> {
    site_name: &'a str,
    original: &'a str,
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
    pub site: &'a str,
    pub title: &'a str,
    pub description: String,
    pub image: Option<&'a str>,
    /// Index of the video in `post.media`, if any.
    pub video: Option<usize>,
    pub instant_view: bool,
}

pub fn summary<'a>(post: &'a Post, media: &'a [MediaUrl], settings: &'a Settings) -> Summary<'a> {
    let title = post.title.as_deref().or(post.author.as_deref()).unwrap_or(&settings.site_name);
    let video = post.media.iter().position(|m| m.kind == MediaKind::Video);
    let image = match video {
        Some(i) => media[i].poster.as_deref(),
        None => media.first().map(|m| m.src.as_str()),
    };
    let photos = post.photo_count();
    let text = post.text.as_deref().unwrap_or_default();
    let description = match (photos, video) {
        (n, None) if n > 1 && text.is_empty() => format!("1/{n}"),
        (n, None) if n > 1 => truncate(&format!("1/{n} · {text}"), settings.description_length),
        _ => truncate(text, settings.description_length),
    };
    Summary {
        site: &settings.site_name,
        title,
        description,
        image,
        video,
        instant_view: instant_view_enabled(post, settings),
    }
}

/// Renders the preview page. `media` holds one entry per `post.media` item.
pub fn preview(post: &Post, media: &[MediaUrl], settings: &Settings, original: &str) -> String {
    let Summary { title, description, image, video: video_index, instant_view, .. } = summary(post, media, settings);
    let text = post.text.as_deref().unwrap_or_default();
    let video =
        video_index.map(|i| VideoTag { src: &media[i].src, width: post.media[i].width, height: post.media[i].height });

    let article = instant_view.then(|| {
        let published_at = post.published_at.unwrap_or_else(|| jiff::Timestamp::now().as_second());
        let published = jiff::Timestamp::from_second(published_at).unwrap_or_default();
        let date = post
            .published_at
            .and_then(|t| jiff::Timestamp::from_second(t).ok())
            .map(|t| t.strftime("%-d %b %Y").to_string());
        let byline = match (post.author.as_deref(), date) {
            (Some(author), Some(date)) => Some(format!("{author} · {date}")),
            (Some(author), None) => Some(author.to_string()),
            (None, Some(date)) => Some(date),
            (None, None) => None,
        };
        Article {
            published: published.to_string(),
            author: post.author.as_deref(),
            heading: title,
            byline,
            body: text_to_html(text),
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
        }
    });

    PreviewPage {
        site_name: &settings.site_name,
        title,
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

/// The minimal "couldn't load" page.
pub fn unavailable(settings: &Settings, original: &str) -> String {
    UnavailablePage { site_name: &settings.site_name, original }.render().expect("rendering to a String can't fail")
}

/// Cuts text to `max` characters, ending with an ellipsis when cut.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max.saturating_sub(1)) {
        Some((index, _)) if text[index..].chars().nth(1).is_some() => format!("{}…", text[..index].trim_end()),
        _ => text.to_string(),
    }
}

/// Paragraphs on blank lines, line breaks within them, and clickable http(s) links; all escaped.
pub fn text_to_html(text: &str) -> String {
    let mut html = String::new();
    let normalized = text.replace("\r\n", "\n");
    for paragraph in normalized.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        html.push_str("<p>");
        for (i, line) in paragraph.lines().enumerate() {
            if i > 0 {
                html.push_str("<br>");
            }
            linkify(line, &mut html);
        }
        html.push_str("</p>\n");
    }
    html
}

fn linkify(line: &str, out: &mut String) {
    let mut rest = line;
    while let Some(start) = find_link(rest) {
        escape_into(&rest[..start], out);
        let link_end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |e| start + e);
        let mut link = &rest[start..link_end];
        // Trailing punctuation belongs to the sentence, not the link.
        while let Some(last) = link.chars().last()
            && matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'')
        {
            link = &link[..link.len() - last.len_utf8()];
        }
        out.push_str("<a href=\"");
        escape_into(link, out);
        out.push_str("\">");
        escape_into(link, out);
        out.push_str("</a>");
        rest = &rest[start + link.len()..];
    }
    escape_into(rest, out);
}

fn find_link(text: &str) -> Option<usize> {
    let http = text.find("http://");
    let https = text.find("https://");
    let start = match (http, https) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    // Only at the start of a word.
    if start == 0 || text[..start].ends_with(|c: char| c.is_whitespace() || c == '(') {
        Some(start)
    } else {
        let next = find_link(&text[start + 4..])?;
        Some(start + 4 + next)
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
    use crate::extract::Media;
    use crate::extract::facebook::url::Target;
    use crate::extract::fake::FakeExtractor;

    fn direct(post: &Post) -> Vec<MediaUrl> {
        post.media.iter().map(|m| MediaUrl { src: m.url.clone(), poster: m.thumbnail.clone() }).collect()
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
            text_to_html("a <b> & c\nline 2 (see https://x.com/a?b=1&c=2).\n\n\nnext"),
            "<p>a &lt;b&gt; &amp; c<br>line 2 (see <a href=\"https://x.com/a?b=1&amp;c=2\">https://x.com/a?b=1&amp;c=2</a>).</p>\n<p>next</p>\n"
        );
        assert_eq!(text_to_html("xhttps://no"), "<p>xhttps://no</p>\n");
        assert_eq!(text_to_html("\"><script>"), "<p>&quot;&gt;&lt;script&gt;</p>\n");
    }
}
