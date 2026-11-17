//! Issues: failed extractions grouped by site, step and message, like an error tracker, each with
//! what usually causes it and what to do. Fixed ones can be marked so; they come back if the
//! failure does. The failures themselves are the newest 500 the server kept.

use super::pager::{Pager, address, page_number};
use super::*;

/// Issues a page: each is a card with its links.
const PAGE: usize = 20;

/// Failures with the same site, step and message.
pub struct Issue {
    /// Its row's id on the Issues page, from its newest failure: `issue-12`.
    pub anchor: String,
    pub source: Option<&'static str>,
    pub stage: String,
    /// The newest failure's message.
    pub error: String,
    pub advice: &'static str,
    /// Every failure in it, for marking it fixed.
    pub ids: String,
    pub page_types: Vec<String>,
    pub count: usize,
    pub first: String,
    pub last: String,
    /// The latest few: when, the link, and the link for a query string.
    pub links: Vec<(String, String, String)>,
    /// Occurrences from the oldest failure shown to now, for a sparkline.
    pub spark: Spark,
}

/// A message without its links, which differ between failures of the same kind.
fn shape(error: &str) -> String {
    error
        .split(' ')
        .map(|word| if word.starts_with("http://") || word.starts_with("https://") { "<link>" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What failures group by: the message without its links or numbers ("3 post-data blocks").
fn group_key(error: &str) -> String {
    let mut key = String::new();
    for c in shape(error).chars() {
        if !c.is_ascii_digit() {
            key.push(c);
        } else if !key.ends_with('#') {
            key.push('#');
        }
    }
    key
}

/// What usually causes failures at a step, and what to do about them.
fn advice(stage: &str) -> &'static str {
    match stage {
        "login_wall" => {
            "The site asked for a login. If its cookies are saved, check on its page that they still work; \
             otherwise the post is shown to logged-in people only."
        }
        "not_found" => {
            "The post wasn't there: private, deleted, expired (stories last a day), or a wrong link. \
             Nothing to do unless links that work in a browser fail too: try one in the playground."
        }
        "rate_limited" => "The site asked to slow down. Lower its fetches per minute, or give it a proxy.",
        "timeout" => {
            "The site answered too slowly. Raise the extraction timeout under Rate limits, or check the proxy."
        }
        "fetch" => {
            "The site couldn't be reached, or answered with an error. Usually brief; if not, check the connection."
        }
        "redirect" => "The link led somewhere that isn't a post, usually because it's wrong or old.",
        _ => "",
    }
}

/// What failures are grouped by: site, step and message.
type IssueKey = (Option<Source>, String, String);

/// Groups failures since `since`, of one site or all, most frequent first.
pub fn issues(failures: &[Failure], since: i64, site: Option<Source>) -> Vec<Issue> {
    let mut groups: Vec<(IssueKey, Vec<&Failure>)> = Vec::new();
    for f in failures.iter().filter(|f| f.at >= since) {
        let source = Source::of_link(&f.url);
        if site.is_some_and(|s| Some(s) != source) {
            continue;
        }
        let key = (source, f.stage.clone(), group_key(&f.error));
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(f),
            None => groups.push((key, vec![f])),
        }
    }
    // Every issue's sparkline shares one axis, from the oldest failure shown to now, in 24 steps.
    let start = groups.iter().flat_map(|(_, g)| g.iter().map(|f| f.at)).min().unwrap_or(since).max(since);
    let span = (now() - start).max(1);
    let mut issues: Vec<Issue> = groups
        .into_iter()
        .map(|((source, stage, _), mut group)| {
            group.sort_by_key(|f| std::cmp::Reverse(f.at));
            let (last, first) = (group[0].at, group[group.len() - 1].at);
            let mut page_types: Vec<String> = group.iter().map(|f| f.page_type.replace('_', " ")).collect();
            page_types.sort();
            page_types.dedup();
            let mut counts = [0u64; 24];
            for f in &group {
                let i = usize::try_from((f.at - start) * 24 / span).unwrap_or(0).min(23);
                counts[i] += 1;
            }
            Issue {
                anchor: format!("issue-{}", group[0].id),
                source: source.map(Source::as_str),
                advice: advice(&stage),
                stage: Stage::parse(&stage).map_or_else(|| stage.replace('_', " "), |s| s.label().to_string()),
                error: shape(&group[0].error),
                ids: group.iter().map(|f| f.id.to_string()).collect::<Vec<_>>().join(","),
                page_types,
                count: group.len(),
                first: ago(first),
                last: ago(last),
                links: group
                    .iter()
                    .take(5)
                    .map(|f| {
                        (when(f.at), f.url.clone(), url::form_urlencoded::byte_serialize(f.url.as_bytes()).collect())
                    })
                    .collect(),
                spark: chart::spark(&counts),
            }
        })
        .collect();
    issues.sort_by_key(|i| std::cmp::Reverse(i.count));
    issues
}

#[derive(Template)]
#[template(path = "dashboard/issues.html")]
pub struct IssuesPage {
    shell: Shell,
    site: Option<Source>,
    sites: Vec<Choice>,
    stages: Vec<(&'static str, String)>,
    stage: String,
    failures: usize,
    latest: String,
    /// Every kind of failure.
    kinds: usize,
    /// This page's issues.
    issues: Vec<Issue>,
    pager: Pager,
}

#[derive(Debug, Default, Deserialize)]
pub struct IssueFilter {
    #[serde(default)]
    site: String,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    page: String,
    done: Option<String>,
}

pub async fn issues_page(State(d): State<Shared>, Query(q): Query<IssueFilter>) -> Response {
    let page = async {
        let failures: Vec<Failure> =
            d.app.stats.failures(500).await?.into_iter().filter(|f| q.stage.is_empty() || f.stage == q.stage).collect();
        let site = Source::from_name(&q.site);
        let issues = issues(&failures, 0, site);
        let pager = Pager::new(
            issues.len(),
            PAGE,
            page_number(&q.page),
            "/issues",
            &[("site", site.map_or("", Source::as_str)), ("stage", &q.stage)],
        );
        anyhow::Ok(IssuesPage {
            shell: Shell::new("issues", &Done { done: q.done.clone() }),
            site,
            sites: site_choices("/issues", site, &[("stage", &q.stage)]),
            stages: Stage::ALL.iter().map(|s| (s.as_str(), s.label().to_string())).collect(),
            stage: q.stage.clone(),
            failures: issues.iter().map(|i| i.count).sum(),
            latest: failures
                .iter()
                .filter(|f| site.is_none_or(|s| Source::of_link(&f.url) == Some(s)))
                .map(|f| f.at)
                .max()
                .map_or_else(|| "–".into(), ago),
            kinds: issues.len(),
            issues: issues.into_iter().skip(pager.rows().start).take(pager.rows().len()).collect(),
            pager,
        })
    };
    show(page.await)
}

#[derive(Deserialize)]
pub struct Fixed {
    #[serde(default)]
    ids: String,
    /// The list it was sent from, to go back to.
    #[serde(flatten)]
    list: IssueFilter,
}

/// Marks an issue fixed: its failures go, and it's back if one happens again.
pub async fn resolve_issue(State(d): State<Shared>, Form(f): Form<Fixed>) -> Response {
    let ids: Vec<i64> = f.ids.split(',').filter_map(|id| id.trim().parse().ok()).take(500).collect();
    if ids.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match d.app.stats.forget_failures(ids).await {
        Ok(()) => {
            let list = &f.list;
            let back = [("done", "issue_fixed"), ("site", &list.site), ("stage", &list.stage), ("page", &list.page)];
            Redirect::to(&address("/issues", &back)).into_response()
        }
        Err(e) => server_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(at: i64, url: &str, stage: &str, error: &str) -> Failure {
        Failure { id: at, at, url: url.into(), page_type: "reel".into(), stage: stage.into(), error: error.into() }
    }

    #[test]
    fn numbers_in_messages_dont_split_issues() {
        let failures = [
            failure(20, "https://www.facebook.com/stories/1/A", "not_found", "none of the page's 3 blocks hold this"),
            failure(10, "https://www.facebook.com/stories/1/B", "not_found", "none of the page's 12 blocks hold this"),
        ];
        let all = issues(&failures, 0, None);
        assert_eq!((all.len(), all[0].count, all[0].ids.as_str()), (1, 2, "20,10"));
        assert_eq!(all[0].error, "none of the page's 3 blocks hold this", "the newest message, as it was");
        assert!(all[0].advice.contains("stories last a day"));
    }

    #[test]
    fn failures_group_by_site_step_and_message() {
        let failures = [
            failure(50, "https://www.facebook.com/reel/3", "login_wall", "Facebook asked to log in"),
            failure(40, "https://www.instagram.com/p/A", "not_found", "not shown: https://www.instagram.com/p/A"),
            failure(30, "https://www.facebook.com/reel/2", "login_wall", "Facebook asked to log in"),
            failure(20, "https://www.instagram.com/p/B", "not_found", "not shown: https://www.instagram.com/p/B"),
            failure(10, "https://www.facebook.com/reel/1", "login_wall", "Facebook asked to log in"),
            failure(1, "https://www.facebook.com/reel/0", "timeout", "too slow"),
        ];
        let all = issues(&failures, 5, None);
        assert_eq!(all.iter().map(|i| i.count).collect::<Vec<_>>(), [3, 2], "the oldest is before the window");
        assert_eq!((all[0].source, all[0].stage.as_str()), (Some("facebook"), "Login wall"));
        assert_eq!(all[0].links[0].1, "https://www.facebook.com/reel/3", "newest first");
        assert_eq!(all[1].error, "not shown: <link>");
        let instagram = issues(&failures, 0, Some(Source::Instagram));
        assert_eq!((instagram.len(), instagram[0].count), (1, 2));
    }
}
