use crate::error::{AppError, Result};
use std::path::Path;

#[cfg(windows)]
fn shell_open(target: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::{PCWSTR, w},
    };
    let wide: Vec<u16> = target.encode_wide().chain([0]).collect();
    let opened = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    opened.0 as isize > 32
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn open_folder_sync(
    folder: &Path,
    missing_message: &'static str,
    error_message: &'static str,
) -> Result<()> {
    if !folder.is_dir() {
        return Err(AppError::invalid("queue", missing_message));
    }
    #[cfg(windows)]
    if shell_open(dunce::simplified(folder).as_os_str()) {
        return Ok(());
    }
    #[cfg(not(windows))]
    let _ = error_message;
    Err(AppError::new("open_folder", "queue", error_message, true))
}

#[cfg(target_os = "linux")]
pub fn open_folder_sync(
    folder: &Path,
    missing_message: &'static str,
    error_message: &'static str,
) -> Result<()> {
    use gio::prelude::FileExt;

    if !folder.is_absolute() || !folder.is_dir() {
        return Err(AppError::invalid("queue", missing_message));
    }
    let uri = gio::File::for_path(folder).uri();
    open_linux_uri_sync(&uri, "open_folder", "queue", error_message)
}

#[cfg(target_os = "linux")]
pub fn open_linux_uri_sync(
    uri: &str,
    code: &'static str,
    scope: &'static str,
    message: &'static str,
) -> Result<()> {
    gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>)
        .map_err(|_| AppError::new(code, scope, message, true))
}

#[cfg(target_os = "macos")]
async fn open_with_macos(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    code: &'static str,
    scope: &'static str,
    message: &'static str,
) -> Result<()> {
    use std::{process::Stdio, time::Duration};
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/open")
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    if matches!(status, Ok(Ok(status)) if status.success()) {
        return Ok(());
    }
    Err(AppError::new(code, scope, message, true))
}

#[cfg(target_os = "macos")]
pub async fn open_folder(folder: &Path) -> Result<()> {
    open_with_macos(
        [
            std::ffi::OsStr::new("-a"),
            std::ffi::OsStr::new("Finder"),
            folder.as_os_str(),
        ],
        "open_folder",
        "queue",
        "Finder could not open the saved release folder.",
    )
    .await
}

#[cfg(target_os = "macos")]
pub async fn reveal_file(path: &Path) -> Result<()> {
    open_with_macos(
        [std::ffi::OsStr::new("-R"), path.as_os_str()],
        "open_folder",
        "queue",
        "Finder could not reveal the conflicting file.",
    )
    .await
}

pub async fn open_url(url: &'static str) -> Result<()> {
    #[cfg(target_os = "macos")]
    return open_with_macos(
        [url],
        "open_link",
        "application",
        "Could not open the selected website.",
    )
    .await;
    #[cfg(target_os = "linux")]
    return tokio::task::spawn_blocking(move || {
        open_linux_uri_sync(
            url,
            "open_link",
            "application",
            "Could not open the selected website.",
        )
    })
    .await
    .map_err(|_| AppError::invalid("application", "The desktop operation was interrupted."))?;
    #[cfg(windows)]
    {
        if shell_open(std::ffi::OsStr::new(url)) {
            return Ok(());
        }
        Err(AppError::new(
            "open_link",
            "application",
            "Could not open the selected website.",
            true,
        ))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
        Err(AppError::new(
            "open_link",
            "application",
            "Could not open the selected website.",
            true,
        ))
    }
}
