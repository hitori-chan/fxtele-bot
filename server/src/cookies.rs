//! Each site's login cookies: pasted or uploaded in the dashboard (a JSON export such as
//! Cookie-Editor's, a Netscape `cookies.txt`, or a `Cookie` header), stored in SQLite, never
//! logged or shown: the dashboard lists their names and expiry dates only.

use std::sync::Arc;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;
use crate::extract::Source;

const fn db_key(source: Source) -> &'static str {
    match source {
        Source::Facebook => "facebook_cookies",
        Source::Instagram => "instagram_cookies",
    }
}

/// The cookies that make a logged-in session; the first is the account's ID.
pub const fn session_cookies(source: Source) -> &'static [&'static str] {
    match source {
        Source::Facebook => &["c_user", "xs"],
        Source::Instagram => &["ds_user_id", "sessionid"],
    }
}

fn site_domain(source: Source) -> &'static str {
    match source {
        Source::Facebook => "facebook.com",
        Source::Instagram => "instagram.com",
    }
}

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
    /// Set when the site answered these cookies with a login page.
    rejected_at: Option<i64>,
}

/// What the dashboard shows about the saved cookies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub count: usize,
    pub has_session: bool,
    /// When the first of the session cookies expires.
    pub expires_at: Option<i64>,
    pub saved_at: Option<i64>,
    pub rejected_at: Option<i64>,
}

/// A saved cookie as the dashboard lists it: no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub domain: String,
    pub expires_at: Option<i64>,
    /// One of those that make the logged-in session (`c_user` and `xs` on Facebook).
    pub session: bool,
}

/// Parses pasted or uploaded cookies, in any of three forms:
/// - a JSON list of cookie objects, as Cookie-Editor and other exporters produce. Accepts
///   `expirationDate` for `expires` and `host` for `domain`; the path defaults to `/`.
/// - a Netscape `cookies.txt`, as "Get cookies.txt" extensions and yt-dlp write it: one cookie
///   per line, seven fields split by tabs.
/// - a `Cookie` header (`c_user=…; xs=…`), as copied from the browser's developer tools or
///   Cookie-Editor's header export, with or without the `Cookie:` name. It carries no expiry dates.
///
/// Expired cookies and other sites' are dropped. Refused without the session cookies
/// ([`session_cookies`]).
pub fn parse_export(source: Source, text: &str) -> Result<Vec<Cookie>> {
    let text = text.trim();
    let text = text.trim_start_matches('\u{feff}');
    let cookies = if text.starts_with(['[', '{']) {
        parse_json(source, text)?
    } else if is_netscape(text) {
        parse_netscape(source, text)
    } else {
        parse_header(source, text)
    };
    let (name, session) = (source.name(), session_cookies(source));
    if cookies.is_empty() {
        let header = session.iter().map(|c| format!("{c}=…")).collect::<Vec<_>>().join("; ");
        bail!(
            "no {name} cookies there; use a JSON export (Cookie-Editor's Export → JSON), a cookies.txt file, or a Cookie header ({header})"
        );
    }
    let missing: Vec<&str> = session.iter().copied().filter(|name| !cookies.iter().any(|c| c.name == *name)).collect();
    if !missing.is_empty() {
        bail!(
            "{} missing, so these aren't a signed-in session; copy them while logged in to {name}",
            missing.join(" and ")
        );
    }
    Ok(cookies)
}

/// The site's domain or one of its subdomains, with or without the leading dot.
fn on_site(source: Source, domain: &str) -> bool {
    let (domain, site) = (domain.trim_start_matches('.'), site_domain(source));
    domain == site || domain.strip_suffix(site).is_some_and(|sub| sub.ends_with('.'))
}

fn valid(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
}

fn parse_json(source: Source, text: &str) -> Result<Vec<Cookie>> {
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
        let domain = text("domain").or_else(|| text("host")).unwrap_or_else(|| format!(".{}", site_domain(source)));
        if !on_site(source, &domain) {
            continue;
        }
        if name.is_empty() || !valid(&name) || !valid(&value) {
            continue;
        }
        cookies.push(Cookie { name, value, domain, path: text("path").unwrap_or_else(|| "/".into()), expires });
    }
    Ok(cookies)
}

fn is_netscape(text: &str) -> bool {
    text.starts_with("# Netscape HTTP Cookie File")
        || text.starts_with("# HTTP Cookie File")
        || text.lines().any(|line| line.split('\t').count() == 7)
}

/// `domain  subdomains  path  secure  expires  name  value`, split by tabs. Lines starting with
/// `#` are comments, except `#HttpOnly_`, which marks an HTTP-only cookie's domain.
fn parse_netscape(source: Source, text: &str) -> Vec<Cookie> {
    let now = jiff::Timestamp::now().as_second();
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [domain, _, path, _, expires, name, value] = fields[..] else { return None };
            let expires: i64 = expires.trim().parse().ok()?;
            let fresh = expires <= 0 || expires > now;
            (on_site(source, domain) && fresh && !name.is_empty() && valid(name) && valid(value)).then(|| Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: domain.to_string(),
                path: if path.is_empty() { "/".into() } else { path.to_string() },
                #[expect(clippy::cast_precision_loss, reason = "Unix seconds fit in an f64")]
                expires: (expires > 0).then_some(expires as f64),
            })
        })
        .collect()
}

fn parse_header(source: Source, text: &str) -> Vec<Cookie> {
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
                domain: format!(".{}", site_domain(source)),
                path: "/".into(),
                expires: None,
            })
        })
        .collect()
}

/// Every site's cookie store.
pub struct Cookies {
    pub facebook: Arc<CookieStore>,
    pub instagram: Arc<CookieStore>,
}

impl Cookies {
    pub async fn load(db: &Db) -> Self {
        Self {
            facebook: Arc::new(CookieStore::load(db.clone(), Source::Facebook).await),
            instagram: Arc::new(CookieStore::load(db.clone(), Source::Instagram).await),
        }
    }

    pub const fn of(&self, source: Source) -> &Arc<CookieStore> {
        match source {
            Source::Facebook => &self.facebook,
            Source::Instagram => &self.instagram,
        }
    }
}

pub struct CookieStore {
    db: Db,
    source: Source,
    state: ArcSwap<Stored>,
}

impl CookieStore {
    pub async fn load(db: Db, source: Source) -> Self {
        let stored = match db.get_json::<Stored>(db_key(source)).await {
            Ok(stored) => stored.unwrap_or_default(),
            Err(e) => {
                tracing::error!("saved {} cookies are unreadable; paste them again: {e:#}", source.name());
                Stored::default()
            }
        };
        Self { db, source, state: ArcSwap::from_pointee(stored) }
    }

    pub const fn source(&self) -> Source {
        self.source
    }

    pub async fn save(&self, cookies: Vec<Cookie>) -> Result<()> {
        let stored = Stored { cookies, saved_at: jiff::Timestamp::now().as_second(), rejected_at: None };
        self.db.set_json(db_key(self.source), &stored).await?;
        self.state.store(Arc::new(stored));
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        self.db.delete(db_key(self.source)).await?;
        self.state.store(Arc::new(Stored::default()));
        Ok(())
    }

    /// The `Cookie` header for a logged-in request, if the saved cookies look usable.
    pub fn header(&self) -> Option<String> {
        let state = self.state.load();
        let status = status_of(self.source, &state);
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

    /// One saved cookie's value (Instagram wants `csrftoken` repeated in a header).
    pub fn value(&self, name: &str) -> Option<String> {
        self.state.load().cookies.iter().find(|c| c.name == name).map(|c| c.value.clone())
    }

    /// The logged-in user's ID, to scrub from saved test pages.
    pub fn user_id(&self) -> Option<String> {
        let id = session_cookies(self.source)[0];
        self.state.load().cookies.iter().find(|c| c.name == id).map(|c| c.value.clone())
    }

    /// The site answered the cookies with a login page. Returns `true` the first time.
    pub async fn mark_rejected(&self) -> bool {
        let mut stored = (**self.state.load()).clone();
        if stored.rejected_at.is_some() || stored.cookies.is_empty() {
            return false;
        }
        stored.rejected_at = Some(jiff::Timestamp::now().as_second());
        if let Err(e) = self.db.set_json(db_key(self.source), &stored).await {
            tracing::error!("can't save the cookie status: {e:#}");
        }
        self.state.store(Arc::new(stored));
        true
    }

    pub fn status(&self) -> Status {
        status_of(self.source, &self.state.load())
    }

    /// The saved cookies without their values, session cookies first.
    pub fn list(&self) -> Vec<Listed> {
        let mut listed: Vec<Listed> = self
            .state
            .load()
            .cookies
            .iter()
            .map(|c| Listed {
                name: c.name.clone(),
                domain: c.domain.clone(),
                #[expect(clippy::cast_possible_truncation, reason = "Unix seconds")]
                expires_at: c.expires.filter(|t| *t > 0.0).map(|t| t as i64),
                session: session_cookies(self.source).contains(&c.name.as_str()),
            })
            .collect();
        listed.sort_by(|a, b| b.session.cmp(&a.session).then_with(|| a.name.cmp(&b.name)));
        listed
    }
}

fn status_of(source: Source, stored: &Stored) -> Status {
    let names = session_cookies(source);
    let session: Vec<&Cookie> =
        names.iter().filter_map(|name| stored.cookies.iter().find(|c| c.name == *name)).collect();
    let now = jiff::Timestamp::now().as_second();
    let expires_at = session.iter().filter_map(|c| c.expires).filter(|t| *t > 0.0).map(|t| t as i64).min();
    Status {
        count: stored.cookies.len(),
        has_session: session.len() == names.len() && expires_at.is_none_or(|t| t > now),
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
        {"name": "lookalike", "value": "y", "domain": ".notfacebook.com"},
        {"name": "datr", "value": "z", "domain": ".facebook.com", "expires": false},
        {"name": "bad", "value": "a;b", "domain": ".facebook.com"},
        "not an object"
    ]"#;

    #[test]
    fn parses_json_exports() {
        let cookies = parse_export(Source::Facebook, EXPORT).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c_user", "xs", "datr"], "expired, other sites', broken and non-cookie entries dropped");
        assert_eq!((cookies[1].domain.as_str(), cookies[0].path.as_str()), (".facebook.com", "/"));
        assert!(parse_export(Source::Facebook, "{}").is_err() && parse_export(Source::Facebook, "[]").is_err());

        // Cookie-Editor's Export → JSON, as it writes it (values shortened).
        let editor = r#"[
            {"domain": ".facebook.com", "expirationDate": 4102444800.738294, "hostOnly": false, "httpOnly": true, "name": "xs",
             "path": "/", "sameSite": "no_restriction", "secure": true, "session": false, "storeId": null, "value": "12%3AaBcD"},
            {"domain": ".facebook.com", "hostOnly": false, "httpOnly": false, "name": "c_user", "path": "/", "sameSite": null,
             "secure": true, "session": true, "storeId": null, "value": "100001"}
        ]"#;
        let cookies = parse_export(Source::Facebook, editor).unwrap();
        assert_eq!(
            (cookies[0].expires, cookies[1].expires),
            (Some(4_102_444_800.738_294), None),
            "then a session cookie"
        );
    }

    #[test]
    fn parses_netscape_files() {
        let file = "# Netscape HTTP Cookie File\n\
                    # This is a generated file! Do not edit.\n\
                    \n\
                    .facebook.com\tTRUE\t/\tTRUE\t4102444800\tc_user\t100001\n\
                    #HttpOnly_.facebook.com\tTRUE\t/\tTRUE\t4102444800\txs\tabc%3A1\r\n\
                    .facebook.com\tTRUE\t/\tTRUE\t1000\told\tx\n\
                    .example.com\tTRUE\t/\tFALSE\t0\tother\ty\n\
                    .facebook.com\tTRUE\t/\tTRUE\t0\tpresence\tz\n";
        let cookies = parse_export(Source::Facebook, file).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c_user", "xs", "presence"], "comments, expired and other sites' cookies dropped");
        assert_eq!(cookies[1].value, "abc%3A1", "an HTTP-only cookie, without the line's \\r");
        assert_eq!((cookies[0].expires, cookies[2].expires), (Some(4_102_444_800.0), None));
        // Without the header line too, as some extensions write it.
        let bare = ".facebook.com\tTRUE\t/\tTRUE\t0\tc_user\t1\n.facebook.com\tTRUE\t/\tTRUE\t0\txs\t2";
        assert_eq!(parse_export(Source::Facebook, bare).unwrap().len(), 2);
    }

    #[test]
    fn parses_cookie_headers() {
        let cookies =
            parse_export(Source::Facebook, "datr=a1; sb=b2; c_user=100001; xs=12%3AaBcD%3A2; wd=1920x1080").unwrap();
        let pairs: Vec<(&str, &str)> = cookies.iter().map(|c| (c.name.as_str(), c.value.as_str())).collect();
        assert_eq!(
            pairs,
            [("datr", "a1"), ("sb", "b2"), ("c_user", "100001"), ("xs", "12%3AaBcD%3A2"), ("wd", "1920x1080")]
        );
        assert!(cookies.iter().all(|c| c.domain == ".facebook.com" && c.path == "/" && c.expires.is_none()));

        assert_eq!(parse_export(Source::Facebook, "Cookie: c_user=1; xs=2\n").unwrap().len(), 2);
        let headers = "accept: text/html\nCOOKIE: c_user=1; xs=2; presence=x\nuser-agent: Mozilla/5.0";
        assert_eq!(
            parse_export(Source::Facebook, headers).unwrap().len(),
            3,
            "the Cookie line from copied request headers"
        );
    }

    #[test]
    fn refuses_pastes_without_a_session() {
        let missing = parse_export(Source::Facebook, "datr=a1; c_user=100001").unwrap_err().to_string();
        assert!(missing.starts_with("xs missing"), "{missing}");
        assert!(
            parse_export(Source::Facebook, "datr=a1; sb=b2")
                .unwrap_err()
                .to_string()
                .starts_with("c_user and xs missing")
        );
        assert!(
            parse_export(Source::Facebook, "hello there").unwrap_err().to_string().starts_with("no Facebook cookies")
        );
        assert!(parse_export(Source::Facebook, "").is_err());
        // Each site's names, however many: Reddit has one.
        for source in Source::ALL {
            let error = parse_export(source, "").unwrap_err().to_string();
            assert!(error.contains(&format!("{}=…)", session_cookies(source).last().unwrap())), "{error}");
        }
    }

    #[test]
    fn instagram_has_its_own_cookies() {
        let export = r#"[
            {"name": "sessionid", "value": "1%3Aab", "domain": ".instagram.com", "expirationDate": 4102444800},
            {"name": "ds_user_id", "value": "42", "domain": ".instagram.com"},
            {"name": "c_user", "value": "1", "domain": ".facebook.com"},
            {"name": "csrftoken", "value": "t", "domain": "www.instagram.com"}
        ]"#;
        let names: Vec<_> = parse_export(Source::Instagram, export).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["sessionid", "ds_user_id", "csrftoken"], "Facebook's dropped");
        let error = parse_export(Source::Instagram, "sessionid=1").unwrap_err().to_string();
        assert!(error.contains("ds_user_id missing") && error.contains("logged in to Instagram"), "{error}");
        assert!(parse_export(Source::Facebook, export).is_err(), "Facebook's alone aren't a session");
    }

    #[tokio::test]
    async fn store_status_header_and_rejection() {
        let db = Db::memory().await.unwrap();
        let store = CookieStore::load(db.clone(), Source::Facebook).await;
        assert_eq!(store.header(), None);
        // A session needs both c_user and xs, even in older saved cookies.
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

        store.save(parse_export(Source::Facebook, EXPORT).unwrap()).await.unwrap();
        assert_eq!(store.header().as_deref(), Some("c_user=100001; xs=abc%3A1; datr=z"));
        let status = store.status();
        assert!(status.has_session);
        assert_eq!(status.expires_at, Some(4_102_444_800));
        assert_eq!(store.user_id().as_deref(), Some("100001"));
        let listed: Vec<(String, bool)> = store.list().into_iter().map(|c| (c.name, c.session)).collect();
        let expected = [("c_user", true), ("xs", true), ("datr", false)].map(|(n, s)| (n.to_string(), s));
        assert_eq!(listed, expected, "session cookies first, never with values");

        assert!(store.mark_rejected().await);
        assert!(!store.mark_rejected().await, "only the first rejection counts");
        assert_eq!(store.header(), None);
        let reloaded = CookieStore::load(db, Source::Facebook).await;
        assert!(reloaded.status().rejected_at.is_some());

        reloaded.save(parse_export(Source::Facebook, EXPORT).unwrap()).await.unwrap();
        assert!(reloaded.header().is_some(), "new cookies clear the rejection");
    }
}
