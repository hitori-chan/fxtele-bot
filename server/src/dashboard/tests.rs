use std::net::SocketAddr;

use axum::body::Body;
use axum::http::{Request, header};
use tower::ServiceExt;

use super::*;
use crate::app::tests::app_with;
use crate::cache::Entry;
use crate::extract::facebook::url::Target;
use crate::extract::fake::FakeExtractor;
use crate::extract::{ExtractError, Stage};
use crate::settings::{CookiePolicy, FailureResponse, MediaLinks, VideoQuality};
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

    async fn sign_in(self) -> Self {
        let mut browser = self;
        let link = auth::create_setup_link(&browser.app.db, &browser.app.config).await.unwrap();
        assert_eq!(browser.get(&link).await.status(), StatusCode::OK);
        assert!(browser.cookie.is_some());
        browser
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<(&str, String)>, origin: Option<&str>) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST)
            .extension(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4000))));
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
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
    assert_eq!(home.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&home), "/login");
    assert_eq!(b.form("/cache/clear", "").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(b.get("/events").await.status(), StatusCode::SEE_OTHER);

    let login = b.get("/login").await;
    assert_eq!(login.status(), StatusCode::OK);
    let headers = login.headers().clone();
    assert!(headers[CONTENT_SECURITY_POLICY].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(headers[X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers[REFERRER_POLICY], "same-origin", "with no-referrer, browsers send Origin: null");
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    assert!(text(login).await.contains("No password or passkey is set up yet"));

    let page = text(b.get("/login").await).await;
    assert!(page.contains(&asset("app.css")) && asset("app.css").starts_with("/static/app.css?v="));
    let script = b.get(&asset("app.js")).await;
    assert_eq!(script.status(), StatusCode::OK);
    assert_eq!(script.headers()[CONTENT_TYPE], "text/javascript");
    assert_eq!(b.get("/static/nope.js").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(b.get("/_/health").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn changes_need_the_dashboards_own_origin() {
    let mut b = Browser::new().await;
    let body = Some(("application/x-www-form-urlencoded", "password=x".to_string()));
    assert_eq!(b.send("POST", "/login/password", body.clone(), None).await.status(), StatusCode::FORBIDDEN);
    let evil = b.send("POST", "/login/password", body, Some("https://evil.example")).await;
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn password_sign_in_and_out() {
    let mut b = Browser::new().await;
    auth::password::set(&b.app.db, "correct horse battery").await.unwrap();
    assert!(text(b.get("/login").await).await.contains("name=\"password\""));

    let wrong = b.form("/login/password", "password=nope").await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(text(wrong).await.contains("Wrong password"));
    assert!(b.cookie.is_none());

    let right = b.form("/login/password", "password=correct+horse+battery").await;
    assert_eq!((right.status(), location(&right)), (StatusCode::SEE_OTHER, "/"));
    assert!(b.cookie.as_deref().unwrap().starts_with("__Host-session="));
    assert!(text(b.get("/").await).await.contains("<h1>Overview</h1>"));
    assert_eq!(location(&b.get("/login").await), "/", "already signed in");

    let out = b.form("/logout", "").await;
    assert_eq!(location(&out), "/login");
    assert!(b.cookie.is_none());
    assert_eq!(b.get("/").await.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn repeated_failures_lock_the_ip_out() {
    let mut b = Browser::new().await;
    auth::password::set(&b.app.db, "correct horse battery").await.unwrap();
    for _ in 0..5 {
        assert_eq!(b.form("/login/password", "password=nope").await.status(), StatusCode::UNAUTHORIZED);
    }
    let locked = b.form("/login/password", "password=correct+horse+battery").await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(text(locked).await.contains("Try again in 1 minutes"));
    assert!(b.cookie.is_none());
}

#[tokio::test]
async fn setup_links_sign_in_once() {
    let mut b = Browser::new().await;
    let link = auth::create_setup_link(&b.app.db, &b.app.config).await.unwrap();
    let page = b.get(&link).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(text(page).await.contains("url=/security?done=welcome"));
    assert!(text(b.get("/security?done=welcome").await).await.contains("setup link, which no longer works"));

    let mut other = Browser { cookie: None, ..b };
    assert_eq!(other.get(&link).await.status(), StatusCode::NOT_FOUND);
    assert!(other.cookie.is_none());
}

#[tokio::test]
async fn every_page_renders() {
    let mut b = Browser::signed_in().await;
    for (path, heading) in [
        ("/", "Overview"),
        ("/facebook", "Facebook"),
        ("/previews", "Previews"),
        ("/videos", "Videos"),
        ("/limits", "Rate limits"),
        ("/telegram", "Telegram IP ranges"),
        ("/playground?url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1", "Playground"),
        ("/failures?page_type=reel&stage=", "Recent failures"),
        ("/cache", "Cache"),
        ("/tests", "Test links and alerts"),
        ("/security", "Login settings"),
        ("/backup", "Import and export"),
        ("/?range=7d", "Overview"),
        ("/?range=nonsense", "Overview"),
    ] {
        let response = b.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let page = text(response).await;
        assert!(page.contains(&format!("<h1>{heading}</h1>")), "{path}");
        assert!(page.contains("hx-boost=\"true\"") && page.contains(&asset("htmx.min.js")), "{path}: htmx is on");
        assert!(!page.contains(" style=") && !page.contains("<style") && !page.contains("<script>"), "{path}: CSP");
    }
    let series: serde_json::Value = serde_json::from_str(&text(b.get("/api/series?hours=24").await).await).unwrap();
    assert_eq!(series, json!({ "timestamps": [], "series": {} }));
    for (file, kind) in
        [("htmx.min.js", "text/javascript"), ("icons.svg", "image/svg+xml"), ("favicon.svg", "image/svg+xml")]
    {
        let response = b.get(&asset(file)).await;
        assert_eq!(response.headers()[CONTENT_TYPE], kind);
        assert!(response.headers()[CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    }
}

#[tokio::test]
async fn htmx_gets_partials_and_a_login_redirect() {
    let mut b = Browser::signed_in().await;
    let took = std::time::Duration::from_millis(5);
    b.app.stats.record(crate::stats::Outcome::Hit, Some(crate::extract::facebook::url::PageType::Reel), took);
    b.app.stats.record(crate::stats::Outcome::Redirect, Some(crate::extract::facebook::url::PageType::Reel), took);

    let chart = text(b.get("/overview/chart?range=1h").await).await;
    assert!(chart.starts_with("<div class=\"chart\"") && !chart.contains("<html"), "only the chart: {chart}");
    assert!(chart.contains("hx-get=\"/overview/chart?range=1h\"") && chart.contains("class=\"s-hit\""));
    let overview = text(b.get("/").await).await;
    assert!(overview.contains("class=\"spark\"") && overview.contains("2 in all"), "sparklines and the chart");

    let csv = b.get("/export/requests.csv?range=24h").await;
    assert!(csv.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("requests-24h.csv"));
    let csv = text(csv).await;
    assert!(
        csv.starts_with("start_utc,outcome,requests\n") && csv.contains(",hit,1\n") && csv.contains(",redirect,1\n")
    );

    // Signed out, htmx is told to load the login page rather than swap it into the page.
    b.cookie = None;
    let mut request = Request::get("/cache").header(header::HOST, HOST).header("hx-request", "true");
    request = request.extension(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4000))));
    let response = b.router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()["hx-redirect"], "/login");
}

#[tokio::test]
async fn the_cache_lists_searches_and_forgets() {
    let mut b = Browser::signed_in().await;
    let keep = std::time::Duration::from_secs(300);
    let reel = Target::from_request("/reel/1").unwrap();
    let mut post = FakeExtractor::post(&reel);
    post.title = Some("Night market".into());
    b.app.cache.insert(64, reel.key().to_string(), Entry::post(post, keep)).await;
    b.app
        .cache
        .insert(64, "walled".into(), Entry::failure(ExtractError::new(Stage::LoginWall, "login page"), keep))
        .await;

    let page = text(b.get("/cache").await).await;
    assert!(page.contains("Night market") && page.contains("2 photos") && page.contains("login_wall"), "{page}");
    assert!(page.contains("1 posts, 1 failures"));
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
    settings["site_name"] = json!("Imported");
    settings["cache_minutes"] = json!(15);
    let body = format!(
        "settings={}",
        url::form_urlencoded::byte_serialize(settings.to_string().as_bytes()).collect::<String>()
    );
    assert_eq!(location(&b.form("/backup/import", &body).await), "/backup?done=imported");
    let s = b.app.settings.get();
    assert_eq!((s.site_name.as_str(), s.cache_minutes), ("Imported", 15));

    // Missing settings get their defaults; wrong ones change nothing.
    assert_eq!(location(&b.form("/backup/import", "settings=%7B%7D").await), "/backup?done=imported");
    assert_eq!(b.app.settings.get().site_name, crate::settings::Settings::default().site_name);
    let wrong = b.form("/backup/import", "settings=%7B%22facebook_concurrency%22%3A0%7D").await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert!(text(wrong).await.contains("Facebook concurrency must be at least 1"));
    let junk = b.form("/backup/import", "settings=nope").await;
    assert_eq!(junk.status(), StatusCode::BAD_REQUEST);
    assert!(text(junk).await.contains("a settings file from this dashboard"));
    assert_eq!(b.app.settings.get().facebook_concurrency, crate::settings::Settings::default().facebook_concurrency);
}

#[tokio::test]
async fn series_line_up_with_timestamps() {
    let mut b = Browser::signed_in().await;
    let took = std::time::Duration::from_millis(5);
    b.app.stats.record(crate::stats::Outcome::Hit, Some(crate::extract::facebook::url::PageType::Reel), took);
    b.app.stats.record(
        crate::stats::Outcome::Failed(Stage::Timeout),
        Some(crate::extract::facebook::url::PageType::Reel),
        took,
    );
    let series: serde_json::Value = serde_json::from_str(&text(b.get("/api/series?hours=1").await).await).unwrap();
    let points = series["timestamps"].as_array().unwrap().len();
    assert!(points >= 59, "a point a minute for an hour");
    for name in ["hit", "failed"] {
        let values = series["series"][name].as_array().unwrap();
        assert_eq!(values.len(), points);
        assert_eq!(values.iter().filter_map(serde_json::Value::as_u64).sum::<u64>(), 1);
    }
}

#[tokio::test]
async fn settings_save_and_mistakes_are_refused() {
    let mut b = Browser::signed_in().await;
    let saved =
        b.form("/limits", "ip_requests_per_minute=30&facebook_fetches_per_minute=20&facebook_concurrency=2").await;
    assert_eq!(location(&saved), "/limits?done=saved");
    assert_eq!(b.app.settings.get().ip_requests_per_minute, 30);

    let refused =
        b.form("/limits", "ip_requests_per_minute=31&facebook_fetches_per_minute=20&facebook_concurrency=0").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let page = text(refused).await;
    assert!(page.contains("Facebook concurrency must be at least 1"));
    assert!(page.contains("value=\"31\""), "what was entered is kept");
    assert_eq!(b.app.settings.get().ip_requests_per_minute, 30);

    let facebook = "proxy=&cookie_policy=never&extract_timeout_seconds=6";
    assert_eq!(location(&b.form("/facebook", facebook).await), "/facebook?done=saved");
    assert_eq!(
        b.form("/facebook", &facebook.replace("proxy=", "proxy=127.0.0.1:1")).await.status(),
        StatusCode::BAD_REQUEST
    );
    let cache = "cache_minutes=30&failure_cache_minutes=2&cache_megabytes=32";
    assert_eq!(location(&b.form("/cache", cache).await), "/cache?done=saved");
    let previews = "site_name=FB&description_length=200&failure_response=page&media_links=redirect&iv_photo=on&iv_album=on&iv_counts=on";
    assert_eq!(location(&b.form("/previews", previews).await), "/previews?done=saved");
    assert_eq!(b.form("/previews", &previews.replace("200", "5")).await.status(), StatusCode::BAD_REQUEST);
    let s = b.app.settings.get();
    assert_eq!(
        (s.cookie_policy, s.failure_response, s.media_links),
        (CookiePolicy::Never, FailureResponse::Page, MediaLinks::Redirect)
    );
    assert_eq!(
        (s.extract_timeout_seconds, s.cache_minutes, s.cache_megabytes, s.site_name.as_str()),
        (6, 30, 32, "FB")
    );
    assert!(s.instant_view.photo && s.instant_view.album && !s.instant_view.video && !s.instant_view.text);
    assert!(s.instant_view_counts && !s.author_pictures, "switches left off are off");

    let videos = "video_quality=standard&video_megabytes=15&video_space_megabytes=500";
    assert_eq!(location(&b.form("/videos", videos).await), "/videos?done=saved");
    let s = b.app.settings.get();
    assert_eq!((s.video_quality, s.video_megabytes, s.video_space_megabytes), (VideoQuality::Standard, 15, 500));
    let refused = b.form("/videos", &videos.replace("=standard", "=best")).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(text(refused).await.contains("none is set up"), "1080p needs a bucket");
    assert_eq!(b.form("/videos", &videos.replace("=500", "=10")).await.status(), StatusCode::BAD_REQUEST);

    assert_eq!(
        location(&b.form("/tests", "links=https%3A%2F%2Fwww.facebook.com%2Freel%2F1&alert_chat=-100123").await),
        "/tests?done=saved"
    );
    assert_eq!(b.app.settings.get().alert_chat, Some(-100_123));
    assert_eq!(
        b.form("/tests", "links=https%3A%2F%2Fexample.com%2F&alert_chat=").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(b.form("/tests", "links=&alert_chat=abc").await.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn videos_page_shows_how_the_bucket_does() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, config) = FakeS3::start().await;
    let extractor = Arc::new(FakeExtractor::default());
    let app = crate::app::tests::app_with_data(extractor, dir.path(), Some(config)).await;
    let mut b = Browser::with(app).sign_in().await;
    let form = "video_quality=best&video_megabytes=20&video_space_megabytes=1024";
    assert_eq!(location(&b.form("/videos", form).await), "/videos?done=saved");
    assert_eq!(b.app.settings.get().video_quality, VideoQuality::Best);
    assert_eq!(fake.lists(), 0, "saving asks nothing of the bucket");

    let page = text(b.get("/videos").await).await;
    assert!(page.contains("<code>videos</code>") && page.contains("Works"), "the first look lists it");
    assert!(page.contains("0 in the bucket"));
    assert!(page.contains("DNS only"), "not R2: the note about Cloudflare's proxy");

    // A video the bucket refuses: 1080p pauses, and the dashboard says why.
    *fake.refuse.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let addr = crate::video::tests::cdn().await;
    let name = crate::video::Videos::file_name("0123456789abcdef", 0).unwrap();
    let hd = crate::video::tests::hd(addr, "video.mp4");
    b.app.videos.get(&name, &hd, "post", &b.app.settings.get()).await.unwrap_err();
    let page = text(b.get("/videos").await).await;
    assert!(page.contains("Failing") && page.contains("Telegram gets 720p"), "{page}");
    assert!(text(b.get("/").await).await.contains("the bucket failed"));
    *fake.refuse.lock().unwrap() = None;
    assert_eq!(location(&b.form("/videos/delete", "").await), "/videos?done=videos_deleted");
    assert_eq!(fake.lists(), 1, "listed once all along");
}

#[tokio::test]
async fn cookies_are_saved_from_a_browser_export() {
    let mut b = Browser::signed_in().await;
    let (keep, wall) = (std::time::Duration::from_secs(300), ExtractError::new(Stage::LoginWall, "login page"));
    let post = FakeExtractor::post(&Target::from_request("/reel/1").unwrap());
    b.app.cache.insert(64, "walled".into(), Entry::failure(wall, keep)).await;
    b.app.cache.insert(64, "fine".into(), Entry::post(post, keep)).await;
    let export = r#"[{"name":"c_user","value":"1","domain":".facebook.com"},{"name":"xs","value":"2","domain":".facebook.com"}]"#;
    let body = format!("cookies={}", url::form_urlencoded::byte_serialize(export.as_bytes()).collect::<String>());
    assert_eq!(location(&b.form("/facebook/cookies", &body).await), "/facebook?done=cookies");
    assert!(b.app.cookies.status().has_session);
    assert!(b.app.cache.get("walled").await.is_none(), "cookies may get past the login wall now");
    assert!(b.app.cache.get("fine").await.is_some());
    assert_eq!(b.form("/facebook/cookies", "cookies=nope").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(location(&b.form("/facebook/cookies/clear", "").await), "/facebook?done=cookies_removed");
    assert_eq!(b.app.cookies.status().count, 0);
}

#[tokio::test]
async fn playground_extracts_then_reads_the_cache() {
    let mut b = Browser::signed_in().await;
    let link = "url=https%3A%2F%2Fwww.facebook.com%2Freel%2F1%3Fs%3Dx";
    let fresh = text(b.form("/playground", &format!("{link}&fresh=on")).await).await;
    assert!(fresh.contains("reel · extracted"), "{fresh}");
    assert!(fresh.contains("Telegram preview") && fresh.contains("srcdoc=\""));
    assert!(fresh.contains("og:title"));
    let cached = text(b.form("/playground", link).await).await;
    assert!(cached.contains("reel · from the cache"));
    let refused = b.form("/playground", "url=https%3A%2F%2Fexample.com%2F").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_last_way_to_sign_in_stays() {
    let mut b = Browser::signed_in().await;
    assert_eq!(b.form("/security/password", "password=short").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        location(&b.form("/security/password", "password=correct+horse+battery").await),
        "/security?done=password"
    );
    let refused = b.form("/security/password/remove", "").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(text(refused).await.contains("the last way to sign in"));
    assert!(auth::password::is_set(&b.app.db).await.unwrap());

    let security = text(b.get("/security").await).await;
    assert!(security.contains("setup link (this one)"));
    assert_eq!(location(&b.form("/security/sessions/clear", "").await), "/login");
    assert!(b.cookie.is_none());
    assert!(auth::sessions(&b.app.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn authenticator_app_codes_after_the_password() {
    let mut b = Browser::signed_in().await;
    assert_eq!(
        location(&b.form("/security/password", "password=correct+horse+battery").await),
        "/security?done=password"
    );
    assert_eq!(location(&b.form("/security/totp/start", "").await), "/security#totp");
    let page = text(b.get("/security").await).await;
    assert!(page.contains("data:image/svg+xml;base64,"));
    let key = page.split("data-totp-key>").nth(1).unwrap().split("</code>").next().unwrap().replace(' ', "");
    let secret = data_encoding::BASE32_NOPAD.decode(key.as_bytes()).unwrap();

    let wrong = b.form("/security/totp/confirm", "code=000000").await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert!(text(wrong).await.contains("Use the newest one"));
    let code = auth::totp::code_now(&secret, 0);
    assert_eq!(location(&b.form("/security/totp/confirm", &format!("code={code}")).await), "/security?done=totp");
    assert!(text(b.get("/security").await).await.contains("On since"));

    b.form("/logout", "").await;
    assert!(text(b.get("/login").await).await.contains("name=\"code\""));
    let no_code = b.form("/login/password", "password=correct+horse+battery").await;
    assert_eq!(no_code.status(), StatusCode::UNAUTHORIZED);
    assert!(text(no_code).await.contains("Wrong password or code"));
    let replayed = b.form("/login/password", &format!("password=correct+horse+battery&code={code}")).await;
    assert_eq!(replayed.status(), StatusCode::UNAUTHORIZED, "the setup code is used up");
    let next = auth::totp::code_now(&secret, 1);
    let ok = b.form("/login/password", &format!("password=correct+horse+battery&code={next}")).await;
    assert_eq!(location(&ok), "/");
    assert!(text(b.get("/security").await).await.contains("password + code (this one)"));

    assert_eq!(location(&b.form("/security/totp/remove", "").await), "/security?done=totp_removed");
    assert!(auth::totp::enabled_at(&b.app.db).await.unwrap().is_none());
}
