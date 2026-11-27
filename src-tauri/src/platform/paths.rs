use crate::error::AppError;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::error::Result;
use std::path::{Path, PathBuf};

pub fn case_folded_text(text: &str) -> String {
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_owned()
    }
}

pub fn case_folded_key(path: &Path) -> String {
    case_folded_text(&path.to_string_lossy())
}

#[cfg(windows)]
pub fn music_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Music, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
    unsafe {
        let value = SHGetKnownFolderPath(&FOLDERID_Music, KNOWN_FOLDER_FLAG(0x4000), None).ok()?;
        let path = value.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(value.0.cast()));
        path
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn music_folder() -> Option<PathBuf> {
    dirs::audio_dir()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn music_folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Music"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn profile_error() -> AppError {
    AppError::new(
        "portable_storage",
        "storage",
        "suonelo needs a writable data folder beside the executable.",
        true,
    )
}

#[cfg(target_os = "linux")]
pub fn profile_error() -> AppError {
    AppError::new(
        "profile_storage",
        "storage",
        "suonelo needs a writable profile folder in your XDG data directory.",
        true,
    )
}

#[cfg(target_os = "macos")]
pub fn profile_error() -> AppError {
    AppError::new(
        "profile_storage",
        "storage",
        "suonelo needs a writable profile folder in Application Support.",
        true,
    )
}

#[cfg(target_os = "macos")]
pub struct MacProfile {
    pub root: PathBuf,
    pub qa_name: Option<String>,
}

#[cfg(target_os = "macos")]
pub fn macos_profile(app: &tauri::App<tauri::Wry>) -> Result<MacProfile> {
    use tauri::Manager;

    let app_root = app.path().app_data_dir().map_err(|_| profile_error())?;
    let Some(name) = qa_profile_name(std::env::args_os().skip(1))? else {
        return Ok(MacProfile {
            root: app_root,
            qa_name: None,
        });
    };
    let qa_root = app_root.join("qa");
    let root = qa_root.join(&name);
    for path in [&app_root, &qa_root, &root, &root.join("data")] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => return Err(profile_error()),
        }
    }
    Ok(MacProfile {
        root,
        qa_name: Some(name),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn qa_profile_name(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<String>> {
    let mut arguments = arguments.into_iter().filter(|argument| {
        argument == "--qa-profile" || argument.as_encoded_bytes().starts_with(b"--qa-profile=")
    });
    let Some(argument) = arguments.next() else {
        return Ok(None);
    };
    let name = argument
        .to_str()
        .and_then(|argument| argument.strip_prefix("--qa-profile="))
        .filter(|name| {
            (1..=40).contains(&name.len())
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                })
        });
    let Some(name) = name.filter(|_| arguments.next().is_none()) else {
        return Err(AppError::invalid(
            "storage",
            "Choose one QA profile name using 1–40 lowercase letters, digits, or hyphens.",
        ));
    };
    Ok(Some(name.to_owned()))
}

#[cfg(target_os = "linux")]
pub fn linux_profile() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(profile_error)?;
    let qa_name = qa_profile_name(std::env::args_os().skip(1))?;
    create_linux_profile(&base, qa_name.as_deref())
}

#[cfg(target_os = "linux")]
fn create_linux_profile(base: &Path, qa_name: Option<&str>) -> Result<PathBuf> {
    use std::{fs, os::unix::fs::DirBuilderExt};

    fn profile_directory(path: &Path) -> Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                    Ok(())
                } else {
                    Err(profile_error())
                }
            }
            Err(_) => Err(profile_error()),
        }
    }

    fs::create_dir_all(base).map_err(|_| profile_error())?;
    let app_root = base.join("com.suonelo.desktop");
    profile_directory(&app_root)?;
    let root = if let Some(name) = qa_name {
        let qa_root = app_root.join("qa");
        profile_directory(&qa_root)?;
        let root = qa_root.join(name);
        profile_directory(&root)?;
        root
    } else {
        app_root
    };
    let data = root.join("data");
    profile_directory(&data)?;
    profile_directory(&data.join("webview"))?;
    dunce::canonicalize(root).map_err(|_| profile_error())
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn profiles_are_private_and_qa_cannot_alias_production() {
        let base = tempfile::tempdir().unwrap();
        let production = create_linux_profile(base.path(), None).unwrap();
        let qa = create_linux_profile(base.path(), Some("smoke-1")).unwrap();
        assert_eq!(qa, production.join("qa/smoke-1"));
        assert_eq!(
            create_linux_profile(base.path(), Some("smoke-1")).unwrap(),
            qa
        );
        for path in [&production, &qa, &qa.join("data/webview")] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        symlink(&production, production.join("qa/linked")).unwrap();
        assert!(create_linux_profile(base.path(), Some("linked")).is_err());
    }

    #[test]
    fn qa_profile_accepts_one_safe_name() {
        let parse = |args: &[&str]| qa_profile_name(args.iter().map(std::ffi::OsString::from));
        assert_eq!(
            parse(&["--qa-profile=qa-01"]).unwrap().as_deref(),
            Some("qa-01")
        );
        assert_eq!(parse(&["--unrelated"]).unwrap(), None);
        for args in [
            &["--qa-profile"][..],
            &["--qa-profile="][..],
            &["--qa-profile=../main"][..],
            &["--qa-profile=a", "--qa-profile=b"][..],
        ] {
            assert!(parse(args).is_err());
        }
    }
}
