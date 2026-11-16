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
}

/// A step of the extraction, drawn as a bar in a 0–100 box: label, class, start, width, time.
type Step = (&'static str, &'static str, f64, f64, String);

struct Tried {
    source: &'static str,
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

/// The article page with a stylesheet that makes it look like Telegram's Instant View.
fn framed(html: &str) -> String {
    let link = format!(r#"<link rel="stylesheet" href="{}">"#, crate::dashboard::asset("iv.css"));
    match html.find("</head>") {
        Some(at) => format!("{}{link}{}", &html[..at], &html[at..]),
        None => format!("{link}{html}"),
    }
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
    PlaygroundPage { shell, url, fresh, cookies, recent: recent(d), result, avatar }
}

#[derive(Debug, Default, Deserialize)]
pub struct PlaygroundQuery {
    #[serde(default)]
    url: String,
    done: Option<String>,
}

pub async fn playground(State(d): State<Shared>, Query(q): Query<PlaygroundQuery>) -> Response {
    let shell = Shell::new("playground", &Done { done: q.done });
    render(&playground_page(&d, shell, q.url, true, false, None))
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
        let shell = Shell::refused("playground", "That isn't a link to a site Homura reads: Facebook or Instagram.");
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

/// A post, and how long extracting it took (`None` when it came from the cache).
type Found = (Arc<Post>, Option<Timings>);

async fn try_link(app: &App, target: &Target, fresh: bool, cookies: bool) -> Result<Tried, Busy> {
    let settings = app.settings.get();
    let cached = if fresh { None } else { app.cache.get(target.key()).await };
    let (outcome, result): (_, Result<Found, ExtractError>) = match cached {
        Some(Entry { outcome, .. }) => ("from the cache", outcome.map(|post| (post, None))),
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
                    ("extracted", Ok((Arc::new(post), Some(timings))))
                }
                Err(e) => ("failed", Err(e)),
            }
        }
    };
    let mut tried = Tried {
        source: target.source.as_str(),
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
        Err(e) => tried.error = Some(e.to_string()),
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
            let summary = preview::summary(&post, &media, None, &settings, target);
            // The author's pictures, which a browser won't show from Meta's CDN on this page.
            let mut shown = (*post).clone();
            if let Some(url) = &post.profile.picture {
                shown.profile.picture =
                    Some(inlined(app, &settings, target.source, url).await.unwrap_or_else(|| url.clone()));
            }
            let mut image = summary.image.map(str::to_string);
            if summary.avatar
                && let Some(url) = &image
                && let Some(data) = inlined(app, &settings, target.source, url).await
            {
                image = Some(data);
            }
            tried.html = framed(&preview::preview(&shown, &media, &settings, target));
            let tags = tried
                .html
                .lines()
                .filter(|line| line.starts_with("<meta property=") || line.starts_with("<meta name=\"twitter"))
                .collect::<Vec<_>>()
                .join("\n");
            tried.preview = Some(Mock {
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
