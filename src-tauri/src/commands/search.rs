use super::{Managed, blocking};
use crate::error::Result;

#[tauri::command]
pub fn search_genres() -> &'static [crate::search::Genre] {
    crate::search::genres()
}

#[tauri::command]
pub async fn search_query(
    app: Managed<'_>,
    request: crate::search::Query,
) -> Result<crate::search::Snapshot> {
    crate::search::query(&app, request).await
}

#[tauri::command]
pub fn search_cancel(app: Managed<'_>, sequence: u64) {
    app.state().search.cancel(sequence);
}

#[tauri::command]
pub async fn search_detail(
    app: Managed<'_>,
    request: crate::search::DetailRequest,
) -> Result<crate::search::DetailResult> {
    crate::search::detail(&app, request).await
}

#[tauri::command]
pub async fn search_artwork(
    app: Managed<'_>,
    generation: u64,
    key: crate::qobuz::search::Id,
) -> Result<Option<String>> {
    crate::search::artwork(&app, generation, key).await
}

#[tauri::command]
pub async fn clear_search_history(app: Managed<'_>) -> Result<String> {
    let app = app.inner().clone();
    blocking(move || crate::search::clear_history(&app)).await
}

#[tauri::command]
pub fn prepare_search_submission(
    app: Managed<'_>,
    request: crate::search::Submission,
) -> Result<crate::search::Review> {
    crate::search::prepare(&app, request)
}

#[tauri::command]
pub async fn commit_search_submission(
    app: Managed<'_>,
    token: String,
) -> Result<crate::search::SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || crate::search::commit(&app, token)).await
}
