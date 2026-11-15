//! Facebook login cookies: pasted in the dashboard (Cookie-Editor's JSON export or a `Cookie`
//! header), stored in SQLite, never logged.

use std::sync::Arc;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;

const DB_KEY: &str = "facebook_cookies";
/// Both are needed for a logged-in session.
const SESSION_COOKIES: [&str; 2] = ["c_user", "xs"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; `None` for session cookies.
    pub expires: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Stored {
    cookies: Vec<Cookie>,
    saved_at: i64,
    /// Set when Facebook answered these cookies with a login page.
    rejected_at: Option<i64>,
}

/// What the dashboard shows about the saved cookies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub count: usize,
    pub has_session: bool,
    /// When the first of `c_user` and `xs` expires.
    pub expires_at: Option<i64>,
    pub saved_at: Option<i64>,
    pub rejected_at: Option<i64>,
}

/// Parses pasted cookies, in either of two forms:
/// - a JSON list of cookie objects, as Cookie-Editor and other exporters produce. Accepts
///   `expirationDate` for `expires` and `host` for `domain`; the path defaults to `/`.
/// - a `Cookie` header (`c_user=…; xs=…`), as copied from the browser's developer tools or
///   Cookie-Editor's header export, with or without the `Cookie:` name. It carries no expiry dates.
///
/// Expired and non-Facebook cookies are dropped. Refused without `c_user` and `xs`, since
/// those make the logged-in session.
pub fn parse_export(text: &str) -> Result<Vec<Cookie>> {
    let text = text.trim();
    let cookies = if text.starts_with(['[', '{']) { parse_json(text)? } else { parse_header(text) };
    if cookies.is_empty() {
        bail!("no Facebook cookies there; paste Cookie-Editor's JSON export or a Cookie header (c_user=…; xs=…)");
    }
    let missing: Vec<&str> =
        SESSION_COOKIES.into_iter().filter(|name| !cookies.iter().any(|c| c.name == *name)).collect();
    if !missing.is_empty() {
        bail!(
            "{} missing, so these aren't a signed-in session; copy them while logged in to Facebook",
            missing.join(" and ")
        );
    }
    Ok(cookies)
}

fn valid(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
}

fn parse_json(text: &str) -> Result<Vec<Cookie>> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        bail!("that JSON isn't a list of cookies; in Cookie-Editor, use Export → JSON");
    };
    let now = jiff::Timestamp::now().as_second() as f64;
    let mut cookies = Vec::new();
    for item in items {
        let Value::Object(item) = item else { continue };
        let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(name), Some(value)) = (text("name"), text("value")) else { continue };
        let expires = item
            .get("expires")
            .or_else(|| item.get("expirationDate"))
            .filter(|v| !v.is_boolean())
            .and_then(Value::as_f64);
        if expires.is_some_and(|t| t > 0.0 && t < now) {
            continue;
        }
        let domain = text("domain").or_else(|| text("host")).unwrap_or_else(|| ".facebook.com".into());
        if !domain.trim_start_matches('.').ends_with("facebook.com") {
            continue;
        }
        if name.is_empty() || !valid(&name) || !valid(&value) {
            continue;
        }
        cookies.push(Cookie { name, value, domain, path: text("path").unwrap_or_else(|| "/".into()), expires });
    }
    Ok(cookies)
}

fn parse_header(text: &str) -> Vec<Cookie> {
    // "Copy request headers" pastes several lines; take the Cookie one if it's there.
    let line = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("cookie").then_some(value)
        })
        .unwrap_or(text);
    line.split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            (!name.is_empty() && valid(name) && valid(value)).then(|| Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: ".facebook.com".into(),
                path: "/".into(),
                expires: None,
            })
        })
        .collect()
}

pub struct CookieStore {
    db: Db,
    state: ArcSwap<Stored>,
}

impl CookieStore {
    pub async fn load(db: Db) -> Self {
        let stored = match db.get_json::<Stored>(DB_KEY).await {
            Ok(stored) => stored.unwrap_or_default(),
            Err(e) => {
                tracing::error!("saved Facebook cookies are unreadable: {e:#}");
                Stored::default()
            }
        };
        Self { db, state: ArcSwap::from_pointee(stored) }
    }

    pub async fn save(&self, cookies: Vec<Cookie>) -> Result<()> {
        let stored = Stored { cookies, saved_at: jiff::Timestamp::now().as_second(), rejected_at: None };
        self.db.set_json(DB_KEY, &stored).await?;
        self.state.store(Arc::new(stored));
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        self.db.delete(DB_KEY).await?;
        self.state.store(Arc::new(Stored::default()));
        Ok(())
    }

    /// The `Cookie` header for a logged-in request, if the saved cookies look usable.
    pub fn header(&self) -> Option<String> {
        let state = self.state.load();
        let status = status_of(&state);
        if !status.has_session || state.rejected_at.is_some() {
            return None;
        }
        let now = jiff::Timestamp::now().as_second() as f64;
        let header = state
            .cookies
            .iter()
            .filter(|c| c.expires.is_none_or(|t| t <= 0.0 || t > now))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        Some(header)
    }

    /// The logged-in user's ID, to scrub from saved test pages.
    pub fn user_id(&self) -> Option<String> {
        self.state.load().cookies.iter().find(|c| c.name == "c_user").map(|c| c.value.clone())
    }

    /// Facebook answered the cookies with a login page. Returns `true` the first time.
    pub async fn mark_rejected(&self) -> bool {
        let mut stored = (**self.state.load()).clone();
        if stored.rejected_at.is_some() || stored.cookies.is_empty() {
            return false;
        }
        stored.rejected_at = Some(jiff::Timestamp::now().as_second());
        if let Err(e) = self.db.set_json(DB_KEY, &stored).await {
            tracing::error!("can't save the cookie status: {e:#}");
        }
        self.state.store(Arc::new(stored));
        true
    }

    pub fn status(&self) -> Status {
        status_of(&self.state.load())
    }
}

fn status_of(stored: &Stored) -> Status {
    let session: Vec<&Cookie> =
        SESSION_COOKIES.iter().filter_map(|name| stored.cookies.iter().find(|c| c.name == *name)).collect();
    let now = jiff::Timestamp::now().as_second();
    let expires_at = session.iter().filter_map(|c| c.expires).filter(|t| *t > 0.0).map(|t| t as i64).min();
    Status {
        count: stored.cookies.len(),
        has_session: session.len() == SESSION_COOKIES.len() && expires_at.is_none_or(|t| t > now),
        expires_at,
        saved_at: (!stored.cookies.is_empty()).then_some(stored.saved_at),
        rejected_at: stored.rejected_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = r#"[
        {"name": "c_user", "value": "100001", "domain": ".facebook.com", "expirationDate": 4102444800.5},
        {"name": "xs", "value": "abc%3A1", "host": ".facebook.com", "expires": 4102444800},
        {"name": "old", "value": "x", "domain": ".facebook.com", "expirationDate": 1000},
        {"name": "other", "value": "y", "domain": ".example.com"},
        {"name": "datr", "value": "z", "domain": ".facebook.com", "expires": false},
        {"name": "bad", "value": "a;b", "domain": ".facebook.com"},
        "not an object"
    ]"#;

    #[test]
    fn parses_browser_exports() {
        let cookies = parse_export(EXPORT).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c_user", "xs", "datr"]);
        assert_eq!(cookies[1].domain, ".facebook.com");
        assert_eq!(cookies[0].path, "/");
        assert!(parse_export("{}").is_err());
        assert!(parse_export("[]").is_err());
    }

    /// Cookie-Editor's Export → JSON, as it writes it (values shortened).
    const COOKIE_EDITOR: &str = r#"[
    {
        "domain": ".facebook.com",
        "expirationDate": 4102444800.738294,
        "hostOnly": false,
        "httpOnly": true,
        "name": "xs",
        "path": "/",
        "sameSite": "no_restriction",
        "secure": true,
        "session": false,
        "storeId": null,
        "value": "12%3AaBcD%3A2%3A1700000000%3A-1%3A-1"
    },
    {
        "domain": ".facebook.com",
        "expirationDate": 4102444800.738227,
        "hostOnly": false,
        "httpOnly": false,
        "name": "c_user",
        "path": "/",
        "sameSite": "no_restriction",
        "secure": true,
        "session": false,
        "storeId": null,
        "value": "100001"
    },
    {
        "domain": ".facebook.com",
        "hostOnly": false,
        "httpOnly": false,
        "name": "presence",
        "path": "/",
        "sameSite": null,
        "secure": true,
        "session": true,
        "storeId": null,
        "value": "C%7B%22t3%22%3A%5B%5D%7D"
    }
]"#;

    #[test]
    fn parses_cookie_editor_json() {
        let cookies = parse_export(COOKIE_EDITOR).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["xs", "c_user", "presence"]);
        assert_eq!(cookies[0].expires, Some(4_102_444_800.738_294));
        assert_eq!(cookies[2].expires, None, "a session cookie");
    }

    #[test]
    fn parses_cookie_headers() {
        let cookies = parse_export("datr=a1; sb=b2; c_user=100001; xs=12%3AaBcD%3A2; wd=1920x1080").unwrap();
        let pairs: Vec<(&str, &str)> = cookies.iter().map(|c| (c.name.as_str(), c.value.as_str())).collect();
        assert_eq!(
            pairs,
            [("datr", "a1"), ("sb", "b2"), ("c_user", "100001"), ("xs", "12%3AaBcD%3A2"), ("wd", "1920x1080")]
        );
        assert!(cookies.iter().all(|c| c.domain == ".facebook.com" && c.path == "/" && c.expires.is_none()));

        assert_eq!(parse_export("Cookie: c_user=1; xs=2\n").unwrap().len(), 2);
        let headers = "accept: text/html\nCOOKIE: c_user=1; xs=2; presence=x\nuser-agent: Mozilla/5.0";
        assert_eq!(parse_export(headers).unwrap().len(), 3, "the Cookie line from copied request headers");
    }

    #[test]
    fn refuses_pastes_without_a_session() {
        let missing = parse_export("datr=a1; c_user=100001").unwrap_err().to_string();
        assert!(missing.starts_with("xs missing"), "{missing}");
        assert!(parse_export("datr=a1; sb=b2").unwrap_err().to_string().starts_with("c_user and xs missing"));
        assert!(parse_export("hello there").unwrap_err().to_string().starts_with("no Facebook cookies"));
        assert!(parse_export("").is_err());
    }

    #[tokio::test]
    async fn store_status_header_and_rejection() {
        let db = Db::memory().await.unwrap();
        let store = CookieStore::load(db.clone()).await;
        assert_eq!(store.header(), None);
        assert!(!store.status().has_session);

        store.save(parse_export(EXPORT).unwrap()).await.unwrap();
        assert_eq!(store.header().as_deref(), Some("c_user=100001; xs=abc%3A1; datr=z"));
        let status = store.status();
        assert!(status.has_session);
        assert_eq!(status.expires_at, Some(4_102_444_800));
        assert_eq!(store.user_id().as_deref(), Some("100001"));

        assert!(store.mark_rejected().await);
        assert!(!store.mark_rejected().await, "only the first rejection counts");
        assert_eq!(store.header(), None);
        let reloaded = CookieStore::load(db).await;
        assert!(reloaded.status().rejected_at.is_some());

        reloaded.save(parse_export(EXPORT).unwrap()).await.unwrap();
        assert!(reloaded.header().is_some(), "new cookies clear the rejection");
    }

    #[tokio::test]
    async fn session_needs_both_cookies() {
        let store = CookieStore::load(Db::memory().await.unwrap()).await;
        let only_user = Cookie {
            name: "c_user".into(),
            value: "1".into(),
            domain: ".facebook.com".into(),
            path: "/".into(),
            expires: None,
        };
        store.save(vec![only_user]).await.unwrap();
        assert!(!store.status().has_session);
        assert_eq!(store.header(), None);
    }
}
