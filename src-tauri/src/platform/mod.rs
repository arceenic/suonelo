pub mod paths;
pub mod shell;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod macos_webview;

#[cfg(windows)]
pub use windows::show_error;

#[cfg(not(windows))]
pub fn show_error(message: &str) {
    eprintln!("{message}");
}
