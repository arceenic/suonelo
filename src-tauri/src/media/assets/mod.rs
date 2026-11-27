mod validate;
#[cfg(test)]
mod tests;

pub use validate::validate_cover_filename;
pub(in crate::media) use validate::{ImageInfo, image_data_info};
use validate::{COVER_EXTENSIONS, extra_directory, image_info, verify_pdf};

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    Media, MediaError, TransferProgress, blocking, cancellable, copy_stage, existing_root,
    output_store::{CheckCancelled, Receipt, Stage},
    template::sanitize,
};
use crate::qobuz::ResolvedTrack;
use crate::recovery::{RetryBudget, TransferContext};

pub(super) const ART_LIMIT: u64 = 40 * 1024 * 1024;
const BOOKLET_LIMIT: u64 = 128 * 1024 * 1024;

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd,
)]
pub enum ArtworkResolution {
    #[serde(rename = "300")]
    Px300,
    #[serde(rename = "600")]
    Px600,
    #[serde(rename = "1200")]
    Px1200,
    #[default]
    #[serde(rename = "original")]
    Original,
}

impl ArtworkResolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Px1200 => "1200 px",
            Self::Px600 => "600 px",
            Self::Px300 => "300 px",
        }
    }

    fn px(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::Px1200 => Some(1200),
            Self::Px600 => Some(600),
            Self::Px300 => Some(300),
        }
    }
}

pub struct ArtworkOutput {
    pub path: PathBuf,
    pub resolution: ArtworkResolution,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub reused: bool,
}

/// Staged provider bytes shared by the tracks and sidecar of one release attempt.
#[derive(Default)]
pub struct ArtworkCache {
    staged: OnceCell<Result<StagedArtwork, MediaError>>,
}

impl ArtworkCache {
    async fn prepare(
        &self,
        load: impl AsyncFnOnce() -> Result<StagedArtwork, MediaError>,
    ) -> Result<&StagedArtwork, MediaError> {
        self.staged
            .get_or_try_init(|| async {
                match load().await {
                    Err(error) if matches!(error.code, "disk_full" | "cancelled") => Err(error),
                    settled => Ok(settled),
                }
            })
            .await?
            .as_ref()
            .map_err(Clone::clone)
    }
}

struct StagedArtwork {
    file: Stage,
    info: ImageInfo,
    resolution: ArtworkResolution,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct CoverKey {
    attempt: (u64, u64),
    album: String,
    path: PathBuf,
    resolution: ArtworkResolution,
}

#[derive(Clone)]
pub struct BookletSource {
    pub id: String,
    pub name: String,
    url: Option<Url>,
}

pub fn resolve_booklets(album: &Value, safe_mode: bool) -> Vec<BookletSource> {
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut unnamed = 0;
    album["goodies"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, entry)| {
            if entry["file_format_id"].as_u64() != Some(21)
                && entry["file_format_id"].as_str() != Some("21")
            {
                return None;
            }
            let url = secure_url(&entry["url"]);
            let key = url
                .as_ref()
                .map(|url| format!("{}{}", url.host_str().unwrap_or_default(), url.path()))
                .unwrap_or_else(|| format!("unavailable:{index}"));
            if !seen.insert(key.clone()) {
                return None;
            }
            let id = hex::encode(Sha256::digest(key.as_bytes()));
            let title = ["title", "name", "file_name"]
                .into_iter()
                .find_map(|key| entry[key].as_str().filter(|title| !title.trim().is_empty()));
            let title = title
                .map(|title| {
                    let title = title.to_lowercase();
                    sanitize(title.strip_suffix(".pdf").unwrap_or(&title), safe_mode)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join("_")
                })
                .filter(|title| !title.is_empty());
            let name = loop {
                let candidate = if let Some(title) = &title {
                    let count = counts.entry(title.clone()).or_default();
                    *count += 1;
                    if *count == 1 {
                        format!("{title}.pdf")
                    } else {
                        format!("{title}_{count:02}.pdf")
                    }
                } else {
                    unnamed += 1;
                    format!("booklet_{unnamed:02}.pdf")
                };
                if names.insert(candidate.clone()) {
                    break candidate;
                }
            };
            Some(BookletSource { id, name, url })
        })
        .collect()
}

fn secure_url(value: &Value) -> Option<Url> {
    value.as_str().and_then(crate::net_url::parse_https_url)
}

fn artwork_sources(album: &Value, ceiling: ArtworkResolution) -> Vec<(ArtworkResolution, Url)> {
    use ArtworkResolution::*;
    let image = &album["image"];
    let mut sources = std::collections::BTreeMap::new();
    for (key, size) in [
        ("original", Original),
        ("org", Original),
        ("maximum", Original),
        ("extralarge", Px1200),
        ("1200", Px1200),
        ("large", Px600),
        ("600", Px600),
        ("medium", Px300),
        ("300", Px300),
        ("small", Px300),
        ("thumbnail", Px300),
    ] {
        let Some(url) = secure_url(&image[key]) else {
            continue;
        };
        let path = url.path().to_ascii_lowercase();
        let resolution = if path.contains("_org.") {
            Original
        } else if path.contains("_1200.") {
            Px1200
        } else if path.contains("_600.") {
            Px600
        } else if path.contains("_300.") || path.contains("_230.") {
            Px300
        } else {
            size
        };
        sources.entry(resolution).or_insert_with(Vec::new).push(url);
    }
    let base = sources
        .iter()
        .rev()
        .flat_map(|(_, urls)| urls)
        .find(|url| {
            crate::net_url::is_qobuz_host(url)
                && url
                    .path()
                    .rsplit_once('.')
                    .and_then(|(prefix, _)| prefix.rsplit_once('_'))
                    .is_some_and(|(_, size)| {
                        ["org", "1200", "600", "300", "230", "50"].contains(&size)
                    })
        })
        .cloned();
    if let Some(url) = base {
        for (size, suffix) in [
            (Original, "org"),
            (Px1200, "1200"),
            (Px600, "600"),
            (Px300, "300"),
        ] {
            let (path, extension) = url
                .path()
                .rsplit_once('.')
                .expect("validated Qobuz artwork path");
            let (prefix, _) = path
                .rsplit_once('_')
                .expect("validated Qobuz artwork suffix");
            let mut derived = url.clone();
            derived.set_path(&format!("{prefix}_{suffix}.{extension}"));
            sources.entry(size).or_default().push(derived);
        }
    }
    let mut seen = BTreeSet::new();
    sources
        .into_iter()
        .rev()
        .filter(|(size, _)| *size <= ceiling)
        .flat_map(|(size, urls)| urls.into_iter().map(move |url| (size, url)))
        .filter(|(_, url)| seen.insert(url.as_str().to_owned()))
        .collect()
}

pub(super) fn artwork_error() -> MediaError {
    MediaError::new(
        "artwork_unavailable",
        "No usable cover was available up to the selected resolution. Retry cover artwork.",
        true,
    )
}

pub(super) fn artwork_conflict() -> MediaError {
    MediaError::new(
        "artwork_conflict",
        "This folder contains a cover that could not be verified for this release and resolution.",
        false,
    )
}

impl Media {
    pub async fn prestage_artwork(
        &self,
        track: &ResolvedTrack,
        candidate: &Path,
        root: &Path,
        resolution: ArtworkResolution,
        basename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: &ArtworkCache,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<(), MediaError> {
        cache
            .prepare(async || {
                let root = existing_root(root).await?;
                if tokio::fs::symlink_metadata(candidate).await.is_ok()
                    && let Ok(directory) = extra_directory(candidate).await
                {
                    let basename = cover_basename(basename);
                    let _lock = self.lock_path(&directory.join(basename), cancel).await?;
                    if let Ok(Some(cover)) = self
                        .verified_artwork(track, &directory, resolution, basename, cancel)
                        .await
                    {
                        self.wait_for_drive(&root, transfer, cancel)?;
                        let file = self.store.stage(&root, ".image").await?;
                        let copied = async {
                            copy_stage(&cover.path, file.path(), cancel).await?;
                            let path = file.path().to_owned();
                            blocking(move || image_info(&path)).await
                        }
                        .await;
                        return match copied {
                            Ok(info) => Ok(StagedArtwork {
                                file,
                                info,
                                resolution: cover.resolution,
                            }),
                            Err(error) => {
                                self.store.discard(file).await;
                                Err(error)
                            }
                        };
                    }
                }
                self.stage_artwork(track, &root, resolution, transfer, cancel, &progress)
                    .await
            })
            .await
            .map(|_| ())
    }

    pub(super) async fn artwork_for_embedding(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        staging_dir: &Path,
        resolution: ArtworkResolution,
        basename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: &ArtworkCache,
    ) -> Result<PathBuf, MediaError> {
        cancel.check()?;
        validate_cover_filename(basename)?;
        let basename = cover_basename(basename);
        match tokio::fs::symlink_metadata(directory).await {
            Err(error)
                if directory.is_absolute() && error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                let directory = extra_directory(directory).await?;
                let _guard = self.lock_path(&directory.join(basename), cancel).await?;
                if let Some(cover) = self
                    .verified_artwork(track, &directory, resolution, basename, cancel)
                    .await?
                {
                    return Ok(cover.path);
                }
            }
        }
        let quiet = |_| {};
        let prepared = cache
            .prepare(async || {
                self.stage_artwork(track, staging_dir, resolution, transfer, cancel, &quiet)
                    .await
            })
            .await
            .map_err(|error| {
                if error.code == "disk_full" {
                    transfer.disk_full(error.path.as_deref().unwrap_or(staging_dir));
                }
                error
            })?;
        Ok(prepared.file.path().to_owned())
    }

    pub async fn discard_artwork(&self, cache: ArtworkCache) {
        if let Some(Ok(staged)) = cache.staged.into_inner() {
            self.store.discard(staged.file).await;
        }
    }

    pub async fn download_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        cover_filename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: Option<&ArtworkCache>,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<ArtworkOutput, MediaError> {
        cancel.check()?;
        validate_cover_filename(cover_filename)?;
        let cover_filename = cover_basename(cover_filename);
        let directory = extra_directory(directory).await?;
        let path = directory.join(cover_filename);
        let _path_lock = self.lock_path(&path, cancel).await?;
        let key = CoverKey {
            attempt: transfer.attempt_key(),
            album: track.album_id.clone(),
            path,
            resolution,
        };
        let cached = self.cover_failures.lock().unwrap().get(&key).cloned();
        if let Some(error) = cached {
            return Err(error);
        }
        let result = async {
            if let Some(cover) = self
                .verified_artwork(track, &directory, resolution, cover_filename, cancel)
                .await?
            {
                return Ok(cover);
            }
            let prepared = if let Some(cache) = cache {
                let cached = cache
                    .prepare(async || {
                        self.stage_artwork(
                            track, &directory, resolution, transfer, cancel, &progress,
                        )
                        .await
                    })
                    .await?;
                self.wait_for_drive(&directory, transfer, cancel)?;
                let copy = self.store.stage(&directory, ".image").await?;
                if let Err(error) = copy_stage(cached.file.path(), copy.path(), cancel).await {
                    self.store.discard(copy).await;
                    return Err(error);
                }
                StagedArtwork {
                    file: copy,
                    info: cached.info,
                    resolution: cached.resolution,
                }
            } else {
                self.stage_artwork(track, &directory, resolution, transfer, cancel, &progress)
                    .await?
            };
            let path = directory.join(format!("{cover_filename}.{}", prepared.info.extension));
            if let Err(error) = self.wait_for_drive(&path, transfer, cancel) {
                self.store.discard(prepared.file).await;
                return Err(error);
            }
            let reused = self
                .store
                .publish(
                    prepared.file,
                    &path,
                    &artwork_receipt(track, prepared.resolution),
                    cancel,
                )
                .await?;
            Ok(prepared.info.output(path, prepared.resolution, reused))
        }
        .await;
        if let Err(error) = &result {
            if error.code == "disk_full" {
                transfer.disk_full(error.path.as_deref().unwrap_or(&directory));
            }
            if error.code != "cancelled" && error.code != "disk_full" {
                let mut failures = self.cover_failures.lock().unwrap();
                if failures.len() >= 1024 {
                    failures.clear();
                }
                failures.insert(key, error.clone());
            }
        }
        result
    }

    async fn verified_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        cover_filename: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<ArtworkOutput>, MediaError> {
        let variants = [
            ArtworkResolution::Original,
            ArtworkResolution::Px1200,
            ArtworkResolution::Px600,
            ArtworkResolution::Px300,
        ];
        for extension in COVER_EXTENSIONS {
            let path = directory.join(format!("{cover_filename}.{extension}"));
            match tokio::fs::symlink_metadata(&path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(MediaError::disk(error)),
                Ok(_) => {}
            }
            for size in variants.into_iter().filter(|size| *size <= resolution) {
                match self
                    .store
                    .verify(&path, &artwork_receipt(track, size), cancel)
                    .await
                {
                    Ok(true) => {
                        let info = blocking({
                            let path = path.clone();
                            move || image_info(&path)
                        })
                        .await?;
                        return Ok(Some(info.output(path, size, true)));
                    }
                    Ok(false) => break,
                    Err(error) if error.code == "output_conflict" => {}
                    Err(error) => return Err(error),
                }
            }
            return Err(artwork_conflict().at_path(&path));
        }
        Ok(None)
    }

    async fn stage_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<StagedArtwork, MediaError> {
        let budget = transfer.budget();
        let mut last_error = None;
        for (size, url) in artwork_sources(&track.album, resolution) {
            cancel.check()?;
            self.wait_for_drive(directory, transfer, cancel)?;
            let stage = self.store.stage(directory, ".image").await?;
            let prepared = async {
                self.stream_asset(&url, &stage, ART_LIMIT, transfer, &budget, cancel, progress)
                    .await?;
                let info = blocking({
                    let path = stage.path().to_owned();
                    move || image_info(&path)
                })
                .await?;
                if let Some(ceiling) = resolution.px()
                    && info
                        .dimensions
                        .is_some_and(|(width, height)| width.max(height) > ceiling)
                {
                    return Err(artwork_error());
                }
                Ok(info)
            }
            .await;
            match prepared {
                Ok(info) => {
                    return Ok(StagedArtwork {
                        file: stage,
                        info,
                        resolution: size,
                    });
                }
                Err(error) => {
                    self.store.discard(stage).await;
                    if error.code != "artwork_unavailable"
                        && !(error.code.starts_with("media_") && error.retryable)
                    {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(artwork_error))
    }

    pub async fn download_booklet(
        &self,
        source: &BookletSource,
        album_id: &str,
        directory: &Path,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<PathBuf, MediaError> {
        let result = self
            .fetch_booklet(source, album_id, directory, transfer, cancel, &progress)
            .await;
        result.map_err(|error| {
            if error.code == "disk_full" {
                transfer.disk_full(error.path.as_deref().unwrap_or(directory));
            }
            error
        })
    }

    async fn fetch_booklet(
        &self,
        source: &BookletSource,
        album_id: &str,
        directory: &Path,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<PathBuf, MediaError> {
        cancel.check()?;
        let url = source.url.as_ref().ok_or_else(|| {
            MediaError::new(
                "booklet_unavailable",
                "Qobuz did not provide a usable URL for the booklet.",
                true,
            )
        })?;
        let directory = extra_directory(directory).await?;
        let target = directory.join(&source.name);
        let _path_lock = self.lock_path(&target, cancel).await?;
        let receipt = Receipt {
            kind: "booklet",
            identity: album_id.to_owned(),
            variant: source.id.clone(),
        };
        if self.store.verify(&target, &receipt, cancel).await? {
            return Ok(target);
        }
        self.wait_for_drive(&target, transfer, cancel)?;
        let stage = self.store.stage(&directory, ".pdf").await?;
        let budget = transfer.budget();
        let prepared = async {
            self.stream_asset(
                url,
                &stage,
                BOOKLET_LIMIT,
                transfer,
                &budget,
                cancel,
                progress,
            )
            .await?;
            blocking({
                let path = stage.path().to_owned();
                move || verify_pdf(&path)
            })
            .await
        }
        .await;
        if let Err(error) = prepared {
            self.store.discard(stage).await;
            return Err(error.at_path(&target));
        }
        self.wait_for_drive(&target, transfer, cancel)?;
        self.store.publish(stage, &target, &receipt, cancel).await?;
        Ok(target)
    }

    async fn stream_asset(
        &self,
        url: &Url,
        stage: &super::Stage,
        limit: u64,
        transfer: &TransferContext,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<(), MediaError> {
        loop {
            match self
                .stream(url, stage, limit, transfer, cancel, progress)
                .await
            {
                Err(error) if matches!(error.code, "media_network" | "media_response") => {
                    if !cancellable(cancel, async { Ok(budget.wait(&error.message).await) }).await?
                    {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }
}

fn cover_basename(input: &str) -> &str {
    let name = input.trim();
    if name.is_empty() { "cover" } else { name }
}

fn artwork_receipt(track: &ResolvedTrack, size: ArtworkResolution) -> Receipt {
    Receipt {
        kind: "artwork",
        identity: track.album_id.clone(),
        variant: size.label().into(),
    }
}
