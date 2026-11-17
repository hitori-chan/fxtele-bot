//! The request log: the latest requests, newest first, with new ones added live. Kept in memory
//! only, so a restart empties it.

use super::pager::{Pager, address, page_number};
use super::*;
use crate::stats::Event;

/// Requests a page; the live first page keeps this many as new ones come in.
const PAGE: usize = 100;

/// The log's filters: what kind of request (`telegram` for the cards Telegram and Discord got,
/// `failed`, `redirect`) and a search.
const KINDS: [(&str, &str); 4] = [("", "All"), ("telegram", "Cards"), ("failed", "Failed"), ("redirect", "People")];

#[derive(Template)]
#[template(path = "dashboard/requests.html")]
pub struct RequestsPage {
    shell: Shell,
    site: Option<Source>,
    sites: Vec<Choice>,
    kind: String,
    kinds: Vec<Choice>,
    search: String,
    rows: Vec<Logged>,
    pager: Pager,
    /// The first page, with new requests added as they come in; other pages stay as they were.
    live: bool,
    /// The newest request on a page that stays put, and the log as it is now.
    as_of: Option<String>,
    live_href: String,
    page_size: usize,
    keep: usize,
    telegram: usize,
    failed: usize,
    people: usize,
}

#[derive(Debug, Default, Deserialize)]
pub struct RequestFilter {
    #[serde(default)]
    site: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: String,
    /// The newest request the pages hold, so they stay put as new ones come in.
    #[serde(default)]
    upto: String,
}

pub async fn requests(State(d): State<Shared>, Query(f): Query<RequestFilter>) -> Response {
    let site = Source::from_name(&f.site);
    let search = f.q.trim().to_lowercase();
    let all: Vec<Logged> =
        d.app.stats.recent().iter().filter(|e| site.is_none_or(|s| s.as_str() == e.source)).map(Logged::of).collect();
    let count = |kind: &str| all.iter().filter(|r| r.kind == kind).count();
    let (telegram, failed, people) = (count("telegram") + count("failed"), count("failed"), count("redirect"));
    let upto: Option<u64> = f.upto.trim().parse().ok();
    let rows: Vec<Logged> = all
        .into_iter()
        .filter(|r| upto.is_none_or(|upto| r.seq <= upto))
        .filter(|r| match f.kind.as_str() {
            "telegram" => r.kind != "redirect",
            "failed" | "redirect" => r.kind == f.kind,
            _ => true,
        })
        .filter(|r| {
            search.is_empty()
                || r.page_type.contains(&search)
                || r.link.as_deref().is_some_and(|l| l.to_lowercase().contains(&search))
        })
        .collect();
    let filters = [("site", site.map_or("", Source::as_str)), ("kind", f.kind.as_str()), ("q", f.q.trim())];
    let anchor = upto.or_else(|| rows.first().map(|r| r.seq)).map(|n| n.to_string()).unwrap_or_default();
    let mut pairs = filters.to_vec();
    pairs.push(("upto", &anchor));
    let pager = Pager::new(rows.len(), PAGE, page_number(&f.page), "/requests", &pairs);
    let live = upto.is_none() && pager.page == 1;
    let as_of = (!live).then(|| rows.first().map(|r| r.time.clone())).flatten();
    let rows = rows.into_iter().skip(pager.rows().start).take(pager.rows().len()).collect();
    let live_href = address("/requests", &filters);
    render(&RequestsPage {
        shell: Shell::new("requests", &Done::default()),
        site,
        sites: site_choices("/requests", site, &[("kind", &f.kind), ("q", f.q.trim())]),
        kind: f.kind.clone(),
        kinds: KINDS
            .iter()
            .map(|&(kind, label)| Choice {
                label,
                icon: None,
                href: address("/requests", &[("site", filters[0].1), ("kind", kind), ("q", f.q.trim())]),
                current: f.kind == kind,
                count: None,
            })
            .collect(),
        search: f.q.clone(),
        rows,
        pager,
        live,
        as_of,
        live_href,
        page_size: PAGE,
        keep: crate::stats::KEEP_RECENT,
        telegram,
        failed,
        people,
    })
}

/// One row of the log, as the live log adds it: the same as the page's own rows.
#[derive(Template)]
#[template(path = "dashboard/request.html")]
pub struct Row<'a> {
    r: &'a Logged,
}

impl Logged {
    /// The row's HTML.
    pub fn render(&self) -> askama::Result<String> {
        Row { r: self }.render()
    }
}

/// A request in a log, as pages show it.
pub struct Logged {
    pub seq: u64,
    pub time: String,
    pub source: &'static str,
    /// "reel", or "refused" for requests refused before a page was read.
    pub page_type: String,
    pub kind: &'static str,
    pub level: &'static str,
    pub outcome: String,
    pub ms: String,
    pub link: Option<String>,
    /// `link` for a query string.
    pub encoded: String,
    /// Discord's card, which the row says; Telegram's are the rest.
    pub discord: bool,
}

impl Logged {
    pub fn of(e: &Event) -> Self {
        let failed = e.outcome.starts_with("failed");
        let step = |code: &str| Stage::parse(code).map_or_else(|| code.replace('_', " "), |s| s.label().to_string());
        let (level, outcome) = match e.outcome {
            "hit" => (OK, "Cache hit".to_string()),
            "extracted" => (OK, "Extracted".to_string()),
            "redirect" => (IDLE, "Redirected".to_string()),
            "busy" => (WARN, "No budget".to_string()),
            "rate_limited" => (WARN, "Too many requests".to_string()),
            o if o.starts_with("failed_cached:") => {
                (BAD, format!("{}, cached", step(o.trim_start_matches("failed_cached:"))))
            }
            o => (BAD, step(o.trim_start_matches("failed:"))),
        };
        // Telegram's addresses aren't rate limited, so those are strangers too.
        let kind = if e.outcome == "redirect" || e.outcome == "rate_limited" {
            "redirect"
        } else if failed {
            "failed"
        } else {
            "telegram"
        };
        // The day too, once it isn't today's.
        let time = crate::dashboard::local(e.at).map_or_else(
            || "?".into(),
            |t| {
                let today = crate::dashboard::local(now()).is_some_and(|n| n.date() == t.date());
                t.strftime(if today { "%H:%M:%S" } else { "%-d %b, %H:%M:%S" }).to_string()
            },
        );
        Self {
            seq: e.seq,
            time,
            source: e.source,
            page_type: if e.page_type == "-" { "refused".into() } else { e.page_type.replace('_', " ") },
            kind,
            level,
            outcome,
            ms: millis(e.ms),
            encoded: e
                .link
                .as_deref()
                .map(|l| url::form_urlencoded::byte_serialize(l.as_bytes()).collect())
                .unwrap_or_default(),
            link: e.link.clone(),
            discord: e.by == "discord",
        }
    }
}
