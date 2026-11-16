//! The dashboard's cache page.

use super::*;

/// One cached post or failure.
struct CacheRow {
    source: Option<&'static str>,
    key: String,
    /// `key` for a query string.
    link: String,
    title: String,
    /// `None` for a failure: it never got as far as knowing.
    page_type: Option<&'static str>,
    /// What the post holds, or why it failed.
    detail: String,
    cached: String,
    expires: String,
}

/// Rows the cache page lists at most; the search finds the rest.
const CACHE_ROWS: usize = 100;

#[derive(Template)]
#[template(path = "dashboard/cache.html")]
pub struct CachePage {
    shell: Shell,
    s: Settings,
    posts: usize,
    failed: usize,
    size: String,
    bytes: u64,
    limit: u64,
    site: Option<Source>,
    sites: [Source; 2],
    q: String,
    rows: Vec<CacheRow>,
    shown: usize,
    matching: usize,
}

/// What the Cache page calls an entry, and searches by with its link.
fn cache_title(entry: &Entry) -> &str {
    match &entry.outcome {
        Ok(post) => post.title.as_deref().or(post.author.as_deref()).unwrap_or("Untitled"),
        Err(_) => "Couldn't load",
    }
}

fn cache_row(key: &str, entry: &Entry) -> CacheRow {
    let (page_type, detail) = match &entry.outcome {
        Ok(post) => {
            let photos = post.photo_count();
            let videos = post.media.len() - photos;
            let detail = match (photos, videos) {
                (0, 0) => "text".to_string(),
                (p, 0) => plural(p, "photo"),
                (0, v) => plural(v, "video"),
                (p, v) => format!("{}, {}", plural(p, "photo"), plural(v, "video")),
            };
            (Some(post.page_type.as_str()), detail)
        }
        Err(e) => (None, format!("{}: {}", e.stage.as_str(), e.message)),
    };
    CacheRow {
        source: Source::of_link(key).map(Source::as_str),
        link: url::form_urlencoded::byte_serialize(key.as_bytes()).collect(),
        key: key.to_string(),
        title: cache_title(entry).to_string(),
        page_type,
        detail,
        cached: ago(entry.stored_at),
        expires: until(entry.expires_at()),
    }
}

fn until(at: i64) -> String {
    match (at - now()).max(0) {
        0..60 => "in under a minute".into(),
        s @ 60..3600 => format!("in {} min", s / 60),
        s => format!("in {} h", s / 3600),
    }
}

impl CachePage {
    fn is_site(&self, s: &Source) -> bool {
        self.site == Some(*s)
    }
}

async fn cache_page(app: &App, shell: Shell, s: Settings, q: &str, site: Option<Source>) -> CachePage {
    let limit = s.cache_megabytes * 1_048_576;
    let (_, bytes) = app.cache.usage().await;
    let all = app.cache.entries().await;
    let failed = all.iter().filter(|(_, entry)| entry.outcome.is_err()).count();
    let needle = q.trim().to_lowercase();
    let matches = |key: &str, entry: &Entry| {
        site.is_none_or(|s| Source::of_link(key) == Some(s))
            && (needle.is_empty()
                || key.to_lowercase().contains(&needle)
                || cache_title(entry).to_lowercase().contains(&needle))
    };
    // The cache can hold far more than a page: only the newest matches are sorted and shown.
    let mut found: Vec<&(Arc<String>, Entry)> = all.iter().filter(|(key, entry)| matches(key, entry)).collect();
    let matching = found.len();
    let newest_first = |(_, entry): &&(Arc<String>, Entry)| std::cmp::Reverse(entry.stored_at);
    if found.len() > CACHE_ROWS {
        found.select_nth_unstable_by_key(CACHE_ROWS, newest_first);
        found.truncate(CACHE_ROWS);
    }
    found.sort_unstable_by_key(newest_first);
    let rows: Vec<CacheRow> = found.into_iter().map(|(key, entry)| cache_row(key, entry)).collect();
    CachePage {
        shell,
        s,
        posts: all.len() - failed,
        failed,
        size: size(bytes),
        bytes,
        limit,
        site,
        sites: Source::ALL,
        q: q.trim().to_string(),
        shown: rows.len(),
        matching,
        rows,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CacheQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    site: String,
    done: Option<String>,
}

pub async fn cache(State(d): State<Shared>, Query(q): Query<CacheQuery>) -> Response {
    let shell = Shell::new("cache", &Done { done: q.done.clone() });
    let site = Source::from_name(&q.site);
    render(&cache_page(&d.app, shell, (*d.app.settings.get()).clone(), &q.q, site).await)
}

#[derive(Deserialize)]
pub struct CacheForm {
    cache_minutes: u64,
    failure_cache_minutes: u64,
    cache_megabytes: u64,
}

pub async fn save_cache(State(d): State<Shared>, Form(f): Form<CacheForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.cache_minutes = f.cache_minutes;
    s.failure_cache_minutes = f.failure_cache_minutes;
    s.cache_megabytes = f.cache_megabytes;
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/cache", "saved"),
        Err(e) => refused(&cache_page(&d.app, Shell::refused("cache", format!("{e:#}")), s, "", None).await),
    }
}

/// A link to forget, or a cache key from the list.
#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    key: String,
}

pub async fn cache_remove(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    if !f.key.is_empty() {
        d.app.cache.remove(&f.key).await;
        return done("/cache", "forgotten");
    }
    let Some(target) = Target::from_link(f.url.trim()) else {
        let shell = Shell::refused("cache", "That isn't a facebook.com link.");
        return refused(&cache_page(&d.app, shell, (*d.app.settings.get()).clone(), "", None).await);
    };
    d.app.cache.remove(target.key()).await;
    done("/cache", "forgotten")
}

pub async fn cache_clear(State(d): State<Shared>) -> Response {
    d.app.cache.clear();
    done("/cache", "cleared")
}
