//! Test links: one list to add, check, try and remove them, each with its latest check and a day
//! of history, by site and state, searchable and a page at a time; the run going on; when the
//! next one is; and which kinds of post each site has covered.

use std::collections::HashMap;

use super::pager::{Pager, address, page_number};
use super::*;
use crate::extract::{PageType, Target};

/// Links a page.
const PAGE: usize = 25;

/// Kinds of post worth a test link, per site. Stories expire within a day, and Facebook's watch
/// links and other pages are read like videos and posts.
const WORTH_TESTING: [(Source, &[PageType]); Source::ALL.len()] = [
    (
        Source::Facebook,
        &[PageType::Post, PageType::Reel, PageType::Video, PageType::Photo, PageType::Album, PageType::Profile],
    ),
    (Source::Instagram, &[PageType::Post, PageType::Reel, PageType::Profile]),
    (Source::Reddit, &[PageType::Post, PageType::Profile]),
    (Source::Threads, &[PageType::Post, PageType::Profile]),
];

/// A tested link: its site and kind, latest check, and history as `ok`/`bad` cells, oldest first.
struct Tested {
    url: String,
    encoded: String,
    source: Option<&'static str>,
    kind: String,
    /// `bad`, `idle` before its first check, or `ok`; also the order they're listed in.
    level: &'static str,
    state: &'static str,
    /// When it was last checked, and what that found.
    checked: String,
    detail: String,
    uptime: String,
    cells: Vec<(&'static str, String)>,
}

/// Which links the list shows, kept through its pages and the buttons on its rows.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Filters {
    #[serde(default)]
    site: String,
    /// `failing`, `passing` or `new`; all when empty.
    #[serde(default)]
    state: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: String,
}

impl Filters {
    fn pairs<'a>(&'a self, page: &'a str) -> [(&'static str, &'a str); 4] {
        [("site", &self.site), ("state", &self.state), ("q", &self.q), ("page", page)]
    }
}

/// A site's kinds of post worth testing, and whether one is.
struct Covered {
    source: &'static str,
    name: &'static str,
    kinds: Vec<(&'static str, bool)>,
}

#[derive(Template)]
#[template(path = "dashboard/tests.html")]
pub struct TestsPage {
    shell: Shell,
    /// What was typed to add, when it was refused.
    adding: String,
    /// This page's links.
    tested: Vec<Tested>,
    /// Every link.
    total: usize,
    list: Filters,
    sites: Vec<Choice>,
    states: Vec<Choice>,
    pager: Pager,
    /// This page's address, which refreshes it while a run goes on.
    here: String,
    /// The run going on: links checked, and in all.
    progress: Option<(usize, usize)>,
    /// Sites with more links than a run checks, and how often each of their links is.
    rotating: Vec<String>,
    passing: usize,
    failing: usize,
    uptime: String,
    checks: usize,
    last: String,
    next: String,
    coverage: Vec<Covered>,
}

async fn tests_page(app: &App, shell: Shell, adding: String, list: Filters) -> Result<TestsPage> {
    let settings = app.settings.get();
    let mut history = canary::history(app).await?;
    let mut results: HashMap<String, TestResult> =
        canary::results(app).await?.into_iter().map(|r| (r.url.clone(), r)).collect();
    let latest = results.values().filter(|r| settings.test_links.contains(&r.url)).map(|r| r.checked_at).max();
    let (mut ok_checks, mut checks) = (0, 0);
    let mut tested: Vec<Tested> = settings
        .test_links
        .iter()
        .map(|url| {
            let target = Target::from_link(url);
            let past = history.remove(url).unwrap_or_default();
            let ok = past.iter().filter(|c| c.1).count();
            (ok_checks, checks) = (ok_checks + ok, checks + past.len());
            let mut cells: Vec<(&str, String)> = vec![("", String::new()); canary::HISTORY.saturating_sub(past.len())];
            cells.extend(past.iter().map(|&(at, ok)| {
                (if ok { "ok" } else { "bad" }, format!("{}: {}", when(at), if ok { "passed" } else { "failed" }))
            }));
            let result = results.remove(url);
            let (level, state) = match &result {
                Some(r) if r.ok => ("ok", "Pass"),
                Some(_) => ("bad", "Fail"),
                None => ("idle", "Not checked yet"),
            };
            Tested {
                encoded: url::form_urlencoded::byte_serialize(url.as_bytes()).collect(),
                source: target.as_ref().map(|t| t.source.as_str()),
                kind: target.map_or("link", |t| t.page_type.as_str()).replace('_', " "),
                level,
                state,
                checked: result.as_ref().map_or_else(String::new, |r| ago(r.checked_at)),
                detail: result.map(|r| readable(&r.detail).to_string()).unwrap_or_default(),
                uptime: if past.is_empty() { "–".into() } else { percent(ok as u64, past.len() as u64) },
                cells,
                url: url.clone(),
            }
        })
        .collect();
    let rank = |level: &str| match level {
        "bad" => 0,
        "idle" => 1,
        _ => 2,
    };
    tested.sort_by(|a, b| (rank(a.level), a.source, &a.kind).cmp(&(rank(b.level), b.source, &b.kind)));
    let (total, passing, failing) = (
        tested.len(),
        tested.iter().filter(|t| t.level == "ok").count(),
        tested.iter().filter(|t| t.level == "bad").count(),
    );

    let site = Source::from_name(&list.site);
    let needle = list.q.trim().to_lowercase();
    let state_of = |t: &Tested| match t.level {
        "bad" => "failing",
        "ok" => "passing",
        _ => "new",
    };
    let in_view = |t: &Tested| {
        site.is_none_or(|s| t.source == Some(s.as_str()))
            && (needle.is_empty() || t.url.to_lowercase().contains(&needle) || t.kind.contains(&needle))
    };
    let link_to = |site: &str, state: &str| address("/tests", &[("site", site), ("state", state), ("q", &list.q)]);
    let sites = site_choices("/tests", site, &[("state", &list.state), ("q", &list.q)]);
    // Counts follow the site and the search, as the list does.
    let states = [("", "All"), ("failing", "Failing"), ("passing", "Passing"), ("new", "Not checked yet")]
        .into_iter()
        .map(|(state, label)| Choice {
            label,
            icon: None,
            href: link_to(&list.site, state),
            current: list.state == state,
            count: Some(tested.iter().filter(|t| in_view(t) && (state.is_empty() || state_of(t) == state)).count()),
        })
        .collect();
    let shown: Vec<Tested> =
        tested.into_iter().filter(|t| in_view(t) && (list.state.is_empty() || state_of(t) == list.state)).collect();
    let pager = Pager::new(shown.len(), PAGE, page_number(&list.page), "/tests", &list.pairs("")[..3]);
    let rows = pager.rows();
    let tested: Vec<Tested> = shown.into_iter().skip(rows.start).take(rows.len()).collect();
    let here = pager.href(pager.page);
    let rotating = Source::ALL
        .iter()
        .filter_map(|s| {
            let links = settings.test_links.iter().filter(|l| Source::of_link(l) == Some(*s)).count();
            let per_run = canary::per_run(settings.source(*s));
            (links > per_run).then(|| {
                let hours = links.div_ceil(per_run) as f64 / 2.0;
                format!("{}: {links} links, {per_run} a run, so each about every {hours} hours", s.name())
            })
        })
        .collect();
    let coverage = WORTH_TESTING
        .iter()
        .map(|(source, kinds)| {
            let covered: Vec<PageType> = settings
                .test_links
                .iter()
                .filter_map(|l| Target::from_link(l))
                .filter(|t| t.source == *source)
                .map(|t| t.page_type)
                .collect();
            let mut shown: Vec<(&str, bool)> = kinds.iter().map(|k| (k.as_str(), covered.contains(k))).collect();
            for extra in covered.iter().filter(|k| !kinds.contains(k)) {
                if !shown.iter().any(|(k, _)| *k == extra.as_str()) {
                    shown.push((extra.as_str(), true));
                }
            }
            Covered { source: source.as_str(), name: source.name(), kinds: shown }
        })
        .collect();
    Ok(TestsPage {
        shell,
        adding,
        tested,
        total,
        list,
        sites,
        states,
        pager,
        here,
        progress: app.canary.progress(),
        rotating,
        passing,
        failing,
        uptime: if checks == 0 { "–".into() } else { percent(ok_checks as u64, checks as u64) },
        checks,
        last: latest.map_or_else(|| "never".into(), ago),
        next: app.canary.next_run().map_or_else(|| "soon".into(), until),
        coverage,
    })
}

/// A check's finding without the step's code in front ("not_found: "), which its words explain.
fn readable(detail: &str) -> &str {
    match detail.split_once(": ") {
        Some((code, rest)) if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') => rest,
        _ => detail,
    }
}

/// The links in what was typed or pasted: one or several, however they're separated. A pasted
/// list loses its line breaks in a one-line field, so a new link also starts at each `http`.
fn links_in(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    for word in text.split_whitespace() {
        let mut starts: Vec<usize> =
            word.match_indices("http://").chain(word.match_indices("https://")).map(|(i, _)| i).collect();
        starts.sort_unstable();
        if starts.first() != Some(&0) {
            starts.insert(0, 0);
        }
        starts.push(word.len());
        links.extend(
            starts
                .windows(2)
                .map(|w| word[w[0]..w[1]].trim_end_matches([',', ';']).to_string())
                .filter(|l| !l.is_empty()),
        );
    }
    links
}

#[derive(Deserialize)]
pub struct TestsQuery {
    #[serde(flatten)]
    list: Filters,
    done: Option<String>,
}

pub async fn tests(State(d): State<Shared>, Query(q): Query<TestsQuery>) -> Response {
    show(tests_page(&d.app, Shell::new("tests", &Done { done: q.done }), String::new(), q.list).await)
}

#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    url: String,
    /// The list it was sent from, to go back to.
    #[serde(flatten)]
    list: Filters,
}

/// Back to the list as it was, with a notice; `done` first, where the activity log reads it.
fn back(list: &Filters, code: &str) -> Response {
    let mut pairs = vec![("done", code)];
    pairs.extend(list.pairs(&list.page));
    Redirect::to(&address("/tests", &pairs)).into_response()
}

/// Adds one link or several. One is checked at once, so whether it works shows straight away;
/// several are checked in the background, at their sites' pace, and show as each finishes.
pub async fn add_tests(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    let typed = links_in(&f.url);
    let mut new: Vec<String> = Vec::new();
    for link in typed.iter().filter(|l| !s.test_links.contains(l)) {
        if !new.contains(link) {
            new.push(link.clone());
        }
    }
    s.test_links.extend(new.iter().cloned());
    let saved = match (typed.is_empty(), new.is_empty()) {
        (true, _) => Err("Paste a link to add.".to_string()),
        (false, true) => Err("That link is tested already.".to_string()),
        _ => d.app.settings.set(s).await.map_err(|e| format!("{e:#}")),
    };
    if let Err(error) = saved {
        return refused_page(tests_page(&d.app, Shell::refused("tests", error), f.url, f.list).await);
    }
    if new.len() == 1 {
        if let Err(e) = d.app.canary.check(&d.app, Some(&new)).await {
            return server_error(&e);
        }
        return back(&f.list, "test_added");
    }
    canary::start(&d.app, Some(new));
    back(&f.list, "tests_added")
}

pub async fn remove_test(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.test_links.retain(|l| *l != f.url);
    if let Err(e) = d.app.settings.set(s).await {
        return server_error(&e);
    }
    match d.app.canary.forget(&d.app, &f.url).await {
        Ok(()) => back(&f.list, "test_removed"),
        Err(e) => server_error(&e),
    }
}

pub async fn check_test(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    match d.app.canary.check(&d.app, Some(std::slice::from_ref(&f.url))).await {
        Ok(_) => back(&f.list, "checked"),
        Err(e) => server_error(&e),
    }
}

/// Checks a run's worth of the links in the background; the page follows along.
pub async fn run_tests(State(d): State<Shared>, Form(list): Form<Filters>) -> Response {
    back(&list, if canary::start(&d.app, None) { "ran" } else { "running" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_come_out_of_whatever_was_pasted() {
        let (a, b) = ("https://www.facebook.com/reel/1", "https://www.instagram.com/p/Abc/");
        assert_eq!(links_in(&format!("  {a}\n{b}\n")), [a, b]);
        assert_eq!(links_in(&format!("{a}{b}")), [a, b], "line breaks dropped by a one-line field");
        assert_eq!(links_in(&format!("{a}, {b};")), [a, b]);
        assert!(links_in("  ").is_empty());
    }

    #[test]
    fn findings_read_without_codes() {
        assert_eq!(readable("not_found: Instagram doesn't show it"), "Instagram doesn't show it");
        assert_eq!(readable("1 media, 432 ms"), "1 media, 432 ms");
        assert_eq!(readable("Facebook: odd"), "Facebook: odd");
    }
}
