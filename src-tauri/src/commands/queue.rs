use super::{Managed, blocking};
use crate::{
    app::{App, BootstrapDto, Destination, SubmissionReport},
    error::{AppError, Result},
    queue::{ClearReport, QueueEvent, QueueSnapshot},
};
use serde::Serialize;
use std::path::PathBuf;
use tauri::ipc::Channel;

#[tauri::command]
pub async fn bootstrap(app: Managed<'_>) -> Result<BootstrapDto> {
    let state = app.state();
    Ok(BootstrapDto {
        settings: state.settings.dto(&app.root),
        queue: state.queue.snapshot(),
        bookmarks: state.bookmark_dtos(),
        account: state.account_status(),
    })
}

#[tauri::command]
pub fn queue_snapshot(app: Managed<'_>) -> Result<QueueSnapshot> {
    Ok(app.state().queue.snapshot())
}

#[tauri::command]
pub fn set_queue_paused(app: Managed<'_>, paused: bool) -> Result<QueueSnapshot> {
    let mut state = app.state();
    state.ensure_open()?;
    state.queue.set_user_pause(paused, app.qobuz.recovery());
    Ok(state.queue.snapshot())
}

#[tauri::command]
pub fn subscribe_queue(app: Managed<'_>, channel: Channel<QueueEvent>) -> Result<()> {
    app.state().queue.subscribe(channel);
    Ok(())
}

#[tauri::command]
pub async fn submit_links(
    app: Managed<'_>,
    text: String,
    destination: Destination,
    quality_override: Option<u8>,
) -> Result<SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || match quality_override {
        Some(quality) => app.submit_with_options(text, destination, Some(quality), false),
        None => app.submit(text, destination),
    })
    .await
}

#[tauri::command]
pub async fn inspect_link_file(path: PathBuf) -> Result<crate::link_import::ImportReport> {
    blocking(move || Ok(crate::link_import::inspect_link_file(&path))).await
}

#[tauri::command]
pub async fn submit_imported_links(
    app: Managed<'_>,
    text: String,
    destination: Destination,
    quality_override: Option<u8>,
) -> Result<SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || app.submit_with_options(text, destination, quality_override, true)).await
}

#[tauri::command]
pub async fn clear_downloads(app: Managed<'_>) -> Result<ClearReport> {
    let (report, removed) = {
        let mut state = app.state();
        state.ensure_open()?;
        let before = state
            .queue
            .jobs
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let report = state.queue.clear_downloads();
        state.queue.reset_idle_pause(app.qobuz.recovery());
        let removed = before
            .into_iter()
            .filter(|id| !state.queue.jobs.contains_key(id))
            .collect::<Vec<_>>();
        (report, removed)
    };
    app.media.discard_retained_jobs(&removed).await;
    Ok(report)
}

#[tauri::command]
pub fn retry_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    retry(app.inner(), job_id, None)
}

#[tauri::command]
pub fn retry_file(app: Managed<'_>, job_id: u64, file_id: u64) -> Result<()> {
    retry(app.inner(), job_id, Some(file_id))
}

#[tauri::command]
pub fn replace_search_audio(
    app: Managed<'_>,
    job_id: u64,
    file_id: u64,
    token: String,
) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    if state.account.is_none() {
        return Err(AppError::invalid(
            "account",
            "Validate the current account before replacing audio.",
        ));
    }
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state
        .queue
        .approve_replacement(job_id, file_id, &token, &settings, reason)
}

fn retry(app: &App, id: u64, extra: Option<u64>) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state.queue.retry(id, &settings, reason, extra)
}

#[tauri::command]
pub fn download_again(app: Managed<'_>, job_id: u64) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state.queue.download_again(job_id, &settings, reason)
}

#[tauri::command]
pub fn cancel_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    let mut state = app.state();
    state.queue.cancel(job_id, false)?;
    state.queue.reset_idle_pause(app.qobuz.recovery());
    Ok(())
}

#[tauri::command]
pub async fn remove_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    {
        let mut state = app.state();
        state.queue.cancel(job_id, true)?;
        state.queue.reset_idle_pause(app.qobuz.recovery());
    }
    app.media.discard_retained_jobs(&[job_id]).await;
    Ok(())
}

#[tauri::command]
pub fn apply_settings_to_pending(app: Managed<'_>, ids: Option<Vec<u64>>) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    state.queue.apply(ids, &settings);
    Ok(())
}

#[tauri::command]
pub fn decide_large_submission(
    app: Managed<'_>,
    id: u64,
    plan_revision: u64,
    decision: bool,
) -> Result<()> {
    app.state().queue.decide(id, plan_revision, decision)
}

#[derive(Serialize)]
pub struct ReleaseFolderStatus {
    available: bool,
    reason: Option<String>,
}

#[tauri::command]
pub async fn release_folder_status(
    app: Managed<'_>,
    job_id: u64,
) -> Result<ReleaseFolderStatus> {
    let app = app.inner().clone();
    blocking(move || {
        let evidence = app.state().queue.release_folder_evidence(job_id);
        let result = evidence.and_then(crate::release_folder::select);
        Ok(ReleaseFolderStatus {
            available: result.is_ok(),
            reason: result.err().map(|error| error.message),
        })
    })
    .await
}

#[tauri::command]
pub async fn open_release_folder(app: Managed<'_>, job_id: u64) -> Result<()> {
    let app = app.inner().clone();
    let resolved = blocking(move || {
        let evidence = app.state().queue.release_folder_evidence(job_id)?;
        let folder = crate::release_folder::select(evidence)?;
        // Another file may finish during verification. Only the chosen witness must still match.
        if !app
            .state()
            .queue
            .release_folder_evidence(job_id)?
            .contains(&folder.evidence)
        {
            return Err(AppError::invalid(
                "queue",
                "The release output changed. Check its folder again.",
            ));
        }
        #[cfg(target_os = "macos")]
        return Ok(folder.path);
        #[cfg(target_os = "linux")]
        return crate::platform::shell::open_folder_sync(
            &folder.path,
            "The saved folder is missing or inaccessible.",
            "The Linux desktop could not open the saved folder. Check that a file manager is available.",
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        crate::platform::shell::open_folder_sync(
            &folder.path,
            "The saved release folder is missing or inaccessible.",
            "Windows could not open the saved release folder.",
        )
    })
    .await;
    #[cfg(target_os = "macos")]
    return crate::platform::shell::open_folder(&resolved?).await;
    #[cfg(not(target_os = "macos"))]
    resolved
}

#[tauri::command]
pub async fn open_containing_folder(app: Managed<'_>, job_id: u64, file_id: u64) -> Result<()> {
    let app = app.inner().clone();
    let resolved = blocking(move || {
        let path = app.state().queue.conflict_path(job_id, file_id)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let unavailable = || {
                AppError::invalid("queue", "The recorded conflicting file is unavailable.")
            };
            if !std::fs::symlink_metadata(&path).is_ok_and(|info| info.is_file()) {
                return Err(unavailable());
            }
            let canonical = dunce::canonicalize(&path).map_err(|_| unavailable())?;
            if canonical.parent() != path.parent() {
                return Err(unavailable());
            }
            if app.state().queue.conflict_path(job_id, file_id)? != path {
                return Err(AppError::invalid(
                    "queue",
                    "The recorded conflicting file changed.",
                ));
            }
            #[cfg(target_os = "macos")]
            return Ok(canonical);
            #[cfg(target_os = "linux")]
            {
                let folder = canonical.parent().ok_or_else(|| {
                    AppError::invalid("queue", "The conflicting file's folder is unavailable.")
                })?;
                return crate::platform::shell::open_folder_sync(
                    folder,
                    "The saved folder is missing or inaccessible.",
                    "The Linux desktop could not open the saved folder. Check that a file manager is available.",
                );
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let folder = path.parent().ok_or_else(|| {
                AppError::invalid("queue", "The conflicting file's folder is unavailable.")
            })?;
            crate::platform::shell::open_folder_sync(
                folder,
                "The conflicting file's folder is unavailable.",
                "Windows could not open the conflicting file's folder.",
            )
        }
    })
    .await;
    #[cfg(target_os = "macos")]
    return crate::platform::shell::reveal_file(&resolved?).await;
    #[cfg(not(target_os = "macos"))]
    resolved
}
