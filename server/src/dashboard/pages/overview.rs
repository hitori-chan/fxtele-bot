//! The overview, for one site or every site: what needs a look, four key numbers against the
//! period before, requests and fetch times over time, top issues, each site and its kinds of
//! post against its budget, and the media Telegram fetched after the previews.

use super::*;
use crate::stats::Media;

/// Request counts over a window, split the way the overview shows them.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    telegram: u64,
    hits: u64,
    success: u64,
    failed: u64,
    /// No fetch budget was free in time.
    busy: u64,
    redirects: u64,
    /// Every answer to Telegram, cache hits included: how long Telegram waited.
    answers: Agg,
    /// Fetches from the sites, whoever they were for: how long fetching took.
    fetches: Agg,
}

impl Counts {
    fn add(&mut self, outcome: &str, agg: Agg) {
        if outcome == crate::stats::FETCH {
            self.fetches.merge(agg);
            return;
        }
        match outcome {
            "redirect" => self.redirects += agg.count,
            "rate_limited" => {}
            _ => {
                self.telegram += agg.count;
                self.answers.merge(agg);
                if outcome == "busy" {
                    self.busy += agg.count;
                } else if outcome == "hit" {
                    self.hits += agg.count;
                    self.success += agg.count;
                } else if outcome == "extracted" {
                    self.success += agg.count;
                } else if outcome.starts_with("failed") {
                    self.failed += agg.count;
                }
            }
        }
    }
}

/// Totals over a window: in all, by site, and by page (`instagram/reel`).
#[derive(Default)]
struct Window {
    total: Counts,
    by_site: BTreeMap<String, Counts>,
    by_page: BTreeMap<String, Counts>,
}

async fn window(app: &App, since: i64, until: i64, site: Option<Source>) -> Result<Window> {
    let mut w = Window::default();
    for ((source, outcome, page_type), agg) in app.stats.totals(since, until).await? {
        if site.is_some_and(|s| s.as_str() != source) {
            continue;
        }
        w.total.add(&outcome, agg);
        w.by_site.entry(source.clone()).or_default().add(&outcome, agg);
        if page_type != "-" {
            w.by_page.entry(format!("{source}/{page_type}")).or_default().add(&outcome, agg);
        }
    }
    Ok(w)
}

/// `?range=…&site=…` for the overview and the CSV export.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQuery {
    #[serde(default)]
    range: String,
    #[serde(default)]
    site: String,
    done: Option<String>,
}

impl StatsQuery {
    fn range(&self) -> Range {
        Range::parse(&self.range).unwrap_or_default()
    }

    fn site(&self) -> Option<Source> {
        Source::from_name(&self.site)
    }
}

/// A change against the period before: its colour (`good`, `bad` or `flat`), arrow and size.
pub struct Delta {
    class: &'static str,
    icon: &'static str,
    text: String,
}

impl Delta {
    /// `higher_is`: `Some(true)` when up is good, `Some(false)` when down is, `None` for neither.
    fn new(change: f64, unit: &str, higher_is: Option<bool>, decimals: usize) -> Self {
        if format!("{:.decimals$}", change.abs()).trim_start_matches(['0', '.']).is_empty() {
            return Self { class: "flat", icon: "", text: format!("0{unit}") };
        }
        let class = match higher_is {
            Some(good) if good == (change > 0.0) => "good",
            Some(_) => "bad",
            None => "flat",
        };
        let icon = if change > 0.0 { "arrow-up" } else { "arrow-down" };
        Self { class, icon, text: format!("{:.decimals$}{unit}", change.abs()) }
    }

    /// Percent change in a count; none from nothing.
    fn count(now: u64, before: u64, higher_is: Option<bool>) -> Option<Self> {
        (before > 0).then(|| Self::new((now as f64 - before as f64) * 100.0 / before as f64, "%", higher_is, 0))
    }

    /// Change in a rate, in percentage points to one decimal: near 100%, a tenth is a lot.
    fn rate(now: (u64, u64), before: (u64, u64)) -> Option<Self> {
        let rate = |(part, whole): (u64, u64)| (whole > 0).then(|| part as f64 * 100.0 / whole as f64);
        Some(Self::new(rate(now)? - rate(before)?, " pts", Some(true), 1))
    }
}

pub struct Kpi {
    label: &'static str,
    /// What it counts, on hover.
    tip: &'static str,
    icon: &'static str,
    value: String,
    delta: Option<Delta>,
    meta: String,
    href: Option<String>,
    spark: Option<Spark>,
}

/// A line of the sites table: Telegram's requests, how many worked, came from the cache, and the
/// slowest fetches.
struct Line {
    telegram: u64,
    success: String,
    hits: String,
    p95: String,
}

impl Line {
    fn of(c: &Counts) -> Self {
        Self {
            telegram: c.telegram,
            success: percent(c.success, c.telegram),
            hits: percent(c.hits, c.telegram),
            p95: c.fetches.percentile(95).map_or_else(|| "–".into(), millis),
        }
    }
}

/// A site in the sites table: its status, its budget against its busiest minute, its totals,
/// and a line per kind of post.
struct Group {
    source: Source,
    status: Status,
    /// "busiest minute 14 of 60 fetches".
    budget: String,
    /// The busiest minute used most of the budget.
    tight: bool,
    total: Line,
    kinds: Vec<(String, Line)>,
}

/// One kind of media and how often it went out.
struct Served {
    label: &'static str,
    icon: &'static str,
    value: u64,
    meta: String,
    level: &'static str,
}

/// Something worth doing before the dashboard is much use.
struct Step {
    href: &'static str,
    icon: &'static str,
    title: &'static str,
    detail: &'static str,
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
pub struct Overview {
    shell: Shell,
    proxy_warning: bool,
    range: Range,
    ranges: [Range; 5],
    site: Option<Source>,
    sites: Vec<Choice>,
    /// This page's filters as a query string, for links that keep them.
    query: String,
    steps: Vec<Step>,
    /// Checks that need a look; none when all is well.
    checks: Vec<Check>,
    kpis: Vec<Kpi>,
    chart: Chart,
    latency: chart::Latency,
    groups: Vec<Group>,
    served: Vec<Served>,
    issues: Vec<Issue>,
}

pub async fn overview(State(d): State<Shared>, Query(q): Query<StatsQuery>, headers: HeaderMap) -> Response {
    let shell = Shell::new("overview", &Done { done: q.done.clone() });
    // The minutely refresh keeps only the numbers (`#stats`), so the checks above them are skipped.
    let numbers_only = headers.get("hx-target").is_some_and(|t| t == "stats");
    show(overview_page(&d.app, shell, &q, numbers_only).await)
}

/// What still needs setting up; nothing once it's done.
fn steps(app: &App, settings: &Settings) -> Vec<Step> {
    let mut steps = Vec::new();
    if settings.test_links.is_empty() {
        steps.push(Step {
            href: "/tests",
            icon: "tests",
            title: "Add test links",
            detail: "Checked every 30 minutes, so a site's change shows up before people notice.",
        });
    }
    if app.config.telegram_bot_token.is_none() || settings.alert_chat.is_none() {
        steps.push(Step {
            href: "/alerts",
            icon: "bell",
            title: "Get alerts in Telegram",
            detail: "Failing previews, broken test links and cookies that stop working.",
        });
    }
    if app.config.instagram_host.is_none() {
        steps.push(Step {
            href: "/sites/instagram",
            icon: "instagram",
            title: "Give Instagram its host",
            detail: "Set INSTAGRAM_HOST, so Instagram links get previews too.",
        });
    }
    steps
}

async fn overview_page(app: &App, shell: Shell, q: &StatsQuery, numbers_only: bool) -> Result<Overview> {
    let settings = app.settings.get();
    let (range, site, now) = (q.range(), q.site(), now());
    let seconds = range.seconds();
    // The same window as the chart's bars, and the one as long before it.
    let since = range.since(now);
    let current = window(app, since, now + 60, site).await?;
    // Statistics go back 90 days, so the longest range has nothing to compare with.
    let before = if range == Range::Quarter { None } else { Some(window(app, since - seconds, since, site).await?) };
    let (c, b) = (current.total, before.as_ref().map(|w| w.total));
    let site_name = site.map(Source::as_str);
    let series = app.stats.series(range.since(now), range.step(), site_name).await?;

    // Per-period numbers for the key numbers' sparklines, a period a chart bar.
    let periods: Vec<Counts> = chart::slotted(range, now, series.iter().map(|(bucket, o, agg)| (*bucket, (o, *agg))))
        .into_iter()
        .map(|parts| {
            let mut counts = Counts::default();
            for (outcome, agg) in parts {
                counts.add(outcome, agg);
            }
            counts
        })
        .collect();
    let rate = |part: u64, whole: u64| (part * 1000).checked_div(whole).unwrap_or(0);
    let sparked = |f: &dyn Fn(&Counts) -> u64| Some(chart::spark(&periods.iter().map(f).collect::<Vec<_>>()));
    // A rate or a time has no value where nothing happened: the line stays where it was rather
    // than dropping to zero, which would read as an outage.
    let held = |f: &dyn Fn(&Counts) -> Option<u64>| {
        let mut last = None;
        let values: Vec<u64> = periods
            .iter()
            .map(|p| {
                last = f(p).or(last);
                last.unwrap_or(0)
            })
            .collect();
        Some(chart::spark(&values))
    };
    let ms = |agg: &Agg, p| agg.percentile(p).map_or_else(|| "–".to_string(), millis);
    let filter = site.map_or_else(String::new, |s| format!("?site={}", s.as_str()));
    let query =
        format!("range={}{}", range.as_str(), site.map_or_else(String::new, |s| format!("&site={}", s.as_str())));
    let busy = if c.busy > 0 { format!(" · {} out of budget", c.busy) } else { String::new() };
    let kpis = vec![
        Kpi {
            label: "Success rate",
            tip: "Telegram's requests that got a preview, from the cache or freshly extracted",
            icon: "check-circle",
            value: precise_percent(c.success, c.telegram),
            delta: b.and_then(|b| Delta::rate((c.success, c.telegram), (b.success, b.telegram))),
            meta: format!("{} failed{busy}", c.failed),
            href: Some(format!("/issues{filter}")),
            spark: held(&|p| (p.telegram > 0).then(|| rate(p.success, p.telegram))),
        },
        Kpi {
            label: "Telegram requests",
            tip: "Requests from Telegram's addresses with its user agent; people opening a link are sent to the post",
            icon: "telegram",
            value: c.telegram.to_string(),
            delta: b.and_then(|b| Delta::count(c.telegram, b.telegram, None)),
            meta: format!("{} people clicked through", c.redirects),
            href: Some(format!("/requests{filter}")),
            spark: sparked(&|p| p.telegram),
        },
        Kpi {
            label: "Answer time, p95",
            tip: "How long Telegram waited for an answer, cache hits included; 95% were quicker",
            icon: "timer",
            value: ms(&c.answers, 95),
            delta: b.and_then(|b| Delta::count(c.answers.percentile(95)?, b.answers.percentile(95)?, Some(false))),
            meta: format!("median {}", ms(&c.answers, 50)),
            href: None,
            spark: held(&|p| p.answers.percentile(95)),
        },
        Kpi {
            label: "Fetches to the sites",
            tip: "Fetches from the sites, for Telegram, test links, the playground and expired media links: what the sites see of this server",
            icon: "globe",
            value: c.fetches.count.to_string(),
            delta: b.and_then(|b| Delta::count(c.fetches.count, b.fetches.count, None)),
            meta: format!("{} from the cache", percent(c.hits, c.telegram)),
            href: Some("/cache".into()),
            spark: sparked(&|p| p.fetches.count),
        },
    ];

    let peaks = app.stats.peak_fetches(now - seconds).await?;
    let groups = Source::ALL
        .into_iter()
        .filter(|s| site.is_none_or(|only| only == *s))
        .map(|source| {
            let total = current.by_site.get(source.as_str()).copied().unwrap_or_default();
            let per_minute = settings.source(source).fetches_per_minute;
            let peak = peaks.get(source.as_str()).copied().unwrap_or(0);
            let prefix = format!("{}/", source.as_str());
            Group {
                source,
                status: health::site_status(app, source),
                budget: format!("busiest minute {peak} of {per_minute} fetches"),
                tight: peak * 5 >= u64::from(per_minute) * 4,
                total: Line::of(&total),
                kinds: current
                    .by_page
                    .iter()
                    .filter_map(|(page, c)| Some((page.strip_prefix(&prefix)?, c)))
                    .filter(|(_, c)| c.telegram > 0)
                    .map(|(kind, c)| (kind.replace('_', " "), Line::of(c)))
                    .collect(),
            }
        })
        .collect();

    let media = app.stats.media_totals(now - seconds, now + 60, site_name).await?;
    let count = |kind| media.get(&kind).copied().unwrap_or(0);
    let (videos, hd, fallback) = (count(Media::Video), count(Media::Video1080), count(Media::Fallback));
    let mut served = vec![
        Served {
            label: "Photos",
            icon: "previews",
            value: count(Media::Image),
            meta: if settings.media_links == MediaLinks::Redirect {
                "and video thumbnails, through this server".into()
            } else {
                "and video thumbnails, linked to the sites".into()
            },
            level: "",
        },
        Served {
            label: "Videos",
            icon: "videos",
            value: videos + hd + fallback,
            meta: if hd + fallback == 0 {
                "the sites' own 720p files".into()
            } else {
                format!("{} at 720p", videos + fallback)
            },
            level: "",
        },
    ];
    if settings.video_quality == VideoQuality::Best || hd + fallback > 0 {
        served.push(Served {
            label: "1080p from the bucket",
            icon: "server",
            value: hd,
            meta: format!("{} of those asked for", percent(hd, hd + fallback)),
            level: "",
        });
        served.push(Served {
            label: "Fell back to 720p",
            icon: "alert",
            value: fallback,
            meta: if fallback == 0 { "none".into() } else { "too slow or failed; see Videos".into() },
            level: if fallback == 0 { "" } else { WARN },
        });
    }
    served.push(Served {
        label: "Mosaics",
        icon: "overview",
        value: count(Media::Mosaic),
        meta: "several photos as one".into(),
        level: "",
    });

    let failures = app.stats.failures(500).await?;
    let checks = if numbers_only {
        Vec::new()
    } else {
        health::checks(app).await?.into_iter().filter(|c| matches!(c.status.level, WARN | BAD)).collect()
    };
    Ok(Overview {
        shell,
        proxy_warning: app.stats.proxy_warning(),
        range,
        ranges: Range::ALL,
        site,
        sites: site_choices("/", site, &[("range", range.as_str())]),
        query,
        steps: if numbers_only { Vec::new() } else { steps(app, &settings) },
        checks,
        kpis,
        chart: chart::requests(range, &series, now),
        latency: chart::latency(range, &series, now, millis),
        groups,
        served,
        issues: issues(&failures, now - seconds, site).into_iter().take(5).collect(),
    })
}

/// "98.7%": a success rate needs the decimal, since the ones worth watching are all in the nineties.
/// A rate to a tenth, rounded down: 9,996 of 10,000 is 99.9%, never 100% while any failed.
fn precise_percent(part: u64, whole: u64) -> String {
    match (part * 1000).checked_div(whole) {
        None => "–".into(),
        Some(1000) => "100%".into(),
        Some(p) => format!("{}.{}%", p / 10, p % 10),
    }
}

/// A site's last day in four numbers, for its page: label, icon, value, and what's under it.
pub async fn site_numbers(app: &App, source: Source) -> Result<Vec<(&'static str, &'static str, String, String)>> {
    let now = now();
    let c = window(app, now - 86_400, now + 60, Some(source)).await?.total;
    let ms = |p| c.fetches.percentile(p).map_or_else(|| "–".to_string(), millis);
    Ok(vec![
        ("Telegram requests", "telegram", c.telegram.to_string(), format!("{} people redirected", c.redirects)),
        ("Success rate", "check-circle", percent(c.success, c.telegram), format!("{} failed", c.failed)),
        ("Cache hit rate", "cache", percent(c.hits, c.telegram), format!("{} fetched", c.fetches.count)),
        ("Fetch time, p95", "timer", ms(95), format!("median {}", ms(50))),
    ])
}

/// Requests per period and outcome, as CSV.
pub async fn export_requests(State(d): State<Shared>, Query(q): Query<StatsQuery>) -> Response {
    let (range, now) = (q.range(), now());
    let rows = match d.app.stats.series(range.since(now), range.step(), q.site().map(Source::as_str)).await {
        Ok(rows) => rows,
        Err(e) => return server_error(&e),
    };
    let mut csv = String::from("start_utc,outcome,requests,mean_ms,p95_ms\n");
    for (bucket, outcome, agg) in rows {
        let start = jiff::Timestamp::from_second(bucket).map(|t| t.to_string()).unwrap_or_default();
        let (mean, p95) = (agg.mean().unwrap_or(0), agg.percentile(95).unwrap_or(0));
        csv.push_str(&format!("{start},{outcome},{},{mean},{p95}\n", agg.count));
    }
    let name = format!("attachment; filename=\"requests-{}.csv\"", range.as_str());
    ([(CONTENT_TYPE, "text/csv; charset=utf-8")], [(CONTENT_DISPOSITION, name)], csv).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg(count: u64, ms: u64) -> Agg {
        let mut agg = Agg::default();
        for _ in 0..count {
            agg.merge(Agg::one(ms));
        }
        agg
    }

    #[test]
    fn counts_split_outcomes() {
        let mut c = Counts::default();
        c.add("hit", agg(3, 1));
        c.add("extracted", agg(1, 900));
        c.add("failed:login_wall", agg(1, 1200));
        c.add("redirect", agg(5, 1));
        c.add("rate_limited", agg(2, 0));
        c.add("busy", agg(1, 2000));
        c.add("failed_cached:not_found", agg(2, 1));
        c.add("fetch", agg(2, 1200));
        assert_eq!((c.telegram, c.hits, c.success, c.failed, c.busy, c.redirects), (8, 3, 4, 3, 1, 5));
        assert_eq!(
            (c.fetches.count, c.fetches.max_ms),
            (2, 1200),
            "hits, cached failures and busy requests aren't fetches"
        );
        assert_eq!((c.answers.count, c.answers.max_ms), (8, 2000), "Telegram waited on each of its requests");
    }

    #[test]
    fn deltas_say_which_way_is_good() {
        let up = Delta::count(150, 100, Some(true)).unwrap();
        assert_eq!((up.class, up.icon, up.text.as_str()), ("good", "arrow-up", "50%"));
        let slower = Delta::count(1200, 1000, Some(false)).unwrap();
        assert_eq!((slower.class, slower.text.as_str()), ("bad", "20%"));
        assert!(Delta::count(5, 0, None).is_none(), "nothing to compare with");
        let rate = Delta::rate((90, 100), (95, 100)).unwrap();
        assert_eq!((rate.class, rate.icon, rate.text.as_str()), ("bad", "arrow-down", "5.0 pts"));
        assert_eq!(Delta::rate((996, 1000), (999, 1000)).unwrap().text, "0.3 pts", "a tenth near 100% shows");
        assert_eq!(Delta::rate((1, 3), (1, 3)).unwrap().class, "flat");
        assert_eq!(
            (precise_percent(987, 1000), precise_percent(9996, 10_000), precise_percent(1, 1), precise_percent(0, 0)),
            ("98.7%".to_string(), "99.9%".to_string(), "100%".to_string(), "–".to_string())
        );
        assert_eq!((percent(995, 1000), percent(1, 1), percent(1, 3)), ("99%".into(), "100%".into(), "33%".into()));
    }
}
