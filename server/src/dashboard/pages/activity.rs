//! Activity: changes made on the dashboard, sign-ins and alerts, newest first.

use super::*;
use crate::activity::{self, Kind};

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
}

#[derive(Debug, Default, Deserialize)]
pub struct ActivityFilter {
    #[serde(default)]
    kind: String,
}

/// How an entry looks: a refused sign-in or an alert about trouble stands out.
fn looks(entry: &activity::Entry) -> (&'static str, &'static str) {
    match entry.kind.as_str() {
        "sign_in" if entry.text.starts_with("Refused") => ("sign-in", BAD),
        "sign_in" => ("sign-in", OK),
        "alert" if entry.text.contains("again") || entry.text.starts_with("Test alert") => ("bell", OK),
        "alert" => ("bell", WARN),
        _ => ("edit", "info"),
    }
}

pub async fn activity_page(State(d): State<Shared>, Query(f): Query<ActivityFilter>) -> Response {
    let page = async {
        let entries = activity::list(&d.app.db, Kind::parse(&f.kind), 300)
            .await?
            .into_iter()
            .map(|e| {
                let (icon, level) = looks(&e);
                Shown { when: when(e.at), ago: ago(e.at), icon, level, actor: e.actor, text: e.text }
            })
            .collect();
        anyhow::Ok(ActivityPage { shell: Shell::new("activity", &Done::default()), kind: f.kind.clone(), entries })
    };
    show(page.await)
}

/// The newest alerts, for the Alerts page.
pub async fn recent_alerts(app: &App) -> Result<Vec<(String, String)>> {
    Ok(activity::list(&app.db, Some(Kind::Alert), 8).await?.into_iter().map(|e| (ago(e.at), e.text)).collect())
}
