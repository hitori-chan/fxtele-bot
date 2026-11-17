//! The dashboard's videos page.

use super::*;

struct BucketView {
    api: String,
    name: String,
    region: String,
    /// An address Telegram can't reach, so it can't download the videos.
    private: bool,
    /// Not R2, so possibly served through Cloudflare's proxy.
    self_hosted: bool,
}

/// The bucket's state, from the requests videos made of it.
struct HealthView {
    /// `Some(true)` works, `Some(false)` failing, `None` not used yet.
    ok: Option<bool>,
    detail: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/videos.html")]
pub struct VideosPage {
    shell: Shell,
    s: Settings,
    best: bool,
    bucket: Option<BucketView>,
    health: HealthView,
    /// `None` when the bucket can't be listed.
    stored: Option<Usage>,
    stored_size: String,
    /// The space setting, for the meter.
    space_bytes: u64,
    failures: Vec<(String, crate::video::Failure)>,
}

async fn videos_page(app: &App, shell: Shell, s: Settings) -> VideosPage {
    let videos = &app.videos;
    let bucket = videos.bucket().map(|b| BucketView {
        api: b.api.as_str().trim_end_matches('/').to_string(),
        name: b.bucket.clone(),
        region: b.region.clone(),
        private: b.is_private(),
        self_hosted: !b.api.host_str().is_some_and(|h| h.ends_with(".r2.cloudflarestorage.com")),
    });
    let stored = videos.usage().await;
    // After the usage above, whose first look at the bucket lists it.
    let health = match videos.bucket_health() {
        BucketHealth { failing: Some(f), .. } => HealthView {
            ok: Some(false),
            detail: format!("since {}", ago(f.since)),
            error: Some(format!(
                "{} Meanwhile Telegram gets 720p; the first look after {} (a video, a preview, this page) tries the bucket again.",
                f.error,
                when(f.retry_at())
            )),
        },
        BucketHealth { worked_at: Some(at), .. } => {
            HealthView { ok: Some(true), detail: format!("last request {}", ago(at)), error: None }
        }
        BucketHealth { .. } => HealthView { ok: None, detail: "not used yet".into(), error: None },
    };
    VideosPage {
        best: s.video_quality == VideoQuality::Best,
        health,
        failures: videos.recent_failures().into_iter().map(|(at, f)| (ago(at), f)).collect(),
        bucket,
        stored_size: stored.map(|u| size(u.bytes)).unwrap_or_default(),
        stored,
        space_bytes: s.video_space_megabytes.saturating_mul(1024 * 1024),
        shell,
        s,
    }
}

pub async fn videos(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&videos_page(&d.app, Shell::new("videos", &q), (*d.app.settings.get()).clone()).await)
}

#[derive(Deserialize)]
pub struct VideosForm {
    video_quality: VideoQuality,
    video_megabytes: u64,
    video_space_megabytes: u64,
    video_days: u64,
}

pub async fn save_videos(State(d): State<Shared>, Form(f): Form<VideosForm>) -> Response {
    let app = &d.app;
    let apply = |s: &mut Settings| {
        s.video_quality = f.video_quality;
        s.video_megabytes = f.video_megabytes;
        s.video_space_megabytes = f.video_space_megabytes;
        s.video_days = f.video_days;
    };
    let saved = if f.video_quality == VideoQuality::Best && app.videos.bucket().is_none() {
        Err(anyhow::anyhow!(
            "1080p videos need a bucket, and none is set up: set VIDEO_BUCKET and the other VIDEO_BUCKET_ variables in .env (docs/deployment.md), then restart the server."
        ))
    } else {
        app.settings.edit(|s| apply(s)).await
    };
    match saved {
        Ok(()) => done("/videos", "saved"),
        Err(e) => {
            let mut entered = (*app.settings.get()).clone();
            apply(&mut entered);
            refused(&videos_page(app, Shell::refused("videos", format!("{e:#}")), entered).await)
        }
    }
}

pub async fn delete_videos(State(d): State<Shared>) -> Response {
    match d.app.videos.delete_all().await {
        Ok(()) => done("/videos", "videos_deleted"),
        Err(e) => refused(
            &videos_page(
                &d.app,
                Shell::refused("videos", format!("Couldn't delete the bucket's videos: {e:#}")),
                (*d.app.settings.get()).clone(),
            )
            .await,
        ),
    }
}
