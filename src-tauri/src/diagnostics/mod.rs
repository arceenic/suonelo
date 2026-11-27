#[cfg(all(windows, feature = "preload-prototype"))]
pub mod preload;

#[cfg(not(all(windows, feature = "preload-prototype")))]
pub mod preload {
    use std::path::Path;
    use tauri::{AppHandle, WebviewWindowBuilder, Window};

    #[inline]
    pub fn begin() -> std::io::Result<()> {
        Ok(())
    }

    #[inline]
    pub fn mark(_stage: &str, _detail: serde_json::Value) {}

    #[inline]
    pub fn frontend_ready(_handle: &AppHandle) {}

    #[inline]
    pub fn native_close(_window: &Window) -> Option<bool> {
        None
    }

    #[inline]
    pub fn build(
        app: &tauri::App,
        config: &tauri::utils::config::WindowConfig,
        root: &Path,
    ) -> tauri::Result<()> {
        WebviewWindowBuilder::from_config(app, config)?
            .data_directory(root.join("data").join("webview"))
            .build()?;
        Ok(())
    }
}
