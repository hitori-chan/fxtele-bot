//! The dashboard's cache page: what's cached, newest first, by site and searchable, a page at a
//! time; and its settings.

use super::pager::{Pager, address, page_number};
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
    /// The step a failure stopped at; its message is the title.
    failed: Option<&'static str>,
    /// What the post holds.
    detail: String,
    cached: String,
    expires: String,
}

/// Posts a page.
const PAGE: usize = 50;

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
    sites: Vec<Choice>,
    q: String,
    rows: Vec<CacheRow>,
    pager: Pager,
    /// What was typed in Forget, kept when it was refused.
    forget: String,
}

/// What the Cache page calls an entry, and searches by with its link.
fn cache_title(entry: &Entry) -> &str {
    match &entry.outcome {
        Ok(post) => post.title.as_deref().or(post.author.as_deref()).unwrap_or("Untitled"),
        Err(e) => &e.message,
    }
}

fn cache_row(key: &str, entry: &Entry) -> CacheRow {
    let failed = entry.outcome.as_ref().err().map(|e| e.stage.label());
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
        Err(_) => (None, String::new()),
    };
    CacheRow {
        source: Source::of_link(key).map(Source::as_str),
        link: url::form_urlencoded::byte_serialize(key.as_bytes()).collect(),
        key: key.to_string(),
        title: cache_title(entry).to_string(),
        page_type,
        failed,
        detail,
        cached: ago(entry.stored_at),
        expires: until(entry.expires_at()),
    }
}

async fn cache_page(
    app: &App,
    shell: Shell,
    s: Settings,
    q: &str,
    site: Option<Source>,
    page: Option<usize>,
) -> CachePage {
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
    let mut found: Vec<&(Arc<String>, Entry)> = all.iter().filter(|(key, entry)| matches(key, entry)).collect();
    // Newest first, and the same order every time for posts stored in the same instant.
    found.sort_unstable_by(|(a_key, a), (b_key, b)| b.stored_at.cmp(&a.stored_at).then_with(|| a_key.cmp(b_key)));
    let pager =
        Pager::new(found.len(), PAGE, page, "/cache", &[("site", site.map_or("", Source::as_str)), ("q", q.trim())]);
    let rows: Vec<CacheRow> = found[pager.rows()].iter().map(|(key, entry)| cache_row(key, entry)).collect();
    CachePage {
        shell,
        s,
        posts: all.len() - failed,
        failed,
        size: size(bytes),
        bytes,
        limit,
        site,
        sites: site_choices("/cache", site, &[("q", q.trim())]),
        q: q.trim().to_string(),
        rows,
        pager,
        forget: String::new(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CacheQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    site: String,
    #[serde(default)]
    page: String,
    done: Option<String>,
}

pub async fn cache(State(d): State<Shared>, Query(q): Query<CacheQuery>) -> Response {
    let shell = Shell::new("cache", &Done { done: q.done.clone() });
    let site = Source::from_name(&q.site);
    render(&cache_page(&d.app, shell, (*d.app.settings.get()).clone(), &q.q, site, page_number(&q.page)).await)
}

#[derive(Deserialize)]
pub struct CacheForm {
    cache_minutes: u64,
    failure_cache_minutes: u64,
    cache_megabytes: u64,
}

pub async fn save_cache(State(d): State<Shared>, Form(f): Form<CacheForm>) -> Response {
    let apply = |s: &mut Settings| {
        s.cache_minutes = f.cache_minutes;
        s.failure_cache_minutes = f.failure_cache_minutes;
        s.cache_megabytes = f.cache_megabytes;
    };
    match d.app.settings.edit(|s| apply(s)).await {
        Ok(()) => done("/cache", "saved"),
        Err(e) => {
            let mut entered = (*d.app.settings.get()).clone();
            apply(&mut entered);
            refused(&cache_page(&d.app, Shell::refused("cache", format!("{e:#}")), entered, "", None, None).await)
        }
    }
}

/// A link to forget, or a cache key from the list, with the list it was on to go back to.
#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    site: String,
    #[serde(default)]
    page: String,
}

pub async fn cache_remove(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    if !f.key.is_empty() {
        d.app.cache.remove(&f.key).await;
        let back = [("done", "forgotten"), ("site", f.site.as_str()), ("q", f.q.as_str()), ("page", f.page.as_str())];
        return Redirect::to(&address("/cache", &back)).into_response();
    }
    let Some(target) = Target::from_link(f.url.trim()) else {
        let shell = Shell::refused("cache", format!("That isn't a link to {}.", Source::listed()));
        let mut page = cache_page(&d.app, shell, (*d.app.settings.get()).clone(), "", None, None).await;
        page.forget = f.url;
        return refused(&page);
    };
    if d.app.cache.remove(target.key()).await { done("/cache", "forgotten") } else { done("/cache", "not_cached") }
}

pub async fn cache_clear(State(d): State<Shared>) -> Response {
    d.app.cache.clear();
    done("/cache", "cleared")
}
