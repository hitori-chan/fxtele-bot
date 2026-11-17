//! Activity: changes made on the dashboard, sign-ins and alerts, newest first.

use super::pager::{Pager, page_number};
use super::*;
use crate::activity::{self, Kind};

/// Entries a page.
const PAGE: usize = 50;

pub struct Shown {
    when: String,
    ago: String,
    icon: &'static str,
    level: &'static str,
    actor: String,
    text: String,
}

#[derive(Template)]
#[template(path = "dashboard/activity.html")]
pub struct ActivityPage {
    shell: Shell,
    kind: String,
    entries: Vec<Shown>,
    pager: Pager,
}

#[derive(Debug, Default, Deserialize)]
pub struct ActivityFilter {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    page: String,
}

/// How an entry looks: a refused sign-in or an alert about trouble stands out.
fn looks(entry: &activity::Entry) -> (&'static str, &'static str) {
    match entry.kind.as_str() {
        "sign_in" if entry.text.starts_with("Refused") => ("sign-in", BAD),
        "sign_in" => ("sign-in", OK),
        // Recoveries: "… works again", "… work again". Not "tried again 10 minutes later".
        "alert" if entry.text.contains(" work again") || entry.text.contains(" works again") => ("bell", OK),
        "alert" if entry.text.starts_with("Test alert") => ("bell", OK),
        "alert" => ("bell", WARN),
        _ => ("edit", "info"),
    }
}

pub async fn activity_page(State(d): State<Shared>, Query(f): Query<ActivityFilter>) -> Response {
    let page = async {
        let kind = Kind::parse(&f.kind);
        let asked = page_number(&f.page).unwrap_or(1);
        let (mut entries, mut total) = activity::page(&d.app.db, kind, (asked - 1) * PAGE, PAGE as u32).await?;
        let pager = Pager::new(total, PAGE, Some(asked), "/activity", &[("kind", kind.map_or("", Kind::as_str))]);
        // A page past the end shows the last one.
        if pager.page != asked {
            (entries, total) = activity::page(&d.app.db, kind, (pager.page - 1) * PAGE, PAGE as u32).await?;
        }
        let pager = Pager::new(total, PAGE, Some(pager.page), "/activity", &[("kind", kind.map_or("", Kind::as_str))]);
        let entries = entries
            .into_iter()
            .map(|e| {
                let (icon, level) = looks(&e);
                Shown { when: when(e.at), ago: ago(e.at), icon, level, actor: e.actor, text: e.text }
            })
            .collect();
        anyhow::Ok(ActivityPage {
            shell: Shell::new("activity", &Done::default()),
            kind: f.kind.clone(),
            entries,
            pager,
        })
    };
    show(page.await)
}

/// The newest alerts, for the Alerts page.
pub async fn recent_alerts(app: &App) -> Result<Vec<(String, String, String)>> {
    Ok(activity::list(&app.db, Some(Kind::Alert), 8).await?.into_iter().map(|e| (ago(e.at), e.text, e.actor)).collect())
}
