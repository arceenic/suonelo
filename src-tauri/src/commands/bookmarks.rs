use super::{Managed, blocking};
use crate::{
    app::{App, BookmarksSnapshot, SubmissionReport},
    error::{AppError, Result},
    storage::BookmarkDto,
};
use serde::Deserialize;
use std::sync::Arc;
use tauri::ipc::Channel;

#[tauri::command]
pub fn list_bookmarks(app: Managed<'_>) -> Result<Vec<BookmarkDto>> {
    Ok(app.state().bookmark_dtos())
}

#[tauri::command]
pub fn subscribe_bookmarks(app: Managed<'_>, channel: Channel<BookmarksSnapshot>) -> Result<()> {
    app.state().subscribe_bookmarks(channel);
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BookmarkScope {
    All,
    Selected { ids: Vec<u64> },
    Single { id: u64 },
}

#[tauri::command]
pub async fn download_bookmarks(
    app: Managed<'_>,
    scope: BookmarkScope,
) -> Result<SubmissionReport> {
    let ids = match scope {
        BookmarkScope::All => None,
        BookmarkScope::Selected { ids } => Some(ids),
        BookmarkScope::Single { id } => Some(vec![id]),
    };
    let app = app.inner().clone();
    blocking(move || app.download_bookmarks(ids)).await
}

#[tauri::command]
pub async fn retry_bookmark(app: Managed<'_>, id: u64) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    if !state.bookmarks.contains_key(&id) {
        return Err(AppError::invalid(
            "bookmarks",
            "This bookmark is no longer saved.",
        ));
    }
    if state.account.is_none() {
        return Err(AppError::new(
            "account_required",
            "bookmarks",
            state.waiting_reason(),
            true,
        ));
    }
    app.inner().resolve_bookmark(&mut state, id);
    Ok(())
}

#[tauri::command]
pub async fn remove_bookmarks(app: Managed<'_>, ids: Vec<u64>) -> Result<()> {
    delete_bookmarks(app.inner().clone(), Some(ids)).await
}

#[tauri::command]
pub async fn clear_bookmarks(app: Managed<'_>) -> Result<()> {
    delete_bookmarks(app.inner().clone(), None).await
}

async fn delete_bookmarks(app: Arc<App>, ids: Option<Vec<u64>>) -> Result<()> {
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        let ids = ids.unwrap_or_else(|| state.bookmarks.keys().copied().collect());
        app.storage.remove_bookmarks(&ids)?;
        for id in ids {
            state.bookmarks.remove(&id);
            state.bookmark_refresh.remove(&id);
        }
        state.bookmarks_changed();
        Ok(())
    })
    .await
}
