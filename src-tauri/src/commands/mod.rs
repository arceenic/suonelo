mod account;
mod bookmarks;
mod queue;
mod search;
mod settings;
mod system;

#[cfg(test)]
mod tests;

pub use account::*;
pub use bookmarks::*;
pub use queue::*;
pub use search::*;
pub use settings::*;
pub use system::*;

use crate::{
    app::App,
    error::{AppError, Result},
};
use std::sync::Arc;
use tauri::State;

pub(crate) type Managed<'a> = State<'a, Arc<App>>;

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::task)?
}
