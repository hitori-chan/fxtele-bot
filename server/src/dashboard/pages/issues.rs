//! Issues: failed extractions grouped by site, step and message, like an error tracker. The
//! failures themselves are the newest 500 the server kept.

use super::*;

/// Failures with the same site, step and message.
pub struct Issue {
    pub source: Option<&'static str>,
    pub stage: String,
    pub error: String,
    pub page_types: Vec<String>,
    pub count: usize,
    pub first: String,
    pub last: String,
    /// The latest few: when, the link, and the link for a query string.
    pub links: Vec<(String, String, String)>,
    /// Occurrences from the oldest failure shown to now, for a sparkline.
    pub spark: Spark,
}

/// Links in an error differ between failures of the same kind, so they're left out of grouping.
fn shape(error: &str) -> String {
    error
        .split(' ')
        .map(|word| if word.starts_with("http://") || word.starts_with("https://") { "<link>" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
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
        let key = (source, f.stage.clone(), shape(&f.error));
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
        .map(|((source, stage, error), mut group)| {
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
                source: source.map(Source::as_str),
                stage: stage.replace('_', " "),
                error,
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
    sites: [Source; 2],
    stages: Vec<(&'static str, String)>,
    stage: String,
    failures: usize,
    latest: String,
    issues: Vec<Issue>,
}

impl IssuesPage {
    fn is_site(&self, s: &Source) -> bool {
        self.site == Some(*s)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct IssueFilter {
    #[serde(default)]
    site: String,
    #[serde(default)]
    stage: String,
}

pub async fn issues_page(State(d): State<Shared>, Query(q): Query<IssueFilter>) -> Response {
    let page = async {
        let failures: Vec<Failure> =
            d.app.stats.failures(500).await?.into_iter().filter(|f| q.stage.is_empty() || f.stage == q.stage).collect();
        let site = Source::from_name(&q.site);
        let issues = issues(&failures, 0, site);
        anyhow::Ok(IssuesPage {
            shell: Shell::new("issues", &Done::default()),
            site,
            sites: Source::ALL,
            stages: Stage::ALL.iter().map(|s| (s.as_str(), s.as_str().replace('_', " "))).collect(),
            stage: q.stage.clone(),
            failures: issues.iter().map(|i| i.count).sum(),
            latest: failures
                .iter()
                .filter(|f| site.is_none_or(|s| Source::of_link(&f.url) == Some(s)))
                .map(|f| f.at)
                .max()
                .map_or_else(|| "–".into(), ago),
            issues,
        })
    };
    show(page.await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(at: i64, url: &str, stage: &str, error: &str) -> Failure {
        Failure { at, url: url.into(), page_type: "reel".into(), stage: stage.into(), error: error.into() }
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
        assert_eq!((all[0].source, all[0].stage.as_str()), (Some("facebook"), "login wall"));
        assert_eq!(all[0].links[0].1, "https://www.facebook.com/reel/3", "newest first");
        assert_eq!(all[1].error, "not shown: <link>");
        let instagram = issues(&failures, 0, Some(Source::Instagram));
        assert_eq!((instagram.len(), instagram[0].count), (1, 2));
    }
}
