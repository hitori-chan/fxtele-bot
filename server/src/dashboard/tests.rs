//! The dashboard through its router, as a browser uses it: pages render, forms save, sign-in holds.

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use tower::ServiceExt;

use super::*;
use crate::app::tests::app_with;
use crate::cache::Entry;
use crate::extract::fake::FakeExtractor;
use crate::extract::{ExtractError, Source, Stage, Target};
use crate::settings::{CookiePolicy, FailureResponse, MediaLinks, VideoQuality};
use crate::stats::{Media, Outcome};
use crate::video::bucket::tests::FakeS3;

const HOST: &str = "dash.test";

/// A browser: keeps the session cookie between requests.
struct Browser {
    router: Router,
    app: Arc<App>,
    cookie: Option<String>,
}

impl Browser {
    async fn new() -> Self {
        Self::with(app_with(Arc::new(FakeExtractor::default())).await)
    }

    fn with(app: Arc<App>) -> Self {
        Self { router: router(Arc::clone(&app), CancellationToken::new()).unwrap(), app, cookie: None }
    }

    async fn signed_in() -> Self {
        Self::new().await.sign_in().await
    }

    /// Signed in as a passkey would: the whole dashboard.
    async fn sign_in(self) -> Self {
        let mut browser = self;
        let ip = "203.0.113.9".parse().unwrap();
        let token = auth::create_session(&browser.app.db, "passkey", ip, "test").await.unwrap();
        browser.cookie = Some(format!("{}={token}", auth::COOKIE_NAME));
        browser
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<(&str, String)>, origin: Option<&str>) -> Response {
        let origin: Vec<_> = origin.map(|o| (header::ORIGIN.as_str(), o)).into_iter().collect();
        self.send_with(method, path, body, &origin).await
    }

    /// A link followed from one of the dashboard's own pages.
    async fn follow(&mut self, path: &str) -> Response {
        self.send_with("GET", path, None, &[("sec-fetch-site", "same-origin")]).await
    }

    async fn send_with(
        &mut self,
        method: &str,
        path: &str,
        body: Option<(&str, String)>,
        headers: &[(&str, &str)],
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST)
            .extension(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4000))));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let request = match body {
            Some((kind, body)) => request.header(CONTENT_TYPE, kind).body(Body::from(body)),
            None => request.body(Body::empty()),
        };
        let response = self.router.clone().oneshot(request.unwrap()).await.unwrap();
        if let Some(set) = response.headers().get(SET_COOKIE) {
            let pair = set.to_str().unwrap().split(';').next().unwrap();
            self.cookie = (!pair.ends_with('=')).then(|| pair.to_string());
        }
        response
    }

    async fn get(&mut self, path: &str) -> Response {
        self.send("GET", path, None, None).await
    }

    async fn form(&mut self, path: &str, body: &str) -> Response {
        let origin = format!("http://{HOST}");
        self.send("POST", path, Some(("application/x-www-form-urlencoded", body.to_string())), Some(&origin)).await
    }
}

async fn text(response: Response) -> String {
    String::from_utf8(axum::body::to_bytes(response.into_body(), 1 << 22).await.unwrap().to_vec()).unwrap()
}

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

#[tokio::test]
async fn signed_out_visitors_only_reach_the_login_page() {
    let mut b = Browser::new().await;
    let home = b.get("/").await;
    assert_eq!((home.status(), location(&home)), (StatusCode::SEE_OTHER, "/login"));
    assert_eq!(b.form("/cache/clear", "").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(b.get("/events").await.status(), StatusCode::SEE_OTHER);
    // Changes come only from the dashboard's own pages.
    let body = Some(("application/x-www-form-urlencoded", "password=x".to_string()));
    assert_eq!(b.send("POST", "/login/password", body.clone(), None).await.status(), StatusCode::FORBIDDEN);
    let evil = b.send("POST", "/login/password", body, Some("https://evil.example")).await;
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);

    let login = b.get("/login").await;
    assert_eq!(login.status(), StatusCode::OK);
    let headers = login.headers().clone();
    assert!(headers[CONTENT_SECURITY_POLICY].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(headers[X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers["strict-transport-security"], "max-age=31536000");
    assert_eq!(headers[REFERRER_POLICY], "same-origin", "with no-referrer, browsers send Origin: null");
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    let page = text(login).await;
    assert!(
        page.contains("No way to sign in is set up yet") && !page.contains("Locked out?"),
        "one pointer to setup-link"
    );
    assert!(page.contains(&asset("app.css")) && asset("app.css").starts_with("/static/app.css?v="));

    let script = b.get(&asset("app.js")).await;
    assert_eq!(
        (script.status(), &script.headers()[CONTENT_TYPE]),
        (StatusCode::OK, &HeaderValue::from_static("text/javascript"))
    );
    assert_eq!(b.get("/static/nope.js").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(b.get("/_/health").await.status(), StatusCode::OK);
}

/// The first password, typed twice as the form asks.
const NEW_PASSWORD: &str = "password=correct+horse+battery&confirm=correct+horse+battery";

#[tokio::test]
async fn password_and_authenticator_app() {
    let mut b = Browser::signed_in().await;
    let mut visitor = Browser::with(Arc::clone(&b.app));
    assert_eq!(location(&b.form("/security/password", NEW_PASSWORD).await), "/security?done=password");
    // The password alone signs in until the app is on.
    let login = text(visitor.get("/login").await).await;
    assert!(login.contains("name=\"password\"") && !login.contains("name=\"code\""));
    let wrong = visitor.form("/login/password", "password=nope").await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(text(wrong).await.contains("Wrong password."));
    assert_eq!(location(&visitor.form("/login/password", "password=correct+horse+battery").await), "/");
    assert!(text(visitor.get("/security").await).await.contains(">password</td>"));
    assert_eq!(location(&visitor.form("/logout", "").await), "/login");

    // The app is turned on with the key from the page and a code from it.
    assert_eq!(location(&b.form("/security/totp/start", "").await), "/security#totp");
    let page = text(b.get("/security").await).await;
    assert!(page.contains("data:image/svg+xml;base64,"));
    let key = page.split("data-totp-key data-value=\"").nth(1).unwrap().split('"').next().unwrap();
    let secret = data_encoding::BASE32_NOPAD.decode(key.as_bytes()).unwrap();
    let wrong = b.form("/security/totp/confirm", "code=000000").await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    let wrong = text(wrong).await;
    assert!(wrong.contains("Use the newest one") && wrong.contains("aria-invalid=\"true\""), "by the field");
    let code = auth::totp::code_now(&secret, 0);
    assert_eq!(location(&b.form("/security/totp/confirm", &format!("code={code}")).await), "/security?done=totp");

    // Signing in takes the password, then a code on a page of its own; each code works once.
    let login = text(visitor.get("/login").await).await;
    assert!(login.contains("name=\"password\"") && !login.contains("name=\"code\"") && login.contains(">Continue<"));
    assert_eq!(location(&visitor.get("/login/code").await), "/login", "no code before the password");
    assert!(text(visitor.form("/login/password", "password=nope").await).await.contains("Wrong password."));
    let right = visitor.form("/login/password", "password=correct+horse+battery").await;
    assert_eq!(location(&right), "/login/code");
    assert!(visitor.cookie.as_deref().unwrap().starts_with("__Host-signin="), "a ticket for the next step");
    assert_eq!(location(&visitor.get("/").await), "/login", "which isn't a session");
    let step = text(visitor.get("/login/code").await).await;
    assert!(step.contains("name=\"code\"") && !step.contains("name=\"password\"") && step.contains("setup-link"));
    let replayed = visitor.form("/login/code", &format!("code={code}")).await;
    assert_eq!(replayed.status(), StatusCode::UNAUTHORIZED);
    assert!(text(replayed).await.contains("Wrong code."), "the code that turned the app on is used up");
    let next = auth::totp::code_now(&secret, 1);
    let right = visitor.form("/login/code", &format!("code={next}")).await;
    assert_eq!((right.status(), location(&right)), (StatusCode::SEE_OTHER, "/"));
    assert!(visitor.cookie.as_deref().unwrap().starts_with("__Host-session="));
    assert_eq!(location(&visitor.get("/login").await), "/", "already signed in");
    assert!(text(visitor.get("/security").await).await.contains(">password + code</td>"));
    assert_eq!(location(&visitor.form("/logout", "").await), "/login");
    assert!(visitor.cookie.is_none());

    // Five wrong codes end the ticket and lock the address out, even for the right password.
    assert_eq!(location(&visitor.form("/login/password", "password=correct+horse+battery").await), "/login/code");
    for _ in 0..4 {
        assert!(text(visitor.form("/login/code", "code=000000").await).await.contains("Wrong code."));
    }
    let ended = text(visitor.form("/login/code", "code=000000").await).await;
    assert!(ended.contains("Too many wrong codes") && ended.contains("name=\"password\""), "{ended}");
    assert!(visitor.cookie.is_none(), "the ticket is gone");
    let locked = visitor.form("/login/password", "password=correct+horse+battery").await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(text(locked).await.contains("Try again in 1 minutes"));
}

#[tokio::test]
async fn changes_to_signing_in_need_a_recent_sign_in() {
    let mut b = Browser::signed_in().await;
    b.app.db.call(|c| c.execute("UPDATE sessions SET created_at = created_at - 3600", [])).await.unwrap();
    for path in ["/security/totp/start", "/security/password", "/security/totp/remove"] {
        let body = match path {
            "/security/password" => NEW_PASSWORD,
            "/security/totp/remove" => "code=000000",
            _ => "",
        };
        let refused = b.form(path, body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{path}");
        assert!(text(refused).await.contains("need a recent sign-in"), "{path}");
    }
    assert!(!auth::password::is_set(&b.app.db).await.unwrap(), "no first password from an old session");
    // Wrong setup links don't count toward the lockout: any page could make a browser open them.
    let mut visitor = Browser::with(Arc::clone(&b.app));
    for _ in 0..6 {
        assert_eq!(visitor.get("/setup/wrong").await.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(visitor.form("/login/password", "password=nope").await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn changing_the_password_takes_the_current_one_twice_typed_new_one() {
    let mut b = Browser::signed_in().await;
    assert_eq!(location(&b.form("/security/password", NEW_PASSWORD).await), "/security?done=password");
    let page = text(b.get("/security").await).await;
    assert!(page.contains("name=\"current\"") && page.contains("name=\"confirm\""));
    for (body, why) in [
        ("password=plaid+otter+mosaic&confirm=plaid+otter+mosaic", "Enter the current password first."),
        ("current=nope&password=plaid+otter+mosaic&confirm=plaid+otter+mosaic", "The current password is wrong."),
        ("current=correct+horse+battery&password=plaid+otter+mosaic&confirm=plaid+otter", "don&#39;t match"),
        ("current=correct+horse+battery&password=correct+horse+battery&confirm=correct+horse+battery", "already"),
        ("current=correct+horse+battery&password=short&confirm=short", "characters"),
    ] {
        let refused = b.form("/security/password", body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{body}");
        let page = text(refused).await;
        assert!(page.contains(why), "{why}: {page}");
    }
    let changed = "current=correct+horse+battery&password=plaid+otter+mosaic&confirm=plaid+otter+mosaic";
    assert_eq!(location(&b.form("/security/password", changed).await), "/security?done=password");
    let mut visitor = Browser::with(Arc::clone(&b.app));
    assert_eq!(location(&visitor.form("/login/password", "password=plaid+otter+mosaic").await), "/");
    // Removing it takes the current one too.
    let refused = b.form("/security/password/remove", "current=correct+horse+battery").await;
    assert!(text(refused).await.contains("The current password is wrong."));
}

/// Turns the authenticator app on, as the Security page does, and gives its secret.
async fn turn_on_totp(app: &App) -> Vec<u8> {
    let secret = auth::totp::Setup::default().start();
    assert!(auth::totp::enable(&app.db, &secret, &auth::totp::code_now(&secret, 0)).await.unwrap());
    secret
}

#[tokio::test]
async fn setup_links_sign_in_once_to_the_login_settings_only() {
    let mut b = Browser::new().await;
    let link = auth::create_setup_link(&b.app.db, &b.app.config).await.unwrap();
    let page = b.get(&link).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(text(page).await.contains("url=/security\""));
    let mut other = Browser::with(Arc::clone(&b.app));
    assert_eq!(other.get(&link).await.status(), StatusCode::NOT_FOUND, "once");
    assert!(other.cookie.is_none());

    let page = text(b.get("/security").await).await;
    assert!(page.contains("The link is used up") && page.contains("<h2>Passkeys</h2>"));
    assert!(
        !page.contains("href=\"/sites/facebook\"") && !page.contains("<h2>Sessions</h2>"),
        "nothing else, not even the menu"
    );
    assert_eq!(location(&b.get("/").await), "/security");
    assert_eq!(location(&b.get("/sites/facebook").await), "/security");
    assert_eq!(b.form("/cache/clear", "").await.status(), StatusCode::FORBIDDEN);
    assert_eq!(location(&b.form("/security/password", NEW_PASSWORD).await), "/security?done=password");
}

#[tokio::test]
async fn every_page_renders() {
    let mut b = Browser::signed_in().await;
    for (path, heading) in [
        ("/", "Overview"),
        ("/sites/facebook", "Facebook"),
        ("/sites/instagram", "Instagram"),
        ("/previews", "Previews"),
        ("/videos", "Videos"),
        ("/limits", "Rate limits"),
        ("/telegram", "Telegram IPs"),
        ("/playground?url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1", "Playground"),
        ("/requests?site=facebook&kind=failed&q=reel", "Requests"),
        ("/issues?site=instagram&stage=timeout", "Issues"),
        ("/alerts", "Alerts"),
        ("/activity?kind=sign_in", "Activity"),
        ("/cache?site=facebook", "Cache"),
        ("/tests", "Test links"),
        ("/security", "Security"),
        ("/backup", "Backup"),
        ("/?range=7d&site=instagram", "Overview"),
        ("/?range=90d", "Overview"),
        ("/?range=nonsense&site=nonsense", "Overview"),
    ] {
        let response = b.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let page = text(response).await;
        assert!(page.contains(&format!("<h1>{heading}</h1>")), "{path}");
        assert!(page.contains("hx-boost=\"true\"") && page.contains(&asset("htmx.min.js")), "{path}: htmx is on");
        assert!(!page.contains(" style=") && !page.contains("<style") && !page.contains("<script>"), "{path}: CSP");
    }
    for (file, kind) in [
        ("htmx.min.js", "text/javascript"),
        ("icons.svg", "image/svg+xml"),
        ("favicon.png", "image/png"),
        ("homura.webp", "image/webp"),
        ("iv.css", "text/css"),
    ] {
        let response = b.get(&asset(file)).await;
        assert_eq!(response.headers()[CONTENT_TYPE], kind);
        let corp = if file == "iv.css" { "cross-origin" } else { "same-origin" };
        assert_eq!(response.headers()["cross-origin-resource-policy"], corp, "{file}");
        assert!(response.headers()[CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    }
}

#[tokio::test]
async fn long_lists_come_a_page_at_a_time() {
    let mut b = Browser::signed_in().await;
    let reel = Target::from_request(Source::Facebook, "/reel/1").unwrap();
    for _ in 0..150 {
        b.app.stats.record(Outcome::Hit, Some(&reel), Duration::from_millis(5));
    }
    let live = text(b.get("/requests").await).await;
    assert!(live.contains("1–100 of 150") && live.contains("data-feed "), "the first page is live: {live}");
    assert!(live.contains("/requests?upto=150&#38;page=2"), "the next page stays as the log was");
    for _ in 0..10 {
        b.app.stats.record(Outcome::Hit, Some(&reel), Duration::from_millis(5));
    }
    let older = text(b.get("/requests?upto=150&page=2").await).await;
    assert!(older.contains("101–150 of 150") && !older.contains("data-feed "), "new requests don't move it");
    assert!(older.contains("Back to live"));

    for n in 0..60 {
        activity::record(&b.app.db, Kind::Change, "password", &format!("Change {n}")).await;
    }
    let first = text(b.get("/activity").await).await;
    assert!(first.contains("1–50 of 60") && first.contains("Change 59") && !first.contains("Change 9<"));
    let past = text(b.get("/activity?page=9").await).await;
    assert!(past.contains("51–60 of 60") && past.contains("Change 0<"), "a page past the end is the last one");

    let keep = Duration::from_secs(600);
    for n in 0..55 {
        let target = Target::from_request(Source::Facebook, &format!("/reel/{n}")).unwrap();
        b.app.cache.insert(64, target.key().to_string(), Entry::post(FakeExtractor::post(&target), keep)).await;
    }
    let cache = text(b.get("/cache?site=facebook&page=2").await).await;
    assert!(cache.contains("51–55 of 55") && cache.contains("/cache?site=facebook\""), "{cache}");
}

#[tokio::test]
async fn htmx_partials_csv_and_a_login_redirect() {
    let mut b = Browser::signed_in().await;
    let reel = Target::from_request(Source::Facebook, "/reel/1").unwrap();
    for outcome in [Outcome::Hit, Outcome::Redirect, Outcome::Failed(Stage::Timeout)] {
        b.app.stats.record(outcome, Some(&reel), Duration::from_millis(5));
    }
    b.app.stats.fetched(&reel, Duration::from_millis(5));
    let status = text(b.get("/status").await).await;
    assert!(status.starts_with("<details class=\"menu\">") && !status.contains("<html"), "only the menu: {status}");
    assert!(status.contains("id=\"dot-instagram\"") && status.contains("hx-swap-oob"), "and the menu's site dots");
    let overview = text(b.get("/").await).await;
    assert!(overview.contains("class=\"spark\"") && overview.contains("3 in all"), "sparklines and the chart");
    assert!(overview.contains("class=\"s-hit\"") && overview.contains("Fetch time"), "both charts");
    assert!(overview.contains("hx-select=\"#stats\""), "the numbers refresh themselves");
    assert!(overview.contains("50.0%") && overview.contains("1 people clicked through"), "success rate, people");
    assert!(overview.contains("busiest minute 1 of 60 fetches"), "the busiest minute against the budget");
    assert!(
        overview.contains("aria-controls=\"kinds-facebook\"") && overview.contains("id=\"kinds-facebook\" hidden"),
        "each site's kinds of post open from its row"
    );
    assert!(overview.contains("times in UTC"), "UTC until the browser gives its time zone");
    let session = b.cookie.clone().unwrap();
    b.cookie = Some(format!("{session}; tz=Asia%2FTokyo"));
    assert!(text(b.get("/").await).await.contains("times in Asia/Tokyo"));
    b.cookie = Some(format!("{session}; tz=Nowhere%2FAtAll"));
    assert!(text(b.get("/").await).await.contains("times in UTC"), "an unknown zone is ignored");
    b.cookie = Some(session);
    b.app.stats.media(Media::Image, Some(Source::Facebook));
    b.app.stats.media(Media::Mosaic, Some(Source::Facebook));
    let overview = text(b.get("/?site=instagram").await).await;
    assert!(!overview.contains("busiest minute 1"), "one site's page leaves the other out");
    assert!(
        !overview.contains("class=\"disclose\"") && overview.contains("id=\"kinds-instagram\">"),
        "one site's kinds show"
    );
    assert!(text(b.get("/").await).await.contains("<p class=\"served-value\">1</p>"), "media, counted apart");

    let csv = b.get("/export/requests.csv?range=24h").await;
    assert!(csv.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("requests-24h.csv"));
    let csv = text(csv).await;
    assert!(csv.starts_with("start_utc,outcome,requests,mean_ms,p95_ms\n"));
    for outcome in [",hit,1,5,5\n", ",redirect,1,5,5\n", ",failed:timeout,1,5,5\n"] {
        assert_eq!(csv.matches(outcome).count(), 1, "{outcome:?} in {csv}");
    }
    let instagram = text(b.get("/export/requests.csv?range=24h&site=instagram").await).await;
    assert_eq!(instagram.lines().count(), 1, "nothing from Instagram: {instagram}");

    // Signed out, htmx is told to load the login page rather than swap it into the page.
    b.cookie = None;
    let mut request = Request::get("/cache").header(header::HOST, HOST).header("hx-request", "true");
    request = request.extension(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4000))));
    let response = b.router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()["hx-redirect"], "/login");
}

#[tokio::test]
async fn activity_requests_and_issues() {
    let mut b = Browser::signed_in().await;
    let previews = "description_length=200&media_links=direct&failure_response=redirect";
    assert_eq!(location(&b.form("/previews", previews).await), "/previews?done=saved");
    assert_eq!(b.form("/login/password", "password=wrong").await.status(), StatusCode::UNAUTHORIZED);
    let activity = text(b.get("/activity").await).await;
    assert!(activity.contains("Changed the preview settings") && activity.contains("passkey"), "who did it");
    assert!(activity.contains("Refused a sign-in from"), "{activity}");
    let changes = text(b.get("/activity?kind=change").await).await;
    assert!(changes.contains("Changed the preview settings") && !changes.contains("Refused a sign-in"));

    let reel = Target::from_request(Source::Facebook, "/reel/7").unwrap();
    b.app.stats.record(Outcome::Failed(Stage::LoginWall), Some(&reel), Duration::from_millis(900));
    b.app.stats.record(Outcome::Redirect, None, Duration::from_millis(1));
    let requests = text(b.get("/requests?kind=failed").await).await;
    assert!(requests.contains("https://www.facebook.com/reel/7") && requests.contains("login wall"), "{requests}");
    assert!(!requests.contains(">refused<"), "people's requests are filtered out");
    assert!(text(b.get("/requests?site=instagram").await).await.contains("Waiting for requests"));

    b.app.stats.failure(reel.key(), reel.page_type, Stage::LoginWall, "Facebook asked to log in").await.unwrap();
    let issues = text(b.get("/issues").await).await;
    assert!(
        issues.contains("Facebook asked to log in")
            && issues.contains("/playground?url=https%3A%2F%2Fwww.facebook.com%2Freel%2F7")
    );
    assert!(text(b.get("/issues?site=instagram").await).await.contains("No issues"));
}

#[tokio::test]
async fn the_cache_lists_searches_and_forgets() {
    let mut b = Browser::signed_in().await;
    let keep = Duration::from_secs(300);
    let reel = Target::from_request(Source::Facebook, "/reel/1").unwrap();
    let mut post = FakeExtractor::post(&reel);
    post.title = Some("Night market".into());
    b.app.cache.insert(64, reel.key().to_string(), Entry::post(post, keep)).await;
    b.app
        .cache
        .insert(64, "walled".into(), Entry::failure(ExtractError::new(Stage::LoginWall, "login page"), keep))
        .await;

    let page = text(b.get("/cache").await).await;
    assert!(
        page.contains("Night market") && page.contains("2 photos") && page.contains("login_wall: login page"),
        "{page}"
    );
    assert!(
        page.contains("Posts</p><p class=\"kpi-value\">1<") && page.contains("Failures</p><p class=\"kpi-value\">1<")
    );
    let facebook = text(b.get("/cache?site=facebook").await).await;
    assert!(facebook.contains("Night market") && !facebook.contains("login_wall"), "the failure has no site");
    let found = text(b.get("/cache?q=night").await).await;
    assert!(found.contains("Night market") && !found.contains("login_wall"));
    assert!(text(b.get("/cache?q=nothing-like-it").await).await.contains("Nothing matches"));

    assert_eq!(location(&b.form("/cache/remove", "key=walled").await), "/cache?done=forgotten");
    assert!(b.app.cache.get("walled").await.is_none());
    assert!(b.app.cache.get(reel.key()).await.is_some());
}

#[tokio::test]
async fn settings_export_and_import() {
    let mut b = Browser::signed_in().await;
    let export = b.get("/backup/settings.json").await;
    assert!(export.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("settings.json"));
    let mut settings: serde_json::Value = serde_json::from_str(&text(export).await).unwrap();
    settings["instagram"]["site_name"] = json!("Imported");
    settings["cache_minutes"] = json!(15);
    let body = format!(
        "settings={}",
        url::form_urlencoded::byte_serialize(settings.to_string().as_bytes()).collect::<String>()
    );
    assert_eq!(location(&b.form("/backup/import", &body).await), "/backup?done=imported");
    let s = b.app.settings.get();
    assert_eq!((s.site_name(Source::Instagram), s.cache_minutes), ("Imported", 15));

    // Missing settings get their defaults; wrong ones change nothing.
    assert_eq!(location(&b.form("/backup/import", "settings=%7B%7D").await), "/backup?done=imported");
    assert_eq!(b.app.settings.get().site_name(Source::Instagram), "Instagram");
    let wrong = b.form("/backup/import", "settings=%7B%22instagram%22%3A%7B%22concurrency%22%3A0%7D%7D").await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert!(text(wrong).await.contains("fetches per minute and at once must be at least 1"));
    // A backup from before Instagram: Facebook's settings were at the top.
    let old = "settings=%7B%22site_name%22%3A%22Old%22%2C%22facebook_concurrency%22%3A3%7D";
    assert_eq!(location(&b.form("/backup/import", old).await), "/backup?done=imported");
    let s = b.app.settings.get();
    assert_eq!((s.site_name(Source::Facebook), s.facebook.concurrency), ("Old", 3));
    let junk = b.form("/backup/import", "settings=nope").await;
    assert_eq!(junk.status(), StatusCode::BAD_REQUEST);
    assert!(text(junk).await.contains("a settings file from this dashboard"));
    assert_eq!(b.app.settings.get().instagram.concurrency, crate::settings::Settings::default().instagram.concurrency);
}

#[tokio::test]
async fn settings_save_and_mistakes_are_refused() {
    // Settings::check's own tests cover the rules; these check each form reaches it.
    let mut b = Browser::signed_in().await;
    let limits = "ip_requests_per_minute=30&extract_timeout_seconds=6&facebook_fetches_per_minute=20&facebook_concurrency=2\
                  &instagram_fetches_per_minute=10&instagram_concurrency=1";
    assert_eq!(location(&b.form("/limits", limits).await), "/limits?done=saved");
    let s = b.app.settings.get();
    assert_eq!((s.ip_requests_per_minute, s.extract_timeout_seconds, s.instagram.fetches_per_minute), (30, 6, 10));
    let refused = b
        .form("/limits", &limits.replace("=30", "=31").replace("instagram_concurrency=1", "instagram_concurrency=0"))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let page = text(refused).await;
    assert!(page.contains("at least 1") && page.contains("value=\"31\""), "kept as typed");
    let missing = b.form("/limits", "ip_requests_per_minute=31").await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(b.app.settings.get().ip_requests_per_minute, 30);

    let site = "proxy=socks5://127.0.0.1:40000&cookie_policy=never&site_name=IG&author_profiles=on";
    assert_eq!(location(&b.form("/sites/instagram", site).await), "/sites/instagram?done=saved");
    // About the author: on by default for Facebook alone, so each way is seen to change.
    assert_eq!(
        location(&b.form("/sites/facebook", "proxy=&cookie_policy=fallback&site_name=").await),
        "/sites/facebook?done=saved"
    );
    assert_eq!(b.form("/sites/myspace", site).await.status(), StatusCode::NOT_FOUND);
    let bad_proxy = b.form("/sites/instagram", "proxy=ftp://x&cookie_policy=never&site_name=").await;
    assert_eq!(bad_proxy.status(), StatusCode::BAD_REQUEST);
    assert!(text(bad_proxy).await.contains("value=\"ftp://x\""), "kept as typed");
    let cache = "cache_minutes=30&failure_cache_minutes=2&cache_megabytes=32";
    assert_eq!(location(&b.form("/cache", cache).await), "/cache?done=saved");
    let previews = "description_length=200&failure_response=page&media_links=redirect&iv_photo=on&iv_album=on\
                    &iv_profile=on&iv_counts=on";
    assert_eq!(location(&b.form("/previews", previews).await), "/previews?done=saved");
    let s = b.app.settings.get();
    assert_eq!(
        (s.instagram.cookie_policy, s.facebook.cookie_policy, s.failure_response, s.media_links),
        (CookiePolicy::Never, CookiePolicy::Fallback, FailureResponse::Page, MediaLinks::Redirect)
    );
    assert_eq!(
        (s.instagram.proxy.as_deref(), s.site_name(Source::Instagram)),
        (Some("socks5://127.0.0.1:40000"), "IG")
    );
    assert_eq!((s.cache_minutes, s.failure_cache_minutes, s.cache_megabytes, s.description_length), (30, 2, 32, 200));
    assert!(s.instant_view.photo && s.instant_view.album && s.instant_view.profile);
    assert!(!s.instant_view.video && !s.instant_view.text);
    assert!(s.instagram.author_profiles && !s.facebook.author_profiles);
    assert!(s.instant_view_counts && !s.photo_mosaic, "switches left off are off");
    assert!(!s.discord, "Discord's cards, on by default, are off when left off");

    let videos = "video_quality=standard&video_megabytes=15&video_space_megabytes=500&video_days=7";
    assert_eq!(location(&b.form("/videos", videos).await), "/videos?done=saved");
    let s = b.app.settings.get();
    assert_eq!(
        (s.video_quality, s.video_megabytes, s.video_space_megabytes, s.video_days),
        (VideoQuality::Standard, 15, 500, 7)
    );
    let refused = b.form("/videos", &videos.replace("=standard", "=best")).await;
    assert!(text(refused).await.contains("none is set up"), "1080p needs a bucket");

    // Test links: several pasted at once (a one-line field drops the line breaks), checked in the
    // background.
    let (reel, post) = ("https://www.facebook.com/reel/1", "https://www.instagram.com/p/DfahZrMjAqq/");
    let enc = |l: &str| url::form_urlencoded::byte_serialize(l.as_bytes()).collect::<String>();
    let pasted = format!("url={}{}", enc(reel), enc(post));
    assert_eq!(location(&b.form("/tests/add", &pasted).await), "/tests?done=tests_added");
    assert_eq!(b.app.settings.get().test_links, [reel, post]);
    for _ in 0..100 {
        if crate::canary::results(&b.app).await.unwrap().len() == 2 && b.app.canary.progress().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let page = text(b.get("/tests").await).await;
    assert!(page.contains("2 <small>of 2</small>"), "both were checked as they were added");
    let instagram = text(b.get("/tests?site=instagram&state=passing").await).await;
    assert!(instagram.contains(post) && !instagram.contains(reel), "filtered by site");
    let none = text(b.get("/tests?state=failing").await).await;
    assert!(none.contains("No links match") && !none.contains(reel));
    assert_eq!(
        location(&b.form("/tests/check", &format!("url={}&site=facebook&page=1", enc(reel))).await),
        "/tests?done=checked&site=facebook&page=1",
        "back to the list as it was"
    );
    let again = b.form("/tests/add", &format!("url={}", enc(reel))).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(text(again).await.contains("tested already"));
    let wrong = text(b.form("/tests/add", "url=https%3A%2F%2Fexample.com%2Fa").await).await;
    assert!(wrong.contains("t a Facebook or Instagram link"), "only the sites' links");
    assert_eq!(location(&b.form("/tests/check", &format!("url={}", enc(reel))).await), "/tests?done=checked");
    assert_eq!(location(&b.form("/tests/remove", &format!("url={}", enc(reel))).await), "/tests?done=test_removed");
    assert_eq!(b.app.settings.get().test_links, [post]);
    assert_eq!(crate::canary::history(&b.app).await.unwrap().len(), 1, "its history went with it");
    assert_eq!(location(&b.form("/alerts", "alert_chat=-100123").await), "/alerts?done=saved");
    assert_eq!(b.app.settings.get().alert_chat, Some(-100_123));
    assert_eq!(b.form("/alerts", "alert_chat=abc").await.status(), StatusCode::BAD_REQUEST);
    let alert = b.form("/alerts/test", "").await;
    assert_eq!(alert.status(), StatusCode::BAD_REQUEST);
    assert!(text(alert).await.contains("Set TELEGRAM_BOT_TOKEN"), "a test alert needs the token");
}

#[tokio::test(flavor = "multi_thread")]
async fn videos_page_shows_how_the_bucket_does() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, config) = FakeS3::start().await;
    let extractor = Arc::new(FakeExtractor::default());
    let app = crate::app::tests::app_with_data(extractor, dir.path(), Some(config)).await;
    let mut b = Browser::with(app).sign_in().await;
    let form = "video_quality=best&video_megabytes=20&video_space_megabytes=1024&video_days=3";
    assert_eq!(location(&b.form("/videos", form).await), "/videos?done=saved");
    assert_eq!(b.app.settings.get().video_quality, VideoQuality::Best);
    assert_eq!(fake.lists(), 0, "saving asks nothing of the bucket");

    let page = text(b.get("/videos").await).await;
    assert!(page.contains("<code>videos</code>") && page.contains("Works"), "the first look lists it");
    assert!(page.contains("0 files;"));
    assert!(page.contains("DNS only"), "not R2: the note about Cloudflare's proxy");

    // A video the bucket refuses: 1080p pauses, and the dashboard says why.
    *fake.refuse.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let addr = crate::video::tests::cdn().await;
    let name = crate::video::Videos::file_name(Source::Facebook, "0123456789abcdef", 0).unwrap();
    let hd = crate::video::tests::hd(addr, "video.mp4");
    b.app.videos.get(&name, &hd, "post", &b.app.settings.get()).await.unwrap_err();
    let page = text(b.get("/videos").await).await;
    assert!(page.contains("Failing") && page.contains("Telegram gets 720p"), "{page}");
    assert!(text(b.get("/").await).await.contains("the bucket failed"));
    *fake.refuse.lock().unwrap() = None;
    assert_eq!(location(&b.form("/videos/delete", "").await), "/videos?done=videos_deleted");
    assert_eq!(fake.lists(), Source::ALL.len(), "listed once all along, a call per site's prefix");
}

#[tokio::test]
async fn cookies_are_saved_from_a_browser_export() {
    let mut b = Browser::signed_in().await;
    let (keep, wall) = (Duration::from_secs(300), ExtractError::new(Stage::LoginWall, "login page"));
    let post = FakeExtractor::post(&Target::from_request(Source::Facebook, "/reel/1").unwrap());
    b.app.cache.insert(64, "walled".into(), Entry::failure(wall, keep)).await;
    b.app.cache.insert(64, "fine".into(), Entry::post(post, keep)).await;
    let export = r#"[{"name":"c_user","value":"1","domain":".facebook.com"},{"name":"xs","value":"secret-xs","domain":".facebook.com"}]"#;
    let body = format!("cookies={}", url::form_urlencoded::byte_serialize(export.as_bytes()).collect::<String>());
    assert_eq!(location(&b.form("/sites/facebook/cookies", &body).await), "/sites/facebook?done=cookies");
    assert!(b.app.cookies.facebook.status().has_session);
    assert_eq!(b.app.cookies.instagram.status().count, 0, "each site has its own");
    let page = text(b.get("/sites/facebook").await).await;
    assert!(page.contains("<code>c_user</code>") && page.contains("<code>xs</code>"), "the names are listed");
    assert!(!page.contains("secret-xs"), "never the values");
    assert!(b.app.cache.get("walled").await.is_none(), "cookies may get past the login wall now");
    assert!(b.app.cache.get("fine").await.is_some());
    // Another site's cookies are refused rather than saved where they'd never be sent.
    assert_eq!(b.form("/sites/instagram/cookies", &body).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(b.form("/sites/facebook/cookies", "cookies=nope").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(location(&b.form("/sites/facebook/cookies/clear", "").await), "/sites/facebook?done=cookies_removed");
    assert_eq!(b.app.cookies.facebook.status().count, 0);
}

#[tokio::test]
async fn sessions_end_one_at_a_time_or_all_together() {
    let mut b = Browser::signed_in().await;
    let mut other = Browser::with(Arc::clone(&b.app)).sign_in().await;
    let id = |browser: &Browser| {
        let token = browser.cookie.as_deref().unwrap().split_once('=').unwrap().1;
        data_encoding::HEXLOWER.encode(&auth::hash(token))
    };
    let (mine, theirs) = (id(&b), id(&other));
    let page = text(b.get("/security").await).await;
    assert!([&mine, &theirs].iter().all(|id| page.contains(&format!("/security/sessions/{id}/end"))));
    assert!(page.contains(">passkey</td>") && page.contains("this one"));
    assert!(page.contains("href=\"/sites/instagram\""), "the menu, for a full session");

    let ended = b.form(&format!("/security/sessions/{theirs}/end"), "").await;
    assert_eq!(location(&ended), "/security?done=session_ended");
    assert_eq!(other.get("/").await.status(), StatusCode::SEE_OTHER, "signed out");
    assert_eq!(location(&b.form(&format!("/security/sessions/{mine}/end"), "").await), "/login", "its own");
    assert!(b.cookie.is_none());

    let mut b = b.sign_in().await;
    let _other = Browser::with(Arc::clone(&b.app)).sign_in().await;
    assert_eq!(location(&b.form("/security/sessions/clear", "").await), "/login");
    assert!(b.cookie.is_none());
    assert!(auth::sessions(&b.app.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn playground_extracts_then_reads_the_cache() {
    let mut b = Browser::signed_in().await;
    let link = "url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1%3Fs%3Dx";
    let fresh = text(b.form("/playground", &format!("{link}&fresh=on")).await).await;
    assert!(fresh.contains("reel</h2>") && fresh.contains(">extracted</span>"), "{fresh}");
    assert!(fresh.contains("/static/iv.css?v="), "the Instant View frame looks like Telegram's");
    assert!(fresh.contains("class=\"tg-preview") && fresh.contains("srcdoc=\"") && fresh.contains("data-iv"));
    assert!(fresh.contains("for=\"iv-open\"") && fresh.contains("id=\"iv-open\""), "the button opens the article");
    assert!(fresh.contains("og:title") && fresh.contains("class=\"waterfall\""), "the tags and the timings");
    let cached = text(b.form("/playground", link).await).await;
    assert!(cached.contains(">from the cache</span>"));
    assert!(
        cached.contains("Recent")
            && cached.matches("/playground?url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1").count() == 1,
        "remembered once"
    );
    let refused = b.form("/playground", "url=https%3A%2F%2Fexample.com%2F").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    // `?run=1` tries the link at once only when the dashboard linked to it.
    let path = "/playground?url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1&run=1";
    assert!(text(b.follow(path).await).await.contains("data-autorun"));
    assert!(!text(b.get(path).await).await.contains("data-autorun"), "a link from another site");
}

#[tokio::test]
async fn the_last_way_to_sign_in_stays() {
    let mut b = Browser::signed_in().await;
    let mut other = Browser::with(Arc::clone(&b.app)).sign_in().await;
    assert_eq!(location(&b.form("/security/password", NEW_PASSWORD).await), "/security?done=password");
    assert_eq!(other.get("/").await.status(), StatusCode::SEE_OTHER, "a new password signs other browsers out");
    assert_eq!(b.get("/").await.status(), StatusCode::OK, "but not this one");
    // With no passkey, the password is the only way in: it can't go, but the app can.
    let secret = turn_on_totp(&b.app).await;
    let refused = b.form("/security/password/remove", "current=correct+horse+battery").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(text(refused).await.contains("the last way to sign in"));
    assert!(auth::password::is_set(&b.app.db).await.unwrap());
    // Turning the app off takes a code from it.
    let refused = b.form("/security/totp/remove", "code=000000").await;
    assert!(text(refused).await.contains("That code didn"));
    let code = format!("code={}", auth::totp::code_now(&secret, 1));
    assert_eq!(location(&b.form("/security/totp/remove", &code).await), "/security?done=totp_removed");
}
