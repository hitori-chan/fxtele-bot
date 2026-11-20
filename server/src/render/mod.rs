//! The page Telegram gets: preview tags in `<head>`, the Instant View article in `<body>`; and
//! Discord's, with its own tags and no article.

use std::borrow::Cow;

use askama::Template;
use unicode_segmentation::UnicodeSegmentation;

use crate::extract::{Media, MediaKind, PageType, Post, PostKind, Relation, Shared, Source, Stage, Stats, Target};
use crate::settings::Settings;

mod site;
mod text;

use site::{Style, TextStyle};
use text::text_to_html;
pub use text::{escape_into, linkify_reddit};

/// Below this width a profile's picture is shown small (`twitter:card=summary`).
const SMALL_PICTURE: u32 = 400;

/// Where each media item's file and thumbnail are linked: CDN URLs, `/_/m/` redirects, or a
/// joined `/_/v/` video.
#[derive(Debug, Clone)]
pub struct MediaUrl {
    pub src: String,
    pub poster: Option<String>,
    /// The file's size when it isn't the media's own (a joined 1080p video).
    pub size: Option<(u32, u32)>,
    /// A video too big for Telegram to play: the card shows its poster instead.
    pub too_big: bool,
}

impl MediaUrl {
    /// The CDN URLs as they are.
    pub fn direct(media: &Media) -> Self {
        Self { src: media.url.clone(), poster: media.thumbnail.clone(), size: None, too_big: false }
    }
}

struct VideoTag<'a> {
    src: &'a str,
    width: Option<u32>,
    height: Option<u32>,
}

/// Who asked for the page: Telegram's fetcher, or Discord's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    Telegram,
    Discord,
}

impl Reader {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Discord => "discord",
        }
    }
}

/// Discord's own tags: its embed's colour and an oEmbed link for the lines above the title (and
/// a player card for a video).
struct DiscordTags<'a> {
    colour: &'static str,
    oembed: Option<&'a str>,
}

struct ImageTag<'a> {
    src: &'a str,
    width: Option<u32>,
    height: Option<u32>,
    /// The author's picture: Telegram shows it small, beside the text.
    small: bool,
}

/// A photo or video of the article. Written bare, one after the other, as FxEmbed writes them: each
/// in a `<figure>`, Telegram put the article's text fields between the first and the rest.
struct Figure<'a> {
    video: bool,
    src: &'a str,
    poster: Option<&'a str>,
}

/// Telegram shows the heading, then the date from `published`, then the rest in order; FxEmbed's
/// layout, so it reads the same as its posts.
struct Article<'a> {
    published: String,
    heading: &'a str,
    /// Who posted where, under a headline: "u/someone in r/rust".
    byline: Option<String>,
    /// Escaped HTML built by [`text_to_html`].
    body: String,
    figures: Vec<Figure<'a>>,
    stats: Option<String>,
    about: Option<About<'a>>,
    /// The post this one shares or answers, shown after the post's own words.
    shared: Option<SharedPart<'a>>,
}

struct SharedPart<'a> {
    heading: String,
    url: Option<&'a str>,
    title: Option<&'a str>,
    /// Escaped HTML built by [`text_to_html`].
    body: String,
    /// The figures are the post's own, so they go with its words; else they're this post's.
    figures_first: bool,
}

/// "About the author" at the end of Instant View, as FxEmbed has: their name even when the
/// heading has it, as there.
pub struct About<'a> {
    name: &'a str,
    /// Only the full-size picture: the post's small one (Facebook's is 40 px) would be a blur.
    picture: Option<&'a str>,
    url: Option<&'a str>,
    /// `@someone` where the site has handles, else the link without `https://www.`:
    /// `facebook.com/someone`.
    url_text: Option<String>,
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
    /// `summary`, `summary_large_image`, or `player` for Discord's videos.
    twitter_card: Option<&'static str>,
    original: &'a str,
    article: Option<Article<'a>>,
    discord: Option<DiscordTags<'a>>,
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
    /// The image is a person's picture, shown small: the author's, or a profile's own when it's
    /// too small to fill a large card.
    pub avatar: bool,
    /// Index of the video in `post.media`, if any.
    pub video: Option<usize>,
    pub instant_view: bool,
    /// Counts for Instant View: "💬 12 🔁 79 👍 1.5K".
    pub stats: Option<String>,
}

/// Which item the card shows: the one the link asks for (`img_index`), else the first video, else
/// the first photo. None of a post marked NSFW or a spoiler.
pub fn shown_item(post: &Post, target: &Target) -> Option<usize> {
    if post.sensitive {
        return None;
    }
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
    let video = shown.filter(|i| post.media[*i].kind == MediaKind::Video && !media[*i].too_big);
    let site_name = settings.site_name(target.source);
    // Reddit's headline is the title, so where and by whom go with the site.
    let byline = byline(post, target.source);
    let site_name = match &byline {
        Some(byline) => Cow::Owned(format!("{site_name} · {}", byline.replace(" in ", " · "))),
        None => Cow::Borrowed(site_name),
    };
    let site = match shown {
        _ if card.is_some() => format!("{site_name} · {} photos", post.media.len()),
        Some(i) if post.media.len() > 1 => {
            let kind = if post.media[i].kind == MediaKind::Video { "Video" } else { "Photo" };
            format!("{site_name} · {kind} {} / {}", i + 1, post.media.len())
        }
        _ => site_name.to_string(),
    };
    // Instagram's logged-out profile pictures are 150 px: blurry across a large card.
    let small_picture = post.kind == PostKind::Profile
        && shown.is_some_and(|i| {
            post.media[i].kind == MediaKind::Photo && post.media[i].width.is_some_and(|w| w < SMALL_PICTURE)
        });
    let (image, image_size) = match (card, shown) {
        (Some(card), _) => (Some(card.src.as_str()), card.size),
        (None, Some(i)) if post.media[i].kind == MediaKind::Video => (media[i].poster.as_deref(), None),
        (None, Some(i)) => (Some(media[i].src.as_str()), post.media[i].width.zip(post.media[i].height)),
        (None, None) => (post.profile.best_picture(), None),
    };
    Summary {
        site,
        title: title(post, settings.site_name(target.source), target.source),
        description: truncate(&card_text(post, target.source), settings.description_length),
        image,
        image_size,
        avatar: card.is_none() && (shown.is_none() || small_picture) && image.is_some(),
        video,
        instant_view: instant_view_enabled(post, settings),
        stats: settings.instant_view_counts.then(|| stats_line(&post.stats, target.source)).flatten(),
    }
}

/// The card's text: the post's, then the post it shares under who wrote it, as FxEmbed shows a
/// quote.
fn card_text(post: &Post, source: Source) -> String {
    let plain = |text: Option<&str>| {
        let text = text.unwrap_or_default().replace("\r\n", "\n");
        if matches!(Style::of(source).text, TextStyle::Markdown) { crate::markdown::to_text(&text) } else { text }
    };
    let own = plain(post.text.as_deref());
    let Some(shared) = &post.shared else { return own };
    let shared_text = match &shared.title {
        Some(title) => format!("{title}\n\n{}", plain(shared.text.as_deref())),
        None => plain(shared.text.as_deref()),
    };
    let part = format!("{}:\n{}", shared_heading(shared), shared_text.trim());
    [own.trim(), part.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn shared_heading(shared: &Shared) -> String {
    match (shared.relation, shared.author.as_deref()) {
        (Relation::Reply, Some(author)) => format!("↘️ Replying to {author}"),
        (Relation::Reply, None) => "↘️ Replying to a post".to_string(),
        (Relation::Quote, Some(author)) => format!("↘️ Quoting {author}"),
        (Relation::Quote, None) => "↘️ Quoting a post".to_string(),
        (Relation::Share, Some(author)) => format!("↘️ Shared from {author}"),
        (Relation::Share, None) => "↘️ Shared post".to_string(),
    }
}

/// Who posted where, when the title is the post's headline (Reddit's): "u/someone in r/rust".
fn byline(post: &Post, source: Source) -> Option<String> {
    if !Style::of(source).headlines || post.kind == PostKind::Profile {
        return None;
    }
    match (post.author.as_deref(), post.group.as_deref()) {
        (Some(author), Some(group)) => Some(format!("{author} in {group}")),
        (author, group) => author.or(group).map(str::to_string),
    }
}

/// Telegram's bold line: who posted ("Author ▸ Group", "Name (@handle)"), or the page's own name
/// for profiles, albums ("Owner ▸ Album") and pages without a post (events, groups).
fn title<'a>(post: &'a Post, site_name: &'a str, source: Source) -> Cow<'a, str> {
    let named = post.title.as_deref().or(post.author.as_deref()).unwrap_or(site_name);
    if Style::of(source).headlines && post.kind != PostKind::Profile {
        return Cow::Borrowed(named);
    }
    let with_handle = |name: &'a str| match post.profile.handle.as_deref() {
        Some(handle) if handle != name => Cow::Owned(format!("{name} (@{handle})")),
        _ => Cow::Borrowed(name),
    };
    if post.kind == PostKind::Profile {
        return with_handle(named);
    }
    // An album's name says what it holds, its owner's whose it is: "Some Page ▸ Mobile uploads".
    if post.page_type == PageType::Album {
        return match (post.author.as_deref(), post.title.as_deref()) {
            (Some(owner), Some(name)) if !name.contains(owner) => Cow::Owned(format!("{owner} ▸ {name}")),
            _ => Cow::Borrowed(named),
        };
    }
    match (post.author.as_deref(), post.group.as_deref()) {
        (Some(author), Some(group)) => Cow::Owned(format!("{author} ▸ {group}")),
        (Some(author), None) => with_handle(author),
        (None, _) => Cow::Borrowed(named),
    }
}

/// "💬 12 🔁 79 👍 1.5K 👁️ 3.9K" in FxEmbed's order and spacing, leaving out what's unknown or
/// zero. Instagram's likes are hearts, as there.
fn stats_line(stats: &Stats, source: Source) -> Option<String> {
    let liked = Style::of(source).liked;
    let counts = [("💬", stats.comments), ("🔁", stats.shares), (liked, stats.reactions), ("👁️", stats.views)];
    let parts: Vec<String> = counts
        .into_iter()
        .filter_map(|(icon, count)| count.filter(|n| *n > 0).map(|n| format!("{icon} {}", compact(n))))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
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
    page(post, media, &summary(post, media, card, settings, target), target, link, None)
}

/// The page Discord gets: the same card, with its own tags, and no Instant View. `oembed` makes
/// the link to the lines Discord shows above the title ([`discord_lines`]) from the card.
pub fn discord_page(
    post: &Post,
    media: &[MediaUrl],
    card: Option<&MediaUrl>,
    settings: &Settings,
    target: &Target,
    oembed: impl FnOnce(&Summary<'_>) -> Option<String>,
) -> String {
    let summary = summary(post, media, card, settings, target);
    let oembed = oembed(&summary);
    page(post, media, &summary, target, &direct, Some(oembed.as_deref()))
}

/// What Discord's oEmbed shows above the card's title: the site, then a video's text (Discord
/// leaves the description out of a video's card, as FxEmbed works around), else the counts.
pub fn discord_lines(summary: &Summary<'_>) -> (String, Option<String>) {
    let author = match summary.video {
        Some(_) if !summary.description.is_empty() => Some(truncate(&summary.description, 255)),
        _ => summary.stats.clone(),
    };
    (summary.site.clone(), author)
}

/// Discord shows a video too big or too small badly: FxEmbed halves past 1920 px and doubles
/// under 400 px, keeping the shape.
fn discord_size(width: Option<u32>, height: Option<u32>) -> (Option<u32>, Option<u32>) {
    let (w, h) = (width.unwrap_or(0), height.unwrap_or(0));
    let scale: fn(u32) -> u32 = if w > 1920 || h > 1920 {
        |n| n / 2
    } else if w > 0 && h > 0 && w < 400 && h < 400 {
        |n| n * 2
    } else {
        |n| n
    };
    (width.map(scale), height.map(scale))
}

fn page(
    post: &Post,
    media: &[MediaUrl],
    summary: &Summary<'_>,
    target: &Target,
    link: LinkFn<'_>,
    discord: Option<Option<&str>>,
) -> String {
    let original = target.original.as_str();
    let video = summary.video.map(|i| {
        let width = media[i].size.map(|s| s.0).or(post.media[i].width);
        let height = media[i].size.map(|s| s.1).or(post.media[i].height);
        let (width, height) = if discord.is_some() { discord_size(width, height) } else { (width, height) };
        VideoTag { src: &media[i].src, width, height }
    });
    let image = summary.image.map(|src| ImageTag {
        src,
        width: summary.image_size.map(|s| s.0),
        height: summary.image_size.map(|s| s.1),
        small: summary.avatar,
    });
    let instant_view = summary.instant_view && discord.is_none();
    // With Instant View on, Telegram reads the description as HTML: line breaks must be `<br>`,
    // and a `<` or `&` of the text escaped, or "a <b> c" loses its "<b>".
    let description = if instant_view {
        summary.description.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br>")
    } else {
        summary.description.clone()
    };
    let player = discord.is_some() && video.is_some();
    let twitter_card = match &image {
        _ if player => Some("player"),
        Some(image) if image.small => Some("summary"),
        Some(_) => Some("summary_large_image"),
        None => None,
    };

    let article = instant_view.then(|| {
        let published_at = post.published_at.unwrap_or_else(|| jiff::Timestamp::now().as_second());
        let published = jiff::Timestamp::from_second(published_at).unwrap_or_default();
        Article {
            published: published.to_string(),
            heading: &summary.title,
            byline: byline(post, target.source),
            body: text_to_html(post.text.as_deref().unwrap_or_default(), &Style::of(target.source).text, link),
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
            // Only with something to add to the name the heading shows, and not on the author's
            // own page, which is all about them already.
            about: post
                .author
                .as_deref()
                .filter(|_| post.kind != PostKind::Profile)
                .map(|name| {
                    let profile = &post.profile;
                    let url_text = match (profile.handle.as_deref(), profile.url.as_deref()) {
                        (Some(handle), Some(_)) => Some(format!("@{handle}")),
                        (None, Some(url)) => Some(url.trim_start_matches("https://www.").to_string()),
                        (_, None) => None,
                    };
                    About {
                        name,
                        picture: profile.picture.as_deref(),
                        url: profile.url.as_deref(),
                        url_text,
                        bio: profile.bio.as_deref(),
                    }
                })
                .filter(|about| about.picture.is_some() || about.url.is_some() || about.bio.is_some()),
            shared: post.shared.as_ref().map(|shared| SharedPart {
                heading: shared_heading(shared),
                url: shared.url.as_deref(),
                title: shared.title.as_deref(),
                body: text_to_html(shared.text.as_deref().unwrap_or_default(), &Style::of(target.source).text, link),
                figures_first: shared.post_owns_media,
            }),
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
        twitter_card,
        original,
        article,
        discord: discord.map(|oembed| DiscordTags { colour: Style::of(target.source).colour, oembed }),
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
        Some(Stage::NotFound) => Style::of(target.source).not_public.to_string(),
        Some(Stage::RateLimited) => format!("{site} asked us to slow down for a while."),
        Some(Stage::Redirect) => format!("This link doesn't lead to a post on {site}."),
        Some(Stage::Fetch) => format!("{site} couldn't be reached just now."),
        Some(Stage::Timeout) => format!("{site} didn't answer in time."),
        None => format!("Too many {site} links at once; try again in a minute."),
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
    if text.chars().nth(max).is_none() {
        return text.to_string();
    }
    // Cut where a character as it's seen ends, so a flag, an emoji made of several, or a letter
    // and its accents is never split; the ellipsis takes the last place.
    let mut end = 0;
    let mut used = 0;
    for (at, grapheme) in text.grapheme_indices(true) {
        used += grapheme.chars().count();
        if used >= max {
            break;
        }
        end = at + grapheme.len();
    }
    format!("{}…", text[..end].trim_end())
}

#[cfg(test)]
mod tests;
