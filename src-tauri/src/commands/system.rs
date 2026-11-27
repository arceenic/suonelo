use super::Managed;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequired {
    requires_confirmation: bool,
}

#[tauri::command]
pub fn request_close(app: Managed<'_>) -> Result<ConfirmationRequired> {
    Ok(ConfirmationRequired {
        requires_confirmation: app.state().queue.pending(),
    })
}

#[tauri::command]
pub async fn shutdown_session(app: Managed<'_>, handle: AppHandle) -> Result<()> {
    crate::diagnostics::preload::mark(
        "shutdown_started",
        serde_json::json!({"path": "normal-session-shutdown"}),
    );
    app.shutdown().await;
    handle.exit(0);
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AboutLink {
    Project,
    QobuzPrivacy,
}

#[tauri::command]
pub async fn open_about_link(link: AboutLink) -> Result<()> {
    let url = match link {
        AboutLink::Project => "https://github.com/HongYue1/qobuz-cli",
        AboutLink::QobuzPrivacy => "https://www.qobuz.com/us-en/discover/legals/privacy",
    };
    crate::platform::shell::open_url(url).await
}

#[tauri::command]
pub fn preload_frontend_ready(handle: AppHandle) {
    crate::diagnostics::preload::frontend_ready(&handle);
}
