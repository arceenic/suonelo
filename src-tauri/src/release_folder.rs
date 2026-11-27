use crate::error::{AppError, Result};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, PartialEq, Eq)]
pub struct Evidence {
    pub path: PathBuf,
    pub root: PathBuf,
}

pub struct VerifiedFolder {
    pub path: PathBuf,
    pub evidence: Evidence,
}

fn unavailable(message: &str) -> AppError {
    AppError::invalid("queue", message)
}

/// Each folder needs its own verified saved file and recorded layout.
fn resolve(evidence: Vec<Evidence>) -> Vec<VerifiedFolder> {
    let mut roots = BTreeMap::new();
    for evidence in evidence {
        let Some(root) = verify(&evidence) else {
            continue;
        };
        #[cfg(target_os = "linux")]
        let key = root.clone();
        #[cfg(not(target_os = "linux"))]
        let key = crate::platform::paths::case_folded_key(&root);
        roots.entry(key).or_insert(VerifiedFolder {
            path: root,
            evidence,
        });
    }
    roots.into_values().collect()
}

fn verify(evidence: &Evidence) -> Option<PathBuf> {
    let root = dunce::canonicalize(&evidence.root).ok()?;
    let file = dunce::canonicalize(&evidence.path).ok()?;
    (root.is_dir() && file.is_file() && file.starts_with(&root)).then_some(root)
}

pub fn select(evidence: Vec<Evidence>) -> Result<VerifiedFolder> {
    let folders = resolve(evidence);
    let [folder] = folders.try_into().map_err(|folders: Vec<VerifiedFolder>| {
        if folders.is_empty() {
            unavailable("No verified output folder is available for this release.")
        } else {
            unavailable("This row contains multiple saved release folders.")
        }
    })?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn distinct_unix_paths_with_the_same_display_text_remain_distinct() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join(OsStr::from_bytes(b"album-\xfe"));
        let second = temp.path().join(OsStr::from_bytes(b"album-\xff"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        let save_evidence = |root: PathBuf| {
            std::fs::create_dir(&root).unwrap();
            let path = root.join("track.flac");
            std::fs::write(&path, b"saved").unwrap();
            Evidence { path, root }
        };
        let first = save_evidence(first);
        let second = save_evidence(second);
        assert_eq!(
            select(vec![first.clone()]).unwrap().path,
            dunce::canonicalize(&first.root).unwrap()
        );
        assert!(select(vec![first, second]).is_err());
    }

    #[test]
    fn recorded_roots_cover_multidisc_custom_and_auxiliary_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("Custom album");
        std::fs::create_dir_all(album.join("Disc 1")).unwrap();
        std::fs::create_dir_all(album.join("Disc 2")).unwrap();
        let mut evidence = Vec::new();
        for name in [
            "Disc 1/audio.flac",
            "Disc 2/audio.flac",
            "cover.jpg",
            "booklet.pdf",
            "playlist.m3u8",
            "Disc 1/lyrics.lrc",
        ] {
            let file = album.join(name);
            std::fs::write(&file, b"saved").unwrap();
            evidence.push(Evidence {
                path: file,
                root: album.clone(),
            });
        }
        assert_eq!(
            select(evidence.clone()).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            select(vec![evidence[3].clone()]).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            select(vec![evidence[2].clone()]).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        let other = temp.path().join("Another album");
        std::fs::create_dir(&other).unwrap();
        let file = other.join("track.mp3");
        std::fs::write(&file, b"saved").unwrap();
        evidence.push(Evidence {
            path: file.clone(),
            root: other.clone(),
        });
        assert_eq!(resolve(evidence.clone()).len(), 2);
        assert_eq!(
            select(vec![evidence[6].clone()]).unwrap().path,
            dunce::canonicalize(other).unwrap()
        );
        assert!(select(evidence).is_err());
        assert!(select(vec![]).is_err());
        for (path, root) in [
            (album.join("absent"), album.clone()),
            (album.join("cover.jpg"), temp.path().join("disconnected")),
            (file, album),
        ] {
            assert!(resolve(vec![Evidence { path, root }]).is_empty());
        }
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "opens Explorer; run explicitly in an isolated native smoke session"]
    fn native_explorer_smoke() {
        use std::path::Path;
        let root =
            std::env::var_os("SUONELO_FOLDER_SMOKE").expect("set an isolated existing folder");
        crate::platform::shell::open_folder_sync(
            Path::new(&root),
            "The saved release folder is missing or inaccessible.",
            "Windows could not open the saved release folder.",
        )
        .unwrap();
    }
}
