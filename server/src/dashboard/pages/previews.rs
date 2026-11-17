//! The dashboard's previews page.

use super::*;

#[derive(Template)]
#[template(path = "dashboard/previews.html")]
pub struct PreviewsPage {
    shell: Shell,
    s: Settings,
    failure: &'static str,
    media_links: &'static str,
}

const fn previews_page(shell: Shell, s: Settings) -> PreviewsPage {
    PreviewsPage {
        failure: match s.failure_response {
            FailureResponse::Redirect => "redirect",
            FailureResponse::Page => "page",
            FailureResponse::Unavailable => "unavailable",
        },
        media_links: match s.media_links {
            MediaLinks::Direct => "direct",
            MediaLinks::Redirect => "redirect",
        },
        shell,
        s,
    }
}

pub async fn previews(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&previews_page(Shell::new("previews", &q), (*d.app.settings.get()).clone()))
}

#[derive(Deserialize)]
pub struct PreviewsForm {
    description_length: usize,
    failure_response: FailureResponse,
    media_links: MediaLinks,
    iv_video: Option<String>,
    iv_photo: Option<String>,
    iv_album: Option<String>,
    iv_text: Option<String>,
    iv_profile: Option<String>,
    iv_counts: Option<String>,
    photo_mosaic: Option<String>,
    discord: Option<String>,
}

pub async fn save_previews(State(d): State<Shared>, Form(f): Form<PreviewsForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.description_length = f.description_length;
    s.failure_response = f.failure_response;
    s.media_links = f.media_links;
    s.instant_view.video = f.iv_video.is_some();
    s.instant_view.photo = f.iv_photo.is_some();
    s.instant_view.album = f.iv_album.is_some();
    s.instant_view.text = f.iv_text.is_some();
    s.instant_view.profile = f.iv_profile.is_some();
    s.instant_view_counts = f.iv_counts.is_some();
    s.photo_mosaic = f.photo_mosaic.is_some();
    s.discord = f.discord.is_some();
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/previews", "saved"),
        Err(e) => refused(&previews_page(Shell::refused("previews", format!("{e:#}")), s)),
    }
}
