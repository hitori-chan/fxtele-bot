//! The dashboard's sites: fetching and cookies page.

use super::*;

#[derive(Template)]
#[template(path = "dashboard/site.html")]
pub struct SitePage {
    shell: Shell,
    source: Source,
    s: SourceSettings,
    /// The site's own name, the default for the card's.
    default_name: &'static str,
    policy: &'static str,
    cookies: Status,
    cookie_summary: String,
    /// What a pasted export looks like, with this site's session cookies.
    cookie_example: String,
    /// The saved cookies' names and expiry dates; never their values.
    saved: Vec<(cookies::Listed, String)>,
    status: Status,
    numbers: Vec<(&'static str, &'static str, String, String)>,
    /// The fetch budget: per minute, at once, busy now, and how often it ran out.
    budget: (u32, u32, u32, u64),
    /// The host its links are rewritten to, when it has its own (`INSTAGRAM_HOST`).
    host: Option<Option<String>>,
}

async fn site_page(app: &App, shell: Shell, source: Source, s: SourceSettings) -> Result<SitePage> {
    let store = app.cookies.of(source);
    let status = store.status();
    let session = cookies::session_cookies(source);
    let cookie_summary = if status.count == 0 {
        format!("No cookies saved, so only what {} shows logged out works.", source.name())
    } else {
        let saved = status.saved_at.map(|at| format!("Saved {}.", when(at))).unwrap_or_default();
        let state = if let Some(at) = status.rejected_at {
            format!(" {} rejected them {}; add fresh ones.", source.name(), ago(at))
        } else if let Some(expires) = status.expires_at.filter(|_| status.expired()) {
            format!(" They expired {}; add fresh ones.", day(expires))
        } else if !status.has_session {
            format!(" They aren't used without {}.", session.join(" and "))
        } else if let Some(expires) = status.expires_at {
            format!(" The session expires {}.", day(expires))
        } else {
            String::new()
        };
        format!("{saved}{state}").trim_start().to_string()
    };
    let budget = app.budgets.of(source);
    let saved_settings = app.settings.get();
    let limits = saved_settings.source(source);
    Ok(SitePage {
        status: health::site_status(app, source),
        numbers: site_numbers(app, source).await?,
        budget: (limits.fetches_per_minute, limits.concurrency, budget.in_use(), budget.exhausted()),
        host: source.host_variable().map(|_| app.config.host(source).map(str::to_string)),
        default_name: source.name(),
        policy: match s.cookie_policy {
            CookiePolicy::Fallback => "fallback",
            CookiePolicy::Always => "always",
            CookiePolicy::Never => "never",
        },
        saved: store
            .list()
            .into_iter()
            .map(|c| {
                let expires = c.expires_at.map_or_else(|| "when the browser closes".into(), day);
                (c, expires)
            })
            .collect(),
        cookies: cookie_status(source, &status),
        cookie_summary,
        cookie_example: format!(
            r#"[{{"domain": ".{}.com", "name": "{}", "value": "…"}}, …]    or    {}"#,
            source.as_str(),
            session[0],
            session.iter().map(|name| format!("{name}=…")).collect::<Vec<_>>().join("; ")
        ),
        shell,
        source,
        s,
    })
}

pub async fn site_settings(State(d): State<Shared>, Path(name): Path<String>, Query(q): Query<Done>) -> Response {
    let Some(source) = Source::from_name(&name) else { return StatusCode::NOT_FOUND.into_response() };
    let s = d.app.settings.get().source(source).clone();
    show(site_page(&d.app, Shell::new(source.as_str(), &q), source, s).await)
}

#[derive(Deserialize)]
pub struct SiteForm {
    proxy: String,
    cookie_policy: CookiePolicy,
    site_name: String,
    author_profiles: Option<String>,
}

pub async fn save_site(State(d): State<Shared>, Path(name): Path<String>, Form(f): Form<SiteForm>) -> Response {
    let Some(source) = Source::from_name(&name) else { return StatusCode::NOT_FOUND.into_response() };
    let mut s = (*d.app.settings.get()).clone();
    let site = s.source_mut(source);
    let proxy = f.proxy.trim();
    site.proxy = (!proxy.is_empty()).then(|| proxy.to_string());
    site.cookie_policy = f.cookie_policy;
    site.site_name = f.site_name.trim().to_string();
    site.author_profiles = f.author_profiles.is_some();
    let entered = site.clone();
    match d.app.settings.set(s).await {
        Ok(()) => done(&format!("/sites/{}", source.as_str()), "saved"),
        Err(e) => {
            refused_page(site_page(&d.app, Shell::refused(source.as_str(), format!("{e:#}")), source, entered).await)
        }
    }
}

#[derive(Deserialize)]
pub struct CookiesForm {
    cookies: String,
}

pub async fn save_cookies(State(d): State<Shared>, Path(name): Path<String>, Form(f): Form<CookiesForm>) -> Response {
    let Some(source) = Source::from_name(&name) else { return StatusCode::NOT_FOUND.into_response() };
    let saved = match parse_export(source, &f.cookies) {
        Ok(cookies) => d.app.cookies.of(source).save(cookies).await,
        Err(e) => Err(e),
    };
    match saved {
        Ok(()) => {
            // A post behind a login wall shouldn't wait out its cached failure.
            d.app.cache.forget_failures().await;
            done(&format!("/sites/{}", source.as_str()), "cookies")
        }
        Err(e) => {
            let s = d.app.settings.get().source(source).clone();
            refused_page(site_page(&d.app, Shell::refused(source.as_str(), format!("{e:#}")), source, s).await)
        }
    }
}

pub async fn clear_cookies(State(d): State<Shared>, Path(name): Path<String>) -> Response {
    let Some(source) = Source::from_name(&name) else { return StatusCode::NOT_FOUND.into_response() };
    match d.app.cookies.of(source).clear().await {
        Ok(()) => done(&format!("/sites/{}", source.as_str()), "cookies_removed"),
        Err(e) => server_error(&e),
    }
}
