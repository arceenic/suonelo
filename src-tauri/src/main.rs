#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod account;
mod app;
mod commands;
mod diagnostics;
mod error;
mod link_import;
mod locations;
mod media;
mod net_url;
mod platform;
#[cfg(any(not(target_os = "macos"), test))]
mod profile_migration;
mod qobuz;
mod queue;
mod recovery;
mod release_folder;
mod search;
mod storage;
mod vault;

use diagnostics::preload;
#[cfg(target_os = "macos")]
use platform::macos;
#[cfg(target_os = "macos")]
use tauri::WebviewWindowBuilder;
use tauri::{Emitter, Manager, WindowEvent};

fn main() {
    if preload::begin().is_err() {
        return;
    }
    #[cfg(windows)]
    if !platform::windows::check_webview_runtime() {
        platform::show_error(
            "suonelo requires the Microsoft Edge WebView2 Evergreen Runtime. Install it from https://developer.microsoft.com/microsoft-edge/webview2/ and start suonelo again.",
        );
        return;
    }
    #[cfg(not(target_os = "macos"))]
    let state = match app::App::initialize() {
        Ok(state) => state,
        Err(error) => {
            platform::show_error(&error.message);
            return;
        }
    };
    preload::mark("profile_lock_acquired_storage_ready", serde_json::json!({}));
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init());
    #[cfg(not(target_os = "macos"))]
    let builder = builder.manage(state.clone());
    let builder = builder
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if let Err(error) = setup_macos(app) {
                macos::show_startup_error(app.handle(), error);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = state.handle.set(app.handle().clone());
                let window = app
                    .config()
                    .app
                    .windows
                    .first()
                    .ok_or("Missing main window configuration")?;
                #[cfg(any(windows, target_os = "linux"))]
                let window = &{
                    let mut config = window.clone();
                    config.title = format!("suonelo v{}", env!("CARGO_PKG_VERSION"));
                    config
                };
                preload::build(app, window, &state.root)?;
                tauri::async_runtime::spawn(state.clone().start());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if matches!(window.label(), "main" | "startup")
                    && let Some(exit) = preload::native_close(window)
                {
                    api.prevent_close();
                    if exit {
                        window.app_handle().exit(0);
                    }
                    return;
                }
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.emit("close-requested", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::preload_frontend_ready,
            commands::queue_snapshot,
            commands::set_queue_paused,
            commands::subscribe_queue,
            commands::submit_links,
            commands::inspect_link_file,
            commands::submit_imported_links,
            commands::clear_downloads,
            commands::retry_job,
            commands::retry_file,
            commands::replace_search_audio,
            commands::download_again,
            commands::cancel_job,
            commands::remove_job,
            commands::apply_settings_to_pending,
            commands::decide_large_submission,
            commands::list_bookmarks,
            commands::subscribe_bookmarks,
            commands::download_bookmarks,
            commands::retry_bookmark,
            commands::remove_bookmarks,
            commands::clear_bookmarks,
            commands::read_settings,
            commands::patch_settings,
            commands::save_connection_limit,
            commands::migrate_theme,
            commands::reset_settings,
            commands::reset_vault,
            #[cfg(target_os = "linux")]
            commands::retry_vault_access,
            commands::validate_credentials,
            commands::start_email_sign_in,
            commands::confirm_account_validation,
            commands::cancel_account_validation,
            commands::open_containing_folder,
            commands::release_folder_status,
            commands::open_release_folder,
            commands::retry_connection,
            commands::search_genres,
            commands::search_query,
            commands::search_cancel,
            commands::search_detail,
            commands::search_artwork,
            commands::clear_search_history,
            commands::prepare_search_submission,
            commands::commit_search_submission,
            commands::metadata_catalog,
            commands::preview_output,
            commands::request_close,
            commands::shutdown_session,
            commands::open_about_link,
        ]);
    #[cfg(not(target_os = "macos"))]
    if builder.run(tauri::generate_context!()).is_err() {
        #[cfg(windows)]
        platform::show_error(
            "suonelo could not open the desktop window. Check that the WebView2 Runtime is usable and the portable folder is writable.",
        );
        #[cfg(target_os = "linux")]
        platform::show_error(
            "suonelo could not open the desktop window. Check WebKitGTK 4.1, the graphical session, and access to your XDG data directory.",
        );
        #[cfg(not(any(windows, target_os = "linux")))]
        platform::show_error("suonelo could not initialize the desktop runtime.");
    }
    #[cfg(target_os = "macos")]
    match builder.build(tauri::generate_context!()) {
        Ok(app) => app.run(|_, _| {}),
        Err(_) => platform::show_error("suonelo could not initialize the macOS desktop runtime."),
    }
}

#[cfg(target_os = "macos")]
fn setup_macos(app: &mut tauri::App<tauri::Wry>) -> error::Result<()> {
    let profile = platform::paths::macos_profile(app)?;
    let state = app::App::initialize_macos(profile.root, profile.qa_name.as_deref())?;
    let _ = state.handle.set(app.handle().clone());
    app.manage(state.clone());
    macos::install_quit_handler(app.handle())?;

    let window = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(macos::webview_error)?;
    WebviewWindowBuilder::from_config(app, window)
        .and_then(|builder| builder.incognito(profile.qa_name.is_some()).build())
        .map_err(|_| macos::webview_error())?;
    tauri::async_runtime::spawn(state.start());
    Ok(())
}
