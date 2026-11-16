//! The dashboard's telegram ip ranges page.

use super::*;

#[derive(Template)]
#[template(path = "dashboard/telegram.html")]
pub struct TelegramPage {
    shell: Shell,
    proxy_warning: bool,
    client_ip_from: &'static str,
    trusted: String,
    nets: Vec<IpNet>,
    refreshed: String,
    suspects: Vec<(String, std::net::IpAddr, String, bool)>,
}

fn telegram_page(app: &App, shell: Shell) -> TelegramPage {
    let ranges = app.telegram_ips.current();
    TelegramPage {
        shell,
        proxy_warning: app.stats.proxy_warning(),
        client_ip_from: app.resolver.source().as_str(),
        trusted: app.config.trusted_proxies.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        nets: ranges.nets.clone(),
        refreshed: match ranges.refreshed_at {
            Some(at) => format!("downloaded {}", ago(at)),
            None => "the copy bundled with this build; the first download is due".into(),
        },
        suspects: app.stats.suspects().into_iter().map(|s| (when(s.at), s.ip, s.user_agent, s.from_proxy)).collect(),
    }
}

pub async fn telegram(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&telegram_page(&d.app, Shell::new("telegram", &q)))
}

pub async fn refresh_telegram(State(d): State<Shared>) -> Response {
    let refreshed = match crate::net::client() {
        Ok(client) => d.app.telegram_ips.refresh(&client, &d.app.db).await,
        Err(e) => Err(e),
    };
    match refreshed {
        Ok(_) => done("/telegram", "refreshed"),
        Err(e) => refused(&telegram_page(&d.app, Shell::refused("telegram", format!("Couldn't refresh: {e:#}")))),
    }
}
