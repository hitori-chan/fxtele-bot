//! The dashboard's rate limits page.

use super::*;

/// A site's fetch budget as the form shows it.
struct SiteBudget {
    source: Source,
    fetches_per_minute: u32,
    concurrency: u32,
    in_use: u32,
    exhausted: u64,
}

#[derive(Template)]
#[template(path = "dashboard/limits.html")]
pub struct LimitsPage {
    shell: Shell,
    s: Settings,
    sites: Vec<SiteBudget>,
}

fn limits_page(app: &App, shell: Shell, s: Settings) -> LimitsPage {
    let sites = Source::ALL
        .into_iter()
        .map(|source| SiteBudget {
            source,
            fetches_per_minute: s.source(source).fetches_per_minute,
            concurrency: s.source(source).concurrency,
            in_use: app.budgets.of(source).in_use(),
            exhausted: app.budgets.of(source).exhausted(),
        })
        .collect();
    LimitsPage { shell, s, sites }
}

pub async fn limits(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&limits_page(&d.app, Shell::new("limits", &q), (*d.app.settings.get()).clone()))
}

/// Visitors' limit, the timeout, and `<site>_fetches_per_minute` and `<site>_concurrency` for
/// every site.
pub async fn save_limits(State(d): State<Shared>, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    // Named as the page labels the field.
    let read = |name: &str, label: &str| -> Result<u32, String> {
        let value = f.get(name).map(|v| v.trim()).unwrap_or_default();
        value.parse().map_err(|_| format!("{label} must be a whole number, not {value:?}."))
    };
    let parsed = (|| -> Result<(), String> {
        s.ip_requests_per_minute = read("ip_requests_per_minute", "Requests per IP")?;
        s.extract_timeout_seconds = read("extract_timeout_seconds", "The extraction timeout")?.into();
        for source in Source::ALL {
            let site = source.name();
            let fetches = read(&format!("{}_fetches_per_minute", source.as_str()), &format!("{site}'s fetches"))?;
            let concurrency = read(&format!("{}_concurrency", source.as_str()), &format!("{site}'s fetches at once"))?;
            let site = s.source_mut(source);
            (site.fetches_per_minute, site.concurrency) = (fetches, concurrency);
        }
        Ok(())
    })();
    let result = match parsed {
        Ok(()) => d.app.settings.set(s.clone()).await.map_err(|e| format!("{e:#}")),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => done("/limits", "saved"),
        Err(e) => refused(&limits_page(&d.app, Shell::refused("limits", e), s)),
    }
}
