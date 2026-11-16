//! The request log: the latest requests, newest first, with new ones added live. Kept in memory
//! only, so a restart empties it.

use super::*;
use crate::stats::Event;

/// The log's filters: what kind of request (`telegram`, `failed`, `redirect`) and a search.
pub const KINDS: [(&str, &str); 4] =
    [("", "All"), ("telegram", "Telegram"), ("failed", "Failed"), ("redirect", "People")];

#[derive(Template)]
#[template(path = "dashboard/requests.html")]
pub struct RequestsPage {
    shell: Shell,
    site: Option<Source>,
    sites: [Source; 2],
    kind: String,
    kinds: [(&'static str, &'static str); 4],
    search: String,
    rows: Vec<Logged>,
    keep: usize,
    telegram: usize,
    failed: usize,
    people: usize,
}

impl RequestsPage {
    fn is_site(&self, s: &Source) -> bool {
        self.site == Some(*s)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RequestFilter {
    #[serde(default)]
    site: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    q: String,
}

pub async fn requests(State(d): State<Shared>, Query(f): Query<RequestFilter>) -> Response {
    let site = Source::from_name(&f.site);
    let search = f.q.trim().to_lowercase();
    let all: Vec<Logged> =
        d.app.stats.recent().iter().filter(|e| site.is_none_or(|s| s.as_str() == e.source)).map(Logged::of).collect();
    let count = |kind: &str| all.iter().filter(|r| r.kind == kind).count();
    let (telegram, failed, people) = (count("telegram") + count("failed"), count("failed"), count("redirect"));
    let rows = all
        .into_iter()
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
    render(&RequestsPage {
        shell: Shell::new("requests", &Done::default()),
        site,
        sites: Source::ALL,
        kind: f.kind,
        kinds: KINDS,
        search: f.q,
        rows,
        keep: crate::stats::KEEP_RECENT,
        telegram,
        failed,
        people,
    })
}

/// A request in a log, as pages show it.
pub struct Logged {
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
}

impl Logged {
    pub fn of(e: &Event) -> Self {
        let failed = e.outcome.starts_with("failed");
        let (level, outcome) = match e.outcome {
            "hit" => (OK, "cache hit".to_string()),
            "extracted" => (OK, "extracted".to_string()),
            "redirect" => (IDLE, "redirected".to_string()),
            "busy" => (WARN, "no budget".to_string()),
            "rate_limited" => (WARN, "rate limited".to_string()),
            o if o.starts_with("failed_cached:") => {
                (BAD, format!("{}, cached", o.trim_start_matches("failed_cached:").replace('_', " ")))
            }
            o => (BAD, o.trim_start_matches("failed:").replace('_', " ")),
        };
        // Telegram's addresses aren't rate limited, so those are strangers too.
        let kind = if e.outcome == "redirect" || e.outcome == "rate_limited" {
            "redirect"
        } else if failed {
            "failed"
        } else {
            "telegram"
        };
        let time = crate::dashboard::local(e.at).map_or_else(|| "?".into(), |t| t.strftime("%H:%M:%S").to_string());
        Self {
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
        }
    }
}
