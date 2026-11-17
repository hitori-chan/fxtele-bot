//! Alerts: where they go, what they're about and how each of those is doing, and the latest sent.

use super::*;

#[derive(Template)]
#[template(path = "dashboard/alerts.html")]
pub struct AlertsPage {
    shell: Shell,
    has_token: bool,
    alert_chat: String,
    status: Status,
    checks: Vec<Check>,
    /// When, what, and whether it reached the chat.
    sent: Vec<(String, String, String)>,
}

async fn alerts_view(app: &App, shell: Shell, alert_chat: String) -> Result<AlertsPage> {
    let settings = app.settings.get();
    Ok(AlertsPage {
        shell,
        has_token: app.config.telegram_bot_token.is_some(),
        alert_chat,
        status: health::alerts_status(app, &settings),
        checks: health::checks(app).await?.into_iter().filter(|c| c.name != "Alerts").collect(),
        sent: recent_alerts(app).await?,
    })
}

pub async fn alerts(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    let chat = d.app.settings.get().alert_chat.map(|c| c.to_string()).unwrap_or_default();
    show(alerts_view(&d.app, Shell::new("alerts", &q), chat).await)
}

#[derive(Deserialize)]
pub struct AlertsForm {
    alert_chat: String,
}

pub async fn save_alerts(State(d): State<Shared>, Form(f): Form<AlertsForm>) -> Response {
    let chat = f.alert_chat.trim();
    let saved = match chat.parse::<i64>() {
        _ if chat.is_empty() => d.app.settings.edit(|s| s.alert_chat = None).await,
        Ok(chat) => d.app.settings.edit(|s| s.alert_chat = Some(chat)).await,
        Err(_) => Err(anyhow::anyhow!("The chat ID is a number, like 123456789 or -1001234567890.")),
    };
    match saved {
        Ok(()) => done("/alerts", "saved"),
        Err(e) => refused_page(alerts_view(&d.app, Shell::refused("alerts", format!("{e:#}")), f.alert_chat).await),
    }
}

/// Sends a message to the alert chat, so the chat ID and token can be checked before something
/// goes wrong.
/// To the chat as typed, saved or not, so a new chat ID can be tried before saving it.
pub async fn test_alert(State(d): State<Shared>, Form(f): Form<AlertsForm>) -> Response {
    let typed = f.alert_chat.trim();
    let sent = match (&d.app.config.telegram_bot_token, typed.parse::<i64>()) {
        (None, _) => Err(anyhow::anyhow!("Set TELEGRAM_BOT_TOKEN in .env and restart the server to send alerts.")),
        (_, _) if typed.is_empty() => Err(anyhow::anyhow!("Type the chat ID to send it to.")),
        (_, Err(_)) => Err(anyhow::anyhow!("The chat ID is a number, like 123456789 or -1001234567890.")),
        (_, Ok(chat)) => {
            crate::alerts::send_to(&d.app, Some(chat), "Test alert from the Homura dashboard: alerts reach this chat.")
                .await
        }
    };
    match sent {
        Ok(()) if d.app.settings.get().alert_chat.map(|c| c.to_string()).as_deref() == Some(typed) => {
            done("/alerts", "alert_sent")
        }
        Ok(()) => {
            let shell = Shell::new("alerts", &Done { done: Some("alert_tried".into()) });
            show(alerts_view(&d.app, shell, f.alert_chat).await)
        }
        Err(e) => refused_page(
            alerts_view(&d.app, Shell::refused("alerts", format!("No alert sent: {e:#}")), f.alert_chat).await,
        ),
    }
}
