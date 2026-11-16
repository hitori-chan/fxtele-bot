//! The dashboard's import and export page.

use super::*;

#[derive(Template)]
#[template(path = "dashboard/backup.html")]
pub struct BackupPage {
    shell: Shell,
    /// What was pasted, when an import was refused.
    pasted: String,
}

pub async fn backup(Query(q): Query<Done>) -> Response {
    render(&BackupPage { shell: Shell::new("backup", &q), pasted: String::new() })
}

/// The settings as a JSON file.
pub async fn export_settings(State(d): State<Shared>) -> Response {
    match serde_json::to_string_pretty(&*d.app.settings.get()) {
        Ok(json) => (
            [(CONTENT_TYPE, "application/json")],
            [(CONTENT_DISPOSITION, "attachment; filename=\"settings.json\"")],
            json + "\n",
        )
            .into_response(),
        Err(e) => server_error(&e.into()),
    }
}

#[derive(Deserialize)]
pub struct ImportForm {
    settings: String,
}

/// Replaces the settings with a file's, checked like the settings pages check them.
pub async fn import_settings(State(d): State<Shared>, Form(f): Form<ImportForm>) -> Response {
    let saved = match serde_json::from_str(&f.settings).and_then(Settings::from_value) {
        Ok(settings) => d.app.settings.set(settings).await,
        Err(e) => Err(anyhow::anyhow!("That isn't a settings file from this dashboard ({e}). Nothing changed.")),
    };
    match saved {
        Ok(()) => done("/backup", "imported"),
        Err(e) => refused(&BackupPage { shell: Shell::refused("backup", format!("{e:#}")), pasted: f.settings }),
    }
}
