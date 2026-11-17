//! The dashboard's playground page.

use super::*;

struct Mock {
    site: String,
    title: String,
    description: String,
    image: Option<String>,
    /// The image is the author's picture, which Telegram shows small, beside the text.
    small: bool,
    video: bool,
    instant_view: bool,
    tags: String,
    /// The site, for Discord's embed colour.
    source: &'static str,
    /// Discord's lines above the title, from its oEmbed.
    provider: String,
    author: Option<String>,
}

/// A step of the extraction, drawn as a bar in a 0–100 box: label, class, start, width, time.
type Step = (&'static str, &'static str, f64, f64, String);

struct Tried {
    source: &'static str,
    /// The site's name, for the heading: "Reddit post".
    site: &'static str,
    page_type: &'static str,
    outcome: &'static str,
    key: String,
    /// What the timings add up to, or why there are none.
    took: String,
    steps: Vec<Step>,
    requests: u32,
    error: Option<String>,
    preview: Option<Mock>,
    /// The whole page Telegram would get, for the sandboxed Instant View frame.
    html: String,
    json: String,
}

/// The article page with a stylesheet that makes it look like Telegram's Instant View, and the
/// date line Telegram writes under the heading.
fn framed(html: &str) -> String {
    let link = format!(r#"<link rel="stylesheet" href="{}">"#, crate::dashboard::asset("iv.css"));
    let mut page = match html.find("</head>") {
        Some(at) => format!("{}{link}{}", &html[..at], &html[at..]),
        None => format!("{link}{html}"),
    };
    if let (Some(date), Some(at)) = (telegram_date(html), page.find("</h1>")) {
        page.insert_str(at + "</h1>".len(), &format!("\n<p class=\"tg-date\">{date}</p>"));
    }
    page
}

/// The article's `article:published_time` as Telegram Desktop writes it under the heading, in the
/// reader's time zone: "November 16 at 8:51 PM", with the year when it isn't this one.
fn telegram_date(html: &str) -> Option<String> {
    const META: &str = r#"<meta property="article:published_time" content=""#;
    let start = html.find(META)? + META.len();
    let at: jiff::Timestamp = html[start..start + html[start..].find('"')?].parse().ok()?;
    let local = crate::dashboard::local(at.as_second())?;
    let this_year = crate::dashboard::local(jiff::Timestamp::now().as_second())?.year();
    let format = if local.year() == this_year { "%B %-d at %-I:%M %p" } else { "%B %-d, %Y at %-I:%M %p" };
    Some(local.strftime(format).to_string())
}

/// A profile picture as a `data:` image. Meta's CDN serves those with
/// `Cross-Origin-Resource-Policy: same-origin`, so a browser won't show one on another site's
/// page, though Telegram's apps do. Only a file on Meta's CDNs, as the site's answer named it,
/// through the site's proxy and without cookies.
async fn inlined(app: &App, settings: &Settings, source: Source, url: &str) -> Option<String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if !crate::extract::trusted_media_url(url) {
        return None;
    }
    let client = app.videos.client(settings.source(source).proxy.as_deref()).ok()?;
    let fetched = tokio::time::timeout(Duration::from_secs(5), crate::video::download(&client, url, LIMIT)).await;
    let bytes = fetched.ok()?.ok()?;
    let kind = match bytes.get(..4)? {
        [0x89, b'P', b'N', b'G'] => "image/png",
        b"RIFF" => "image/webp",
        _ => "image/jpeg",
    };
    Some(format!("data:{kind};base64,{}", data_encoding::BASE64.encode(&bytes)))
}

/// Bars for each step, scaled to the whole.
fn steps(t: &Timings) -> Vec<Step> {
    let parts = [("Fetch", "fetch", t.fetch), ("Parse", "parse", t.parse), ("Extract", "extract", t.extract)];
    let total = parts.iter().map(|p| p.2.as_secs_f64()).sum::<f64>().max(0.001);
    let mut at: f64 = 0.0;
    parts
        .into_iter()
        .map(|(label, class, d)| {
            let width = (d.as_secs_f64() * 100.0 / total).max(0.5);
            let step = (label, class, (at * 100.0).round() / 100.0, (width * 100.0).round() / 100.0, duration(d));
            at += width;
            step
        })
        .collect()
}

#[derive(Template)]
#[template(path = "dashboard/playground.html")]
pub struct PlaygroundPage {
    shell: Shell,
    url: String,
    fresh: bool,
    cookies: bool,
    /// The links tried lately, newest first: the link, it for a query string, and its site.
    recent: Vec<(String, String, Option<&'static str>)>,
    result: Option<Tried>,
    /// The Homura sticker, as the bot's picture in the chat.
    avatar: String,
    /// Run the link as soon as the page loads: it came from a link elsewhere in the dashboard.
    run: bool,
}

fn recent(d: &Dashboard) -> Vec<(String, String, Option<&'static str>)> {
    lock(&d.tried)
        .iter()
        .map(|link| {
            let encoded = url::form_urlencoded::byte_serialize(link.as_bytes()).collect();
            (link.clone(), encoded, Source::of_link(link).map(Source::as_str))
        })
        .collect()
}

fn playground_page(
    d: &Dashboard,
    shell: Shell,
    url: String,
    fresh: bool,
    cookies: bool,
    result: Option<Tried>,
) -> PlaygroundPage {
    let avatar = crate::dashboard::asset("homura.webp");
    PlaygroundPage { shell, url, fresh, cookies, recent: recent(d), result, avatar, run: false }
}

#[derive(Debug, Default, Deserialize)]
pub struct PlaygroundQuery {
    #[serde(default)]
    url: String,
    done: Option<String>,
    /// `?run=1`: try the link at once.
    #[serde(default)]
    run: String,
    /// `?cached=1`: the cache's copy, if there is one, rather than a fresh fetch.
    #[serde(default)]
    cached: String,
}

pub async fn playground(State(d): State<Shared>, headers: HeaderMap, Query(q): Query<PlaygroundQuery>) -> Response {
    let shell = Shell::new("playground", &Done { done: q.done });
    let mut page = playground_page(&d, shell, q.url, q.cached != "1", false, None);
    // Only from the dashboard's own links: another site's link here, followed with a sign-in
    // cookie that goes along (Cloudflare Access's may), mustn't make the server fetch.
    let from_dashboard = headers.get("sec-fetch-site").is_some_and(|v| v == "same-origin");
    page.run = q.run == "1" && from_dashboard && !page.url.is_empty();
    render(&page)
}

#[derive(Deserialize)]
pub struct PlaygroundForm {
    url: String,
    fresh: Option<String>,
    cookies: Option<String>,
}

/// Links the playground remembers, for trying again.
const RECENT: usize = 6;

pub async fn run_playground(State(d): State<Shared>, Form(f): Form<PlaygroundForm>) -> Response {
    let (fresh, cookies, link) = (f.fresh.is_some(), f.cookies.is_some(), f.url.trim().to_string());
    let Some(target) = Target::from_link(&link) else {
        let shell =
            Shell::refused("playground", format!("That isn't a link to a site Homura reads: {}.", Source::listed()));
        return refused(&playground_page(&d, shell, f.url, fresh, cookies, None));
    };
    {
        let mut tried = lock(&d.tried);
        tried.retain(|l| *l != link);
        tried.push_front(link.clone());
        tried.truncate(RECENT);
    }
    match try_link(&d.app, &target, fresh, cookies).await {
        Ok(tried) => {
            render(&playground_page(&d, Shell::new("playground", &Done::default()), f.url, fresh, cookies, Some(tried)))
        }
        Err(Busy) => {
            let message =
                format!("No {} fetch was free within 2 seconds; try again in a moment.", target.source.name());
            refused(&playground_page(&d, Shell::refused("playground", message), f.url, fresh, cookies, None))
        }
    }
}

/// Telegram waits a few seconds for a card's picture, as the preview host does for a mosaic.
const MOSAIC_WAIT: Duration = Duration::from_secs(6);

/// Builds the post's mosaic as the preview host would for Telegram, through the site's proxy.
async fn made_mosaic(app: &App, settings: &Settings, post: &Arc<Post>, id: &str) -> bool {
    let Some(source) = post.source() else { return false };
    let Ok(client) = app.videos.client(settings.source(source).proxy.as_deref()) else { return false };
    match tokio::time::timeout(MOSAIC_WAIT, app.mosaics.build(id, post, &client)).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            tracing::warn!(url = %post.key, "no mosaic for the playground, so the first photo: {e:#}");
            false
        }
        Err(_) => {
            tracing::warn!(url = %post.key, "the playground's mosaic took over {}s, so the first photo", MOSAIC_WAIT.as_secs());
            false
        }
    }
}

/// A mosaic the playground built for its card: the picture Telegram gets from `/_/g/`.
pub async fn playground_card(State(d): State<Shared>, Path(file): Path<String>) -> Response {
    let Some(id) = file.strip_suffix(".jpg") else { return StatusCode::NOT_FOUND.into_response() };
    match d.app.mosaics.get(id).await {
        Some(jpeg) => ([(CONTENT_TYPE, "image/jpeg")], jpeg).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A post, and how long extracting it took (`None` when it came from the cache).
type Found = (Arc<Post>, Option<Timings>);

async fn try_link(app: &App, target: &Target, fresh: bool, cookies: bool) -> Result<Tried, Busy> {
    let settings = app.settings.get();
    // The cached copy wasn't fetched with the cookies, so asking for them is asking for a fetch.
    let cached = if fresh || cookies { None } else { app.cache.get(target.key()).await };
    let (outcome, result): (_, Result<Found, ExtractError>) = match cached {
        Some(Entry { outcome, .. }) => ("From the cache", outcome.map(|post| (post, None))),
        None => {
            let choice = if cookies { CookieChoice::Force } else { CookieChoice::Policy };
            match app.extract(target, choice, &settings).await? {
                Ok((post, timings)) => {
                    // Not what Telegram gets when the cookie policy wouldn't have used the cookies,
                    // so kept out of the cache it's served from.
                    if !cookies {
                        let ttl = Duration::from_secs(settings.cache_minutes * 60);
                        let entry = Entry::post(post.clone(), ttl);
                        app.cache.insert(settings.cache_megabytes, target.key().to_string(), entry).await;
                    }
                    ("Extracted", Ok((Arc::new(post), Some(timings))))
                }
                Err(e) => ("Failed", Err(e)),
            }
        }
    };
    let mut tried = Tried {
        source: target.source.as_str(),
        site: target.source.name(),
        page_type: target.page_type.as_str(),
        outcome,
        key: target.key().to_string(),
        took: String::new(),
        steps: Vec::new(),
        requests: 0,
        error: None,
        preview: None,
        html: String::new(),
        json: String::new(),
    };
    match result {
        Err(e) => {
            tried.outcome = e.stage.label();
            tried.error = Some(e.message);
        }
        Ok((post, timings)) => {
            match timings {
                Some(t) => {
                    tried.took = duration(t.fetch + t.parse + t.extract);
                    tried.steps = steps(&t);
                    tried.requests = t.requests;
                }
                None => tried.took = "No fetch: the post came from the cache.".into(),
            }
            let media: Vec<MediaUrl> = post.media.iter().map(MediaUrl::direct).collect();
            // The card Telegram gets: several photos as one picture, from `/_/g/` on the preview
            // host, as its page links it.
            let mosaic = match crate::mosaic::wanted(&post, target, &settings) {
                true => Some(app.cache.media_id(&post.key).await),
                false => None,
            };
            let card = mosaic.as_ref().map(|id| MediaUrl {
                src: format!("/_/g/{id}.jpg"),
                poster: None,
                size: crate::mosaic::size(&post),
            });
            let summary = preview::summary(&post, &media, card.as_ref(), &settings, target);
            // The author's pictures, which a browser won't show from Meta's CDN on this page.
            let mut shown = (*post).clone();
            if let Some(url) = &post.profile.picture {
                shown.profile.picture =
                    Some(inlined(app, &settings, target.source, url).await.unwrap_or_else(|| url.clone()));
            }
            let mut image = summary.image.map(str::to_string);
            if let Some(id) = &mosaic {
                // Built as the preview host builds it; when that fails, Telegram gets the first photo.
                image = if made_mosaic(app, &settings, &post, id).await {
                    Some(format!("/playground/card/{id}.jpg"))
                } else {
                    media.first().map(|m| m.src.clone())
                };
            }
            if summary.avatar
                && let Some(url) = &image
                && let Some(data) = inlined(app, &settings, target.source, url).await
            {
                image = Some(data);
            }
            tried.html =
                framed(&preview::preview_with(&shown, &media, card.as_ref(), &settings, target, &preview::direct));
            let tags = tried
                .html
                .lines()
                .filter(|line| line.starts_with("<meta property=") || line.starts_with("<meta name=\"twitter"))
                .collect::<Vec<_>>()
                .join("\n");
            let (provider, author) = preview::discord_lines(&summary);
            tried.preview = Some(Mock {
                source: target.source.as_str(),
                provider,
                author,
                site: summary.site.to_string(),
                title: summary.title.to_string(),
                description: summary.description,
                image,
                small: summary.avatar,
                video: summary.video.is_some(),
                instant_view: summary.instant_view,
                tags,
            });
            tried.json = serde_json::to_string_pretty(&*post).unwrap_or_default();
        }
    }
    Ok(tried)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_date_goes_under_the_heading_as_telegram_writes_it() {
        let page = "<head>\n<meta property=\"article:published_time\" content=\"2023-11-14T22:13:20Z\">\n</head>\n<body><article><sub>s</sub><h1>Title</h1><p>text</p></article></body>";
        let framed = framed(page);
        assert!(
            framed.contains("<h1>Title</h1>\n<p class=\"tg-date\">November 14, 2023 at 10:13 PM</p><p>text</p>"),
            "{framed}"
        );
        assert!(framed.find("iv.css") < framed.find("</head>"), "the stylesheet goes in the head");
    }
}
