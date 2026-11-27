use super::{Managed, blocking};
use crate::{
    app::{App, AppState},
    error::{AppError, Result},
    media::{self, MetadataField},
    queue::QueueSnapshot,
    storage::{MaxConnections, Settings, SettingsDto, SettingsPatch},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[tauri::command]
pub fn read_settings(app: Managed<'_>) -> Result<SettingsDto> {
    Ok(app.state().settings.dto(&app.root))
}

#[tauri::command]
pub async fn patch_settings(
    app: Managed<'_>,
    expected_revision: u64,
    patch: SettingsPatch,
) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || save_settings_patch(&app, &mut app.state(), expected_revision, patch, false))
        .await
}

#[derive(Serialize)]
pub struct SavedConnectionLimit {
    settings: SettingsDto,
    queue: QueueSnapshot,
}

#[tauri::command]
pub async fn save_connection_limit(
    app: Managed<'_>,
    expected_revision: u64,
    limit: MaxConnections,
) -> Result<SavedConnectionLimit> {
    let app = app.inner().clone();
    blocking(move || {
        let mut patch = SettingsPatch::default();
        patch.max_connections = Some(limit);
        let mut state = app.state();
        let settings = save_settings_patch(&app, &mut state, expected_revision, patch, true)?;
        Ok(SavedConnectionLimit {
            settings,
            queue: state.queue.snapshot(),
        })
    })
    .await
}

pub(super) fn save_settings_patch(
    app: &App,
    state: &mut AppState,
    expected_revision: u64,
    patch: SettingsPatch,
    live: bool,
) -> Result<SettingsDto> {
    state.ensure_open()?;
    if state.settings.revision != expected_revision {
        return Err(AppError::new(
            "settings_conflict",
            "settings",
            "Settings changed. Reload and apply the edit again.",
            true,
        ));
    }
    let next = state.settings.apply(&app.root, patch)?;
    app.storage.save_settings(&next)?;
    state.settings = next;
    state
        .queue
        .settings_saved(&state.settings, live, app.qobuz.recovery());
    Ok(state.settings.dto(&app.root))
}

#[tauri::command]
pub async fn migrate_theme(
    app: Managed<'_>,
    expected_revision: u64,
    legacy_dark: bool,
) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if let Some(next) = state
            .settings
            .migrated_theme(expected_revision, legacy_dark)?
        {
            app.storage.save_settings(&next)?;
            state.settings = next;
        }
        Ok(state.settings.dto(&app.root))
    })
    .await
}

#[tauri::command]
pub async fn reset_settings(app: Managed<'_>, expected_revision: u64) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if state.settings.revision != expected_revision {
            return Err(AppError::new(
                "settings_conflict",
                "settings",
                "Settings changed. Reload before resetting.",
                true,
            ));
        }
        let next = Settings {
            revision: expected_revision + 1,
            ..Default::default()
        };
        app.storage.save_settings(&next)?;
        state
            .queue
            .settings_saved(&next, false, app.qobuz.recovery());
        state.settings = next;
        Ok(state.settings.dto(&app.root))
    })
    .await
}

#[tauri::command]
pub fn metadata_catalog(app: Managed<'_>) -> Result<Vec<MetadataField>> {
    Ok(app.media.metadata_catalog())
}

#[derive(Deserialize)]
pub struct TemplateInput {
    folder: String,
    filename: String,
}

#[derive(Serialize)]
pub struct Preview {
    path: PathBuf,
}

#[tauri::command]
pub fn preview_output(
    app: Managed<'_>,
    template: TemplateInput,
    quality: Option<u8>,
    safe_mode: Option<bool>,
) -> Result<Preview> {
    let (quality, safe_mode, destination) = {
        let state = app.state();
        (
            quality.unwrap_or(state.settings.output.quality),
            safe_mode.unwrap_or(state.settings.output.safe_mode),
            state.settings.destination(&app.root),
        )
    };
    if !(1..=4).contains(&quality) {
        return Err(AppError::invalid(
            "settings",
            "Choose a quality tier from 1 through 4.",
        ));
    }
    let path = media::preview_output(&template.folder, &template.filename, quality, safe_mode)
        .map_err(|error| AppError::media(error, "settings"))?;
    let path = match destination {
        Some(root) => root.join(path),
        None => path,
    };
    Ok(Preview { path })
}
