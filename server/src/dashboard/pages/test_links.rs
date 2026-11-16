//! Test links: one list to add, check, try and remove them, each with its latest check and a day
//! of history; when the next check runs; and which kinds of post each site has covered.

use std::collections::HashMap;

use super::*;
use crate::extract::{PageType, Target};

/// Kinds of post worth a test link, per site. Stories expire within a day, and Facebook's watch
/// links and other pages are read like videos and posts.
const WORTH_TESTING: [(Source, &[PageType]); 2] = [
    (
        Source::Facebook,
        &[PageType::Post, PageType::Reel, PageType::Video, PageType::Photo, PageType::Album, PageType::Profile],
    ),
    (Source::Instagram, &[PageType::Post, PageType::Reel, PageType::Profile]),
];

/// A tested link: its site and kind, latest check, and history as `ok`/`bad` cells, oldest first.
pub struct Tested {
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

/// A site's kinds of post worth testing, and whether one is.
pub struct Covered {
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
    tested: Vec<Tested>,
    passing: usize,
    failing: usize,
    uptime: String,
    checks: usize,
    last: String,
    next: String,
    coverage: Vec<Covered>,
}

async fn tests_page(app: &App, shell: Shell, adding: String) -> Result<TestsPage> {
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
        passing: tested.iter().filter(|t| t.level == "ok").count(),
        failing: tested.iter().filter(|t| t.level == "bad").count(),
        tested,
        uptime: if checks == 0 { "–".into() } else { percent(ok_checks as u64, checks as u64) },
        checks,
        last: latest.map_or_else(|| "never".into(), ago),
        next: app.canary.next_run().map_or_else(|| "Soon".into(), until),
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

/// "in 12 min", for something planned.
fn until(at: i64) -> String {
    match at - jiff::Timestamp::now().as_second() {
        ..60 => "In under a minute".into(),
        s @ 60..3600 => format!("In {} min", s / 60),
        s => format!("In {} h", s / 3600),
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

pub async fn tests(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    show(tests_page(&d.app, Shell::new("tests", &q), String::new()).await)
}

#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    url: String,
}

/// Adds one link or several, then checks the new ones, so whether they work shows at once.
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
        return refused_page(tests_page(&d.app, Shell::refused("tests", error), f.url).await);
    }
    for link in &new {
        if let Err(e) = d.app.canary.check(&d.app, Some(link)).await {
            return server_error(&e);
        }
    }
    done("/tests", if new.len() == 1 { "test_added" } else { "tests_added" })
}

pub async fn remove_test(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.test_links.retain(|l| *l != f.url);
    if let Err(e) = d.app.settings.set(s).await {
        return server_error(&e);
    }
    match d.app.canary.forget(&d.app, &f.url).await {
        Ok(()) => done("/tests", "test_removed"),
        Err(e) => server_error(&e),
    }
}

pub async fn check_test(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    match d.app.canary.check(&d.app, Some(&f.url)).await {
        Ok(_) => done("/tests", "checked"),
        Err(e) => server_error(&e),
    }
}

pub async fn run_tests(State(d): State<Shared>) -> Response {
    match d.app.canary.run(&d.app).await {
        Ok(_) => done("/tests", "ran"),
        Err(e) => server_error(&e),
    }
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
