//! The public listener: previews for Telegram, redirects for everyone else.

mod links;
mod media;
mod preview;
mod video;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

use crate::app::App;
use crate::client_ip::user_agent;
use crate::stats::Outcome;

/// Who's asking, worked out once per request by the middleware.
#[derive(Debug, Clone, Copy)]
pub struct Visitor {
    /// Telegram's user agent and an address inside Telegram's ranges.
    pub is_telegram: bool,
    /// An address inside Telegram's ranges, whatever the user agent.
    pub in_telegram_ranges: bool,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/robots.txt", get(robots))
        .route("/_/health", get(health))
        .route("/_/m/{id}/{item}", get(media::redirect))
        .route("/_/v/{id}/{file}", get(video::serve))
        .route("/_/l/{signature}", get(links::go))
        .route("/_/{*rest}", any(|| async { not_found() }))
        .route("/", any(preview::handle))
        .route("/{*path}", any(preview::handle))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), visitor))
        // Videos are compressed already, and compressing them would break range requests.
        .layer(
            CompressionLayer::new().compress_when(DefaultPredicate::new().and(NotForContentType::const_new("video/"))),
        )
        .layer(SetResponseHeaderLayer::overriding(CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, Duration::from_secs(30)))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(CatchPanicLayer::new())
        .with_state(app)
}

/// Resolves the client IP, recognizes Telegram, and applies the per-IP limit (Telegram exempt).
async fn visitor(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = app.resolver.resolve(peer.ip(), request.headers());
    let in_ranges = app.telegram_ips.contains(ip.ip);
    let claims_telegram = user_agent(request.headers()).contains("TelegramBot");
    if claims_telegram && !in_ranges {
        app.stats.suspect(ip.ip, user_agent(request.headers()), ip.is_proxy);
    }
    if !in_ranges && !app.ip_limiter.allow(ip.ip, app.settings.get().ip_requests_per_minute) {
        app.stats.record(Outcome::RateLimited, None, Duration::ZERO);
        return (StatusCode::TOO_MANY_REQUESTS, "Too many requests\n").into_response();
    }
    request
        .extensions_mut()
        .insert(Visitor { is_telegram: claims_telegram && in_ranges, in_telegram_ranges: in_ranges });
    next.run(request).await
}

async fn robots() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain")], "User-agent: TelegramBot\nAllow: /\n\nUser-agent: *\nDisallow: /\n")
}

async fn health() -> &'static str {
    "ok\n"
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found\n").into_response()
}

fn redirect(location: &str) -> Response {
    match HeaderValue::from_str(location) {
        Ok(value) => (StatusCode::FOUND, [(LOCATION, value)]).into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "Bad link\n").into_response(),
    }
}

fn html(body: String) -> Response {
    ([(CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::USER_AGENT;
    use tower::ServiceExt;

    use super::*;
    use crate::app::tests::{app_with, app_with_data};
    use crate::extract::fake::FakeExtractor;
    use crate::extract::{ExtractError, Media, Stage};
    use crate::settings::{FailureResponse, MediaLinks, VideoQuality};
    use crate::video::bucket::tests::FakeS3;
    use crate::video::tests::{cdn, hd};

    const TELEGRAM_UA: &str = "TelegramBot (like TwitterBot)";
    const TELEGRAM_IP: &str = "149.154.161.10:4000";

    async fn get(app: &Arc<App>, path: &str, ua: &str, peer: &str) -> Response {
        let mut request = Request::get(path).header(USER_AGENT, ua).header("host", "fb.example.com");
        request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        router(Arc::clone(app)).oneshot(request.body(Body::empty()).unwrap()).await.unwrap()
    }

    async fn body(response: Response) -> String {
        String::from_utf8(axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn people_are_redirected_without_fetching() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        for (ua, peer) in [("Mozilla/5.0", TELEGRAM_IP), (TELEGRAM_UA, "1.2.3.4:5"), ("Discordbot", "1.2.3.4:5")] {
            let response = get(&app, "/reel/1?s=x", ua, peer).await;
            assert_eq!(response.status(), StatusCode::FOUND, "{ua}");
            assert_eq!(response.headers()[LOCATION], "https://www.facebook.com/reel/1?s=x");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        }
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(app.stats.suspects().len(), 1, "the spoofed Telegram request is noted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn telegram_gets_the_preview() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let response = get(&app, "/somepage/posts/1", TELEGRAM_UA, TELEGRAM_IP).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let html = body(response).await;
        assert!(html.contains("og:image\" content=\"https://scontent.xx.fbcdn.net/v/a.jpg?oe=7FFFFFFF"));
        assert!(html.contains("al:android:app_name"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failure_responses() {
        let extractor = Arc::new(FakeExtractor::default());
        *extractor.fail.lock().unwrap() = Some(ExtractError::new(Stage::NotFound, "nothing"));
        let app = app_with(extractor).await;
        let response = get(&app, "/reel/9", TELEGRAM_UA, TELEGRAM_IP).await;
        assert_eq!(response.status(), StatusCode::OK, "a card by default");
        let card = body(response).await;
        assert!(card.contains("Couldn&#39;t load this post") || card.contains("Couldn't load this post"), "{card}");
        assert!(card.contains("it may be private, deleted or for friends only"), "says why");

        for (choice, status) in [
            (FailureResponse::Redirect, StatusCode::FOUND),
            (FailureResponse::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let mut settings = (*app.settings.get()).clone();
            settings.failure_response = choice;
            app.settings.set(settings).await.unwrap();
            assert_eq!(get(&app, "/reel/9", TELEGRAM_UA, TELEGRAM_IP).await.status(), status);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn internal_paths_and_robots() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        assert_eq!(get(&app, "/_/health", "x", "1.2.3.4:5").await.status(), StatusCode::OK);
        assert_eq!(get(&app, "/_/whatever", "x", "1.2.3.4:5").await.status(), StatusCode::NOT_FOUND);
        assert!(
            body(get(&app, "/robots.txt", "x", "1.2.3.4:5").await).await.contains("User-agent: TelegramBot\nAllow: /")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webpage_bot_purges_the_cached_post() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        let purge = |peer: &'static str| {
            let app = Arc::clone(&app);
            async move {
                let request = Request::builder()
                    .method("PURGE")
                    .uri("/reel/1")
                    .header(USER_AGENT, TELEGRAM_UA)
                    .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap();
                router(app).oneshot(request).await.unwrap().status()
            }
        };
        get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await;
        get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await;
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 1, "cached");
        assert_eq!(purge("1.2.3.4:5").await, StatusCode::METHOD_NOT_ALLOWED, "only Telegram may purge");
        assert_eq!(purge(TELEGRAM_IP).await, StatusCode::OK);
        get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await;
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 2, "fetched again after the purge");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn text_links_go_through_the_server_but_not_for_telegram() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let html = body(get(&app, "/somepage/posts/1", TELEGRAM_UA, TELEGRAM_IP).await).await;
        let start = html.find("https://fb.example.com/_/l/").expect("a wrapped link in the text");
        let link = html[start..html[start..].find('"').unwrap() + start].replace("&amp;", "&");
        assert!(link.ends_with("?u=https%3A%2F%2Fexample.com%2Fx"), "{link}");
        let path = link.trim_start_matches("https://fb.example.com");

        // Another site: a page naming it, not a redirect anyone could use.
        let person = get(&app, path, "Mozilla/5.0", "1.2.3.4:5").await;
        assert_eq!(person.status(), StatusCode::OK);
        assert!(person.headers()["content-security-policy"].to_str().unwrap().starts_with("default-src 'none'"));
        let page = body(person).await;
        assert!(page.contains("Continue to example.com") && page.contains("href=\"https://example.com/x\""), "{page}");
        assert_eq!(get(&app, path, TELEGRAM_UA, TELEGRAM_IP).await.status(), StatusCode::FORBIDDEN);
        // Facebook's own, like hashtags: straight there.
        let hashtag = "https://www.facebook.com/hashtag/tag";
        let encoded: String = url::form_urlencoded::byte_serialize(hashtag.as_bytes()).collect();
        let signed = format!("/_/l/{}?u={encoded}", app.cache.link_signature(hashtag));
        let person = get(&app, &signed, "Mozilla/5.0", "1.2.3.4:5").await;
        assert_eq!(
            (person.status(), &person.headers()[LOCATION]),
            (StatusCode::FOUND, &HeaderValue::from_static(hashtag))
        );
        let forged = path.replace("example.com%2Fx", "evil.example%2F");
        assert_eq!(get(&app, &forged, "Mozilla/5.0", "1.2.3.4:5").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn site_files_cost_no_fetch() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        for path in ["/favicon.ico", "/apple-touch-icon-precomposed.png", "/.well-known/security.txt"] {
            assert_eq!(get(&app, path, TELEGRAM_UA, TELEGRAM_IP).await.status(), StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(get(&app, "/photo.php?fbid=1", TELEGRAM_UA, TELEGRAM_IP).await.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn per_ip_limit_spares_telegram() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let mut settings = (*app.settings.get()).clone();
        settings.ip_requests_per_minute = 2;
        app.settings.set(settings).await.unwrap();
        let statuses: Vec<StatusCode> =
            futures_util::future::join_all((0..3).map(|_| get(&app, "/reel/1", "Mozilla/5.0", "1.2.3.4:5")))
                .await
                .iter()
                .map(Response::status)
                .collect();
        assert_eq!(statuses.iter().filter(|s| **s == StatusCode::TOO_MANY_REQUESTS).count(), 1);
        for _ in 0..3 {
            assert_eq!(get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await.status(), StatusCode::OK);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn media_links_redirect_only_to_extracted_media() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let mut settings = (*app.settings.get()).clone();
        settings.media_links = MediaLinks::Redirect;
        app.settings.set(settings).await.unwrap();

        let html = body(get(&app, "/somepage/posts/1", TELEGRAM_UA, TELEGRAM_IP).await).await;
        let start = html.find("https://fb.example.com/_/m/").expect("media link in the page");
        let link = &html[start..html[start..].find('"').unwrap() + start];
        let path = link.trim_start_matches("https://fb.example.com");

        let response = get(&app, path, "Mozilla/5.0", "1.2.3.4:5").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(response.headers()[LOCATION].to_str().unwrap().starts_with("https://scontent.xx.fbcdn.net/"));

        for bad in
            ["/_/m/0000000000000000/0", "/_/m/https%3A%2F%2Fevil.example/0", &format!("{}9", &path[..path.len() - 1])]
        {
            assert_eq!(get(&app, bad, "Mozilla/5.0", "1.2.3.4:5").await.status(), StatusCode::NOT_FOUND, "{bad}");
        }
    }

    async fn get_with(app: &Arc<App>, path: &str, peer: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::get(path).header(USER_AGENT, "TelegramBot (like TwitterBot)");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap())).body(Body::empty()).unwrap();
        router(Arc::clone(app)).oneshot(request).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn joined_videos_when_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let addr = cdn().await;
        let (fake, config) = FakeS3::start().await;
        let extractor = Arc::new(FakeExtractor::default());
        let video = |stream: &str| {
            let mut media = Media::video(format!("http://{addr}/progressive.mp4"), None);
            media.hd = Some(hd(addr, stream));
            media
        };
        *extractor.media.lock().unwrap() = Some(vec![video("video.mp4"), video("denied.mp4")]);
        let app = app_with_data(Arc::clone(&extractor), dir.path(), Some(config.clone())).await;

        // Off by default: the video still comes through this host, as Telegram plays only those in
        // the card, and is sent on to Facebook's file.
        let html = body(get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await).await;
        assert!(html.contains("og:video\" content=\"https://fb.example.com/_/v/"), "{html}");
        assert!(!html.contains(&format!("http://{addr}/progressive.mp4")), "never Facebook's CDN in the page");
        assert!(!html.contains("og:video:width\" content=\"96"), "720p keeps the file's own size");
        let start = html.find("/_/v/").unwrap();
        let path = &html[start..html[start..].find('"').unwrap() + start];
        let response = get_with(&app, path, TELEGRAM_IP, &[]).await;
        assert_eq!(response.headers()[LOCATION], format!("http://{addr}/progressive.mp4").as_str());
        assert!(fake.files.lock().unwrap().is_empty(), "nothing joined at 720p");

        let mut settings = (*app.settings.get()).clone();
        settings.video_quality = VideoQuality::Best;
        app.settings.set(settings).await.unwrap();
        app.cache.clear();
        let html = body(get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await).await;
        let start = html.find("https://fb.example.com/_/v/").expect("a joined video link");
        let link = &html[start..html[start..].find('"').unwrap() + start];
        let path = link.trim_start_matches("https://fb.example.com");
        assert!(path.ends_with("/0.mp4"), "{path}");
        assert!(html.contains("og:video:width\" content=\"96"), "the 1080p stream's size");

        // Telegram is sent to the bucket, which has the file; the server never sends it.
        let name = format!("facebook/{}", path.trim_start_matches("/_/v/").replace("/0.mp4", "-0.mp4"));
        let response = get_with(&app, path, TELEGRAM_IP, &[]).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        let link = response.headers()[LOCATION].to_str().unwrap().to_string();
        assert!(link.contains(&format!("/videos/{name}?X-Amz-")), "a signed link: {link}");
        let downloaded = reqwest::get(&link).await.unwrap().bytes().await.unwrap();
        assert_eq!(downloaded, fake.files.lock().unwrap()[&name].0, "Telegram downloads it with the link");
        assert_eq!(&downloaded[4..8], b"ftyp");
        let html = body(get(&app, "/reel/1", TELEGRAM_UA, TELEGRAM_IP).await).await;
        let page_link = format!("https://fb.example.com{path}");
        assert!(html.contains(&page_link) && !html.contains("X-Amz-"), "previews keep linking /_/v/");

        // Anyone else gets Facebook's file, even once the bucket has it: media IDs follow from a
        // post's link.
        let stranger = get(&app, path, "Mozilla/5.0", "1.2.3.4:5").await;
        assert_eq!(stranger.status(), StatusCode::FOUND);
        assert_eq!(stranger.headers()[LOCATION], format!("http://{addr}/progressive.mp4").as_str());

        // A video that can't be made falls back to Facebook's file.
        let broken = path.replace("/0.mp4", "/1.mp4");
        let response = get_with(&app, &broken, TELEGRAM_IP, &[]).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], format!("http://{addr}/progressive.mp4").as_str());
        assert_eq!(get(&app, "/_/v/0123456789abcdef/0.mp4", "x", "1.2.3.4:5").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&app, "/_/v/..%2F..%2Fetc/0.mp4", "x", "1.2.3.4:5").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn head_is_answered_like_get() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let request = Request::head("/reel/1")
            .header(USER_AGENT, "Mozilla/5.0")
            .extension(ConnectInfo("1.2.3.4:5".parse::<SocketAddr>().unwrap()))
            .body(Body::empty())
            .unwrap();
        let response = router(app).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
    }
}
