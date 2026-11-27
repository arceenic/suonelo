use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{MediaError, blocking};

pub const RECEIPT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS media_receipts (
    location TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    identity TEXT NOT NULL,
    variant TEXT NOT NULL,
    sha256 TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS media_stages (location TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS media_pending_updates (
    location TEXT PRIMARY KEY,
    old_sha256 TEXT NOT NULL,
    new_sha256 TEXT NOT NULL,
    path_location TEXT NOT NULL DEFAULT '',
    audio_identity TEXT,
    audio_variant TEXT
);
"#;

const STAGE_PREFIX: &str = ".suonelo-stage-";
const LEGACY_STAGE_PREFIX: &str = ".qbdlx-stage-"; // Recovery compatibility only.

#[derive(Clone)]
pub(super) struct OutputStore {
    database: PathBuf,
    portable_root: PathBuf,
    db: Arc<Mutex<Option<Connection>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    pub kind: &'static str,
    pub identity: String,
    pub variant: String,
}

#[derive(Clone)]
pub struct AudioReplacement {
    pub token: String,
    path: PathBuf,
    canonical: PathBuf,
    track: String,
    variant: String,
    digest: String,
}

pub fn migrate_replacement_receipts(db: &Connection) -> rusqlite::Result<()> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('media_pending_updates') WHERE name='audio_identity')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        db.execute_batch(
            "ALTER TABLE media_pending_updates ADD COLUMN audio_identity TEXT;
             ALTER TABLE media_pending_updates ADD COLUMN audio_variant TEXT;"
        )?;
    }
    Ok(())
}

pub(super) struct Stage {
    file: tempfile::TempPath,
    location: String,
}

impl Stage {
    pub fn path(&self) -> &Path {
        &self.file
    }
}

fn storage_error() -> MediaError {
    MediaError::new(
        "file_index",
        "The finalized-file index could not be accessed.",
        true,
    )
}

fn conflict(path: &Path) -> MediaError {
    MediaError::new(
        "output_conflict",
        "The path already contains a file that could not be verified for this output.",
        false,
    )
    .at_path(path)
}

impl OutputStore {
    pub fn new(database: PathBuf, portable_root: PathBuf) -> Self {
        let portable_root = fs::canonicalize(&portable_root).unwrap_or(portable_root);
        Self {
            database,
            portable_root,
            db: Arc::new(Mutex::new(None)),
        }
    }

    fn with_db<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, MediaError>,
    ) -> Result<T, MediaError> {
        let mut slot = self.db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            let connection = Connection::open(&self.database).map_err(|_| storage_error())?;
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(|_| storage_error())?;
            let _ = connection.execute_batch("PRAGMA journal_mode = WAL;");
            *slot = Some(connection);
        }
        operation(slot.as_mut().expect("opened SQLite connection"))
    }

    fn location(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.portable_root) {
            format!("portable:{}", relative.to_string_lossy())
        } else {
            format!("external:{}", path.to_string_lossy())
        }
    }

    fn receipt_location(&self, path: &Path) -> String {
        crate::platform::paths::case_folded_text(&self.location(path))
    }

    fn scoped_path<'a>(&self, location: &'a str) -> Option<(PathBuf, &'a str)> {
        let (scope, value) = location.split_once(':')?;
        let path = Path::new(value);
        let resolved = match scope {
            "portable"
                if path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))) =>
            {
                self.portable_root.join(path)
            }
            "external"
                if path.is_absolute()
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir)) =>
            {
                path.to_owned()
            }
            _ => return None,
        };
        Some((resolved, value))
    }

    fn stage_path(&self, location: &str) -> Option<PathBuf> {
        let (resolved, value) = self.scoped_path(location)?;
        let path = Path::new(value);
        if ![STAGE_PREFIX, LEGACY_STAGE_PREFIX].iter().any(|prefix| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        }) {
            return None;
        }
        Some(resolved)
    }

    pub async fn stage(&self, directory: &Path, suffix: &str) -> Result<Stage, MediaError> {
        let store = self.clone();
        let directory = directory.to_owned();
        let suffix = suffix.to_owned();
        blocking(move || {
            let file = tempfile::Builder::new()
                .prefix(STAGE_PREFIX)
                .suffix(&suffix)
                .tempfile_in(&directory)
                .map_err(|error| MediaError::disk(error).at_path(&directory))?
                .into_temp_path();
            let location = store.location(&file);
            store.with_db(|connection| {
                connection
                    .prepare_cached("INSERT INTO media_stages(location) VALUES (?1)")
                    .map_err(|_| storage_error())?
                    .execute([&location])
                    .map_err(|_| storage_error())
            })?;
            Ok(Stage { file, location })
        })
        .await
    }

    pub async fn discard(&self, stage: Stage) {
        let store = self.clone();
        let _ = blocking(move || {
            if stage.file.close().is_ok() {
                store.with_db(|connection| {
                    connection
                        .prepare_cached("DELETE FROM media_stages WHERE location = ?1")
                        .map_err(|_| storage_error())?
                        .execute([&stage.location])
                        .map_err(|_| storage_error())
                })?;
            }
            Ok(())
        })
        .await;
    }

    pub async fn cleanup_stages(&self) -> Result<(), MediaError> {
        let store = self.clone();
        blocking(move || {
            let locations = store.with_db(|connection| {
                let mut statement = connection
                    .prepare_cached("SELECT location FROM media_stages")
                    .map_err(|_| storage_error())?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|_| storage_error())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| storage_error())
            })?;
            for location in locations {
                if let Some(path) = store.stage_path(&location)
                    && let Err(error) = fs::remove_file(path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    continue;
                }
                store.with_db(|connection| {
                    connection
                        .prepare_cached("DELETE FROM media_stages WHERE location = ?1")
                        .map_err(|_| storage_error())?
                        .execute([&location])
                        .map_err(|_| storage_error())
                })?;
            }
            let updates = store.with_db(|connection| {
                let mut statement = connection
                    .prepare_cached("SELECT location, path_location FROM media_pending_updates")
                    .map_err(|_| storage_error())?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|_| storage_error())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| storage_error())
            })?;
            for (location, raw_location) in updates {
                if let Some(path) = store.saved_path(if raw_location.is_empty() {
                    &location
                } else {
                    &raw_location
                }) {
                    store.reconcile_update(&location, &path, &CancellationToken::new())?;
                }
            }
            Ok(())
        })
        .await
    }

    fn reuse(
        &self,
        key: &str,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<(String, String)>, MediaError> {
        self.reconcile_update(key, path, cancel)?;
        match fs::symlink_metadata(path) {
            Ok(info) if info.is_file() => {}
            Ok(_) => return Err(conflict(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MediaError::disk(error)),
        }
        let stored: Option<(String, String)> = self.with_db(|connection| {
            connection
                .prepare_cached(
                    "SELECT variant, sha256 FROM media_receipts WHERE location = ?1 AND kind = ?2 AND identity = ?3",
                )
                .map_err(|_| storage_error())?
                .query_row(params![key, receipt.kind, receipt.identity], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
                .map_err(|_| storage_error())
        })?;
        let Some((variant, expected)) = stored else {
            return Err(conflict(path));
        };
        if !same_variant(&variant, &receipt.variant, receipt.kind) {
            return Err(conflict(path));
        }
        if expected != file_hash(path, Some(cancel))? {
            return Err(conflict(path));
        }
        Ok(Some((variant, expected)))
    }

    fn saved_path(&self, location: &str) -> Option<PathBuf> {
        self.scoped_path(location).map(|(resolved, _)| resolved)
    }

    fn reconcile_update(
        &self,
        key: &str,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), MediaError> {
        let pending: Option<(String, String, Option<String>, Option<String>)> =
            self.with_db(|connection| {
                connection
                    .prepare_cached(
                        "SELECT old_sha256, new_sha256, audio_identity, audio_variant FROM media_pending_updates WHERE location = ?1",
                    )
                    .map_err(|_| storage_error())?
                    .query_row([key], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .optional()
                    .map_err(|_| storage_error())
            })?;
        let Some((old, new, audio_identity, audio_variant)) = pending else {
            return Ok(());
        };
        let digest = match file_hash(path, Some(cancel)) {
            Ok(digest) => Some(digest),
            Err(error) if error.code == "disk" && !path.exists() => None,
            Err(error) => return Err(error),
        };
        let matched = digest.as_deref() == Some(new.as_str());
        self.finish_pending_update_row(
            key,
            &old,
            &new,
            matched,
            audio_identity.as_deref().zip(audio_variant.as_deref()),
        )
    }

    fn finish_pending_update_row(
        &self,
        key: &str,
        old: &str,
        new: &str,
        matched: bool,
        audio: Option<(&str, &str)>,
    ) -> Result<(), MediaError> {
        self.with_db(|connection| {
            let transaction = connection
                .transaction()
                .map_err(|_| storage_error())?;
            if matched {
                if let Some((identity, variant)) = audio {
                    transaction
                        .prepare_cached(
                            "UPDATE media_receipts SET sha256=?1, variant=?2 WHERE location=?3 AND kind='audio' AND identity=?4 AND sha256=?5",
                        )
                        .map_err(|_| storage_error())?
                        .execute(params![new, variant, key, identity, old])
                        .map_err(|_| storage_error())?;
                } else {
                    transaction
                        .prepare_cached(
                            "UPDATE media_receipts SET sha256 = ?1 WHERE location = ?2 AND kind = 'playlist' AND sha256 = ?3",
                        )
                        .map_err(|_| storage_error())?
                        .execute(params![new, key, old])
                        .map_err(|_| storage_error())?;
                }
            }
            transaction
                .prepare_cached("DELETE FROM media_pending_updates WHERE location = ?1")
                .map_err(|_| storage_error())?
                .execute([key])
                .map_err(|_| storage_error())?;
            transaction.commit().map_err(|_| storage_error())
        })
    }

    pub async fn verify_variant(
        &self,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let receipt = receipt.clone();
        let cancel = cancel.clone();
        blocking(move || {
            Ok(store
                .reuse(&store.receipt_location(&path), &path, &receipt, &cancel)?
                .map(|(variant, _)| variant))
        })
        .await
    }

    pub async fn verify(
        &self,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        Ok(self.verify_variant(path, receipt, cancel).await?.is_some())
    }

    pub async fn publish(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, false, None, cancel).await
    }

    pub async fn publish_playlist(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        allow_refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, allow_refresh, None, cancel)
            .await
    }

    fn replacement(
        &self, path: &Path, track: &str, cancel: &CancellationToken,
    ) -> Result<AudioReplacement, MediaError> {
        let key = self.receipt_location(path);
        self.reconcile_update(&key, path, cancel)?;
        if !fs::symlink_metadata(path).is_ok_and(|file| file.is_file()) {
            return Err(conflict(path));
        }
        let canonical = dunce::canonicalize(path).map_err(|_| conflict(path))?;
        let (variant, digest): (String, String) = self
            .with_db(|db| {
                db.prepare_cached(
                    "SELECT variant, sha256 FROM media_receipts WHERE location=?1 AND kind='audio' AND identity=?2",
                )
                .map_err(|_| storage_error())?
                .query_row(params![key, track], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
                .map_err(|_| storage_error())
            })?
            .ok_or_else(|| conflict(path))?;
        if file_hash(path, Some(cancel))? != digest {
            return Err(conflict(path));
        }
        Ok(AudioReplacement {
            token: format!("{:032x}", rand::random::<u128>()),
            path: path.to_owned(), canonical, track: track.to_owned(), variant, digest,
        })
    }

    fn check_replacement(
        &self, path: &Path, track: &str, approval: &AudioReplacement, cancel: &CancellationToken,
    ) -> Result<(), MediaError> {
        if approval.path != path || approval.track != track {
            return Err(conflict(path));
        }
        let current = self.replacement(path, track, cancel)?;
        if current.canonical != approval.canonical || current.variant != approval.variant || current.digest != approval.digest {
            return Err(conflict(path));
        }
        Ok(())
    }

    pub async fn offer_replacement(&self, path: &Path, track: &str, cancel: &CancellationToken) -> Result<AudioReplacement, MediaError> {
        let (store, path, track, cancel) = (self.clone(), path.to_owned(), track.to_owned(), cancel.clone());
        blocking(move || store.replacement(&path, &track, &cancel)).await
    }

    pub async fn verify_replacement(&self, path: &Path, track: &str, approval: &AudioReplacement, cancel: &CancellationToken) -> Result<(), MediaError> {
        let (store, path, track, approval, cancel) = (self.clone(), path.to_owned(), track.to_owned(), approval.clone(), cancel.clone());
        blocking(move || store.check_replacement(&path, &track, &approval, &cancel)).await
    }

    pub async fn verified_audio(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let cancel = cancel.clone();
        blocking(move || {
            if !fs::symlink_metadata(&path).is_ok_and(|info| info.is_file()) {
                return Ok(false);
            }
            let expected: Option<String> = store.with_db(|connection| {
                connection
                    .prepare_cached(
                        "SELECT sha256 FROM media_receipts WHERE location = ?1 AND kind IN ('audio', 'audio_preview')",
                    )
                    .map_err(|_| storage_error())?
                    .query_row([store.receipt_location(&path)], |row| row.get(0))
                    .optional()
                    .map_err(|_| storage_error())
            })?;
            let Some(expected) = expected else {
                return Ok(false);
            };
            match file_hash(&path, Some(&cancel)) {
                Ok(actual) => Ok(expected == actual),
                Err(error) if error.code == "disk" => Ok(false),
                Err(error) => Err(error),
            }
        })
        .await
    }

    pub async fn publish_replacement(
        &self, stage: Stage, path: &Path, receipt: &Receipt, approval: &AudioReplacement, cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, true, Some(approval.clone()), cancel).await
    }

    async fn publish_file(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        allow_refresh: bool,
        replacement: Option<AudioReplacement>,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let receipt = receipt.clone();
        let cancel = cancel.clone();
        blocking(move || {
            let mut file = Some(stage.file);
            let result = (|| {
                let key = store.receipt_location(&path);
                let existing_hash = if let Some(approval) = &replacement {
                    store.check_replacement(&path, &receipt.identity, approval, &cancel)?;
                    Some(approval.digest.clone())
                } else {
                    store
                        .reuse(&key, &path, &receipt, &cancel)?
                        .map(|(_, sha)| sha)
                };
                if existing_hash.is_some() && !allow_refresh && receipt.kind != "playlist" {
                    return Ok(true);
                }
                let stage = file.as_ref().expect("unpublished temporary path");
                let sha256 = file_hash(stage, Some(&cancel))?;
                cancel.check()?;
                fs::OpenOptions::new().write(true).open(stage).and_then(|file| file.sync_all()).map_err(MediaError::disk)?;
                if let Some(previous) = &existing_hash {
                    if previous == &sha256 && replacement.is_none() {
                        return Ok(true);
                    }
                    if !allow_refresh {
                        return Err(conflict(&path));
                    }
                    if let Some(approval) = &replacement
                        && &approval.digest != previous
                    {
                        return Err(conflict(&path));
                    }
                    store.with_db(|connection| {
                        let recorded: String = connection
                            .prepare_cached("SELECT sha256 FROM media_receipts WHERE location = ?1")
                            .map_err(|_| storage_error())?
                            .query_row([&key], |row| row.get(0))
                            .map_err(|_| storage_error())?;
                        if &recorded != previous {
                            return Err(conflict(&path));
                        }
                        let audio = replacement.as_ref().map(|_| (&receipt.identity, &receipt.variant));
                        connection
                            .prepare_cached(
                                "INSERT OR REPLACE INTO media_pending_updates(location, old_sha256, new_sha256, path_location, audio_identity, audio_variant) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            )
                            .map_err(|_| storage_error())?
                            .execute(params![key, previous, sha256, store.location(&path), audio.map(|(id, _)| id), audio.map(|(_, variant)| variant)])
                            .map_err(|_| storage_error())?;
                        Ok(())
                    })?;
                } else {
                    store.with_db(|connection| {
                        connection
                            .prepare_cached(
                                "INSERT OR REPLACE INTO media_receipts(location, kind, identity, variant, sha256) VALUES (?1, ?2, ?3, ?4, ?5)",
                            )
                            .map_err(|_| storage_error())?
                            .execute(params![key, receipt.kind, receipt.identity, receipt.variant, sha256])
                            .map_err(|_| storage_error())?;
                        Ok(())
                    })?;
                }
                let stage = file.take().expect("unpublished temporary path");
                let persisted = if existing_hash.is_some() {
                    stage.persist(&path)
                } else {
                    stage.persist_noclobber(&path)
                };
                if let Err(error) = persisted {
                    file = Some(error.path);
                    let _ = store.with_db(|connection| {
                        if existing_hash.is_some() {
                            let _ = connection
                                .prepare_cached("DELETE FROM media_pending_updates WHERE location = ?1")
                                .and_then(|mut s| s.execute([&key]));
                        } else {
                            let _ = connection
                                .prepare_cached("DELETE FROM media_receipts WHERE location = ?1 AND sha256 = ?2")
                                .and_then(|mut s| s.execute(params![key, sha256]));
                        }
                        Ok(())
                    });
                    return Err(if error.error.kind() == io::ErrorKind::AlreadyExists { conflict(&path) } else { MediaError::disk(error.error).at_path(&path) });
                }
                if let Some(previous) = &existing_hash {
                    let audio = replacement
                        .as_ref()
                        .map(|_| (receipt.identity.as_str(), receipt.variant.as_str()));
                    store.finish_pending_update_row(&key, previous, &sha256, true, audio)?;
                }
                Ok(false)
            })();
            if file.is_none_or(|file| file.close().is_ok()) {
                let _ = store.with_db(|connection| {
                    let _ = connection
                        .prepare_cached("DELETE FROM media_stages WHERE location = ?1")
                        .and_then(|mut s| s.execute([&stage.location]));
                    Ok(())
                });
            }
            result.map_err(|error: MediaError| error.at_path(&path))
        }).await
    }
}

fn same_variant(stored: &str, expected: &str, kind: &str) -> bool {
    if stored == expected {
        return true;
    }
    if !matches!(kind, "audio" | "audio_preview") {
        return false;
    }
    let parse = |text: &str| {
        let mut value = serde_json::from_str::<serde_json::Value>(text).ok()?;
        let fields = value.as_object_mut()?;
        fields.remove("_lyrics");
        fields.remove("reencode_flac");
        Some(value)
    };
    match (parse(stored), parse(expected)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn file_hash(path: &Path, cancel: Option<&CancellationToken>) -> Result<String, MediaError> {
    let mut source = fs::File::open(path).map_err(MediaError::disk)?;
    let mut hasher = Sha256::new();
    let mut bytes = vec![0_u8; 128 * 1024];
    loop {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        let read = source.read(&mut bytes).map_err(MediaError::disk)?;
        if read == 0 {
            break;
        }
        hasher.update(&bytes[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) trait CheckCancelled {
    fn check(&self) -> Result<(), MediaError>;
}

impl CheckCancelled for CancellationToken {
    fn check(&self) -> Result<(), MediaError> {
        if self.is_cancelled() {
            Err(MediaError::cancelled())
        } else {
            Ok(())
        }
    }
}

pub(super) async fn safe_directory(root: &Path, path: &Path) -> Result<PathBuf, MediaError> {
    let root = root.to_owned();
    let path = path.to_owned();
    blocking(move || {
        let outside = || {
            MediaError::new(
                "output_location",
                "The rendered path leaves the selected output folder.",
                false,
            )
        };
        let relative = path.strip_prefix(&root).map_err(|_| outside())?;
        let canonical_root = fs::canonicalize(&root).map_err(MediaError::disk)?;
        if !canonical_root.is_dir() {
            return Err(outside());
        }
        let mut current = canonical_root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(outside());
            };
            current.push(name);
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(MediaError::disk(error)),
            }
            let info = fs::symlink_metadata(&current).map_err(MediaError::disk)?;
            if !info.is_dir() || super::is_reparse_or_symlink(&info) {
                return Err(outside());
            }
            current = fs::canonicalize(&current).map_err(MediaError::disk)?;
            if !current.starts_with(&canonical_root) {
                return Err(outside());
            }
        }
        Ok(current)
    })
    .await
}

#[cfg(test)]
mod tests;
