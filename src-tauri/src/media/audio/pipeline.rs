use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::super::{
    AudioFormat, AudioReplacement, DeliveredQuality, LyricsMode, Media, MediaError,
    ReleaseLayout, TransferProgress, blocking, cancellable, codec, copy_stage, existing_root,
    lock, metadata, plan_output,
    output_store::{CheckCancelled, Receipt, Stage, safe_directory},
};
#[cfg(target_os = "macos")]
use super::inspect::verify_full_duration;
use super::{
    AUDIO_LIMIT, AudioOutput, AudioProperties, AudioRequest, Enrichment, LyricsOutput, StagedAudio,
    StagedKey,
    inspect::{bad_audio, delivered_quality, inspect, offered_quality},
    receipt::audio_receipt,
};
use crate::qobuz::{Lyrics, StreamOffer};
use crate::recovery::RetryBudget;

async fn negotiate(
    request: AudioRequest<'_>,
    budget: &RetryBudget,
    cancel: &CancellationToken,
) -> Result<StreamOffer, MediaError> {
    cancellable(cancel, async {
        request
            .client
            .negotiate_stream(
                &request.track.id,
                request.options.quality,
                request.options.allow_fallback,
                request.credentials,
                budget,
            )
            .await
            .map_err(Into::into)
    })
    .await
}

#[cfg(target_os = "macos")]
pub(super) fn ensure_same_offer(previous: &StreamOffer, refreshed: &StreamOffer) -> Result<(), MediaError> {
    if previous.is_preview != refreshed.is_preview || previous.tier != refreshed.tier || previous.requires_full_duration != refreshed.requires_full_duration {
        return Err(MediaError::new("media_offer_changed", "Qobuz changed the audio offer. Retry the track to select its output again.", true));
    }
    Ok(())
}

pub(super) fn lyrics_unavailable() -> MediaError {
    MediaError::new(
        "lyrics_unavailable",
        "Lyrics unavailable. No usable lyrics were found for this track.",
        false,
    )
}

async fn lookup_lyrics(
    request: AudioRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Lyrics, MediaError> {
    cancellable(cancel, async {
        request
            .client
            .resolve_lyrics(request.track, request.credentials)
            .await
            .map_err(|error| {
                MediaError::new(
                    "lyrics_failed",
                    format!("Lyrics failed. {}", error.message),
                    true,
                )
            })?
            .ok_or_else(lyrics_unavailable)
    })
    .await
}

impl Media {
    pub async fn offer_audio_replacement(&self, path: &Path, track: &str, cancel: &CancellationToken) -> Result<AudioReplacement, MediaError> {
        self.store.offer_replacement(path, track, cancel).await
    }

    pub(super) async fn output_path(
        &self,
        request: AudioRequest<'_>,
        quality: &DeliveredQuality,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, ReleaseLayout), MediaError> {
        let (path, layout) = plan_output(
            request.root,
            request.options,
            request.track,
            quality,
            request.pinned,
            request.sequence,
        )?;
        let root = request
            .pinned
            .map_or(request.root, |layout| layout.album_root.as_path());
        let parent = path.parent().ok_or_else(bad_audio)?;
        if !tokio::fs::metadata(parent)
            .await
            .is_ok_and(|info| info.is_dir())
        {
            self.wait_for_drive(&path, request.transfer, cancel)?;
        }
        let directory = safe_directory(root, parent)
            .await
            .map_err(|error| error.at_path(&path))?;
        let filename = if request.credentials.access_mode.is_preview() {
            let stem = path.file_stem().and_then(OsStr::to_str).ok_or_else(bad_audio)?;
            directory.join(format!("{stem} - Preview.mp3"))
        } else {
            directory.join(path.file_name().ok_or_else(bad_audio)?)
        };
        Ok((filename, layout))
    }

    pub async fn download_audio(
        &self,
        request: AudioRequest<'_>,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<AudioOutput, MediaError> {
        let target = request
            .pinned
            .map_or(request.root, |layout| layout.album_root.as_path());
        let result = self.download_audio_inner(request, cancel, progress).await;
        result.map_err(|error| {
            if error.code == "disk_full" {
                request
                    .transfer
                    .disk_full(error.path.as_deref().unwrap_or(target));
                return error.at_path(target);
            }
            error
        })
    }

    async fn download_audio_inner(
        &self,
        request: AudioRequest<'_>,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<AudioOutput, MediaError> {
        cancel.check()?;
        if let Some(error) = &request.track.unavailable {
            return Err(error.clone().into());
        }
        self.observe_track_metadata(request.track);
        let root = existing_root(
            request
                .pinned
                .map_or(request.root, |layout| layout.album_root.as_path()),
        )
        .await?;
        // The account mode selects the Windows lock before offer negotiation.
        #[cfg(not(target_os = "macos"))]
        let key = StagedKey {
            track: request.track.id.clone(),
            root,
            tier: request.options.quality,
            preview_only: request.credentials.access_mode.is_preview(),
        };
        #[cfg(not(target_os = "macos"))]
        let _track_lock = lock(&self.track_locks, &key, cancel).await?;

        let mut budget = request.transfer.budget();
        budget.defer_no_response = request.transfer.is_sequential();
        let offer = negotiate(request, &budget, cancel).await?;

        #[cfg(not(target_os = "macos"))]
        return self
            .download_offered_audio(request, offer, key, &budget, cancel, &progress)
            .await;

        #[cfg(target_os = "macos")]
        {
            // Scope preview behavior to this offer. The saved account access mode
            // and quality preference remain unchanged for other tracks.
            let mut credentials = request.credentials.clone();
            let mut options = request.options.clone();
            if offer.is_preview {
                credentials.access_mode = crate::qobuz::AccessMode::Preview;
                options.quality = 1;
            }
            let request = AudioRequest {
                credentials: &credentials,
                options: &options,
                require_full_duration: offer.requires_full_duration,
                replacement: if offer.is_preview {
                    None
                } else {
                    request.replacement
                },
                ..request
            };
            let key = StagedKey {
                track: request.track.id.clone(),
                root,
                tier: request.options.quality,
                preview_only: offer.is_preview,
            };
            self.download_offered_audio(request, offer, key, &budget, cancel, &progress)
                .await
                .map_err(|mut error| {
                    if credentials.access_mode.is_preview() && error.code == "output_conflict" {
                        // A preview conflict never authorizes full-audio replacement.
                        error.code = "preview_output_conflict";
                    }
                    error
                })
        }
    }

    pub(super) async fn download_offered_audio(
        &self,
        request: AudioRequest<'_>,
        offer: StreamOffer,
        key: StagedKey,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<AudioOutput, MediaError> {
        #[cfg(target_os = "macos")]
        let _track_lock = lock(&self.track_locks, &key, cancel).await?;
        let observed_quality = offered_quality(&offer);
        let path_quality = observed_quality.clone().or_else(|| {
            let templates = [
                &request.options.folder_template,
                &request.options.filename_template,
            ];
            (!templates.iter().any(|template| {
                template.contains("%bit depth%") || template.contains("%sampling rate%")
            }))
            .then(|| DeliveredQuality::requested(offer.tier))
        });
        let reservation = if let Some(quality) = path_quality {
            let (path, layout) = self.output_path(request, &quality, cancel).await?;
            let guard = self.lock_path(&path, cancel).await?;
            let existing_quality = if observed_quality.is_some()
                || !tokio::fs::try_exists(&path)
                    .await
                    .map_err(MediaError::disk)?
            {
                observed_quality
            } else {
                Some(delivered_quality(
                    inspect(&path, quality.format).await.map_err(|_| {
                        MediaError::new(
                            "output_conflict",
                            "The existing file could not be verified for this output.",
                            false,
                        )
                        .at_path(&path)
                    })?,
                    offer.tier,
                )?)
            };
            if let Some(quality) = existing_quality {
                if let Some(enrichment) =
                    self.verify_audio(request, &path, &quality, cancel).await?
                {
                    drop(guard);
                    return Ok(Self::finish_audio(
                        path,
                        layout,
                        quality,
                        true,
                        #[cfg(target_os = "macos")]
                        request.credentials.access_mode.is_preview(),
                        enrichment,
                    ));
                }
            }
            Some((path, guard))
        } else {
            None
        };

        let retained = self.retained.lock().unwrap().remove(&key);
        let mut source = if let Some(source) = retained {
            let same_offer = source.tier == offer.tier
                && offered_quality(&offer).is_none_or(|quality| {
                    delivered_quality(source.properties, source.tier)
                        .is_ok_and(|delivered| delivered == quality)
                });
            if same_offer {
                source
            } else {
                self.store.discard(source.file).await;
                self.fetch_audio(request, offer, &key.root, budget, cancel, progress)
                    .await?
            }
        } else {
            self.fetch_audio(request, offer, &key.root, budget, cancel, progress)
                .await?
        };
        source.owner = request.transfer.attempt_key().0;
        let result = self
            .process_audio(request, &source, reservation, cancel)
            .await;
        if result.as_ref().is_err_and(|error| {
            matches!(
                error.code,
                "flac_timeout" | "flac_verification" | "tags" | "disk" | "disk_full"
            ) && error.retryable
                && !cancel.is_cancelled()
        }) {
            let discarded = {
                let mut retained = self.retained.lock().unwrap();
                if cancel.is_cancelled() {
                    Some(source)
                } else {
                    retained.insert(key, source)
                }
            };
            if let Some(source) = discarded {
                self.store.discard(source.file).await;
            }
        } else {
            self.store.discard(source.file).await;
        }
        result
    }

    async fn fetch_audio(
        &self,
        request: AudioRequest<'_>,
        mut offer: StreamOffer,
        root: &Path,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<StagedAudio, MediaError> {
        let mut refreshed_after_pause = false;
        loop {
            let format = if offer.tier == 1 {
                AudioFormat::Mp3
            } else {
                AudioFormat::Flac
            };
            let suffix = format!(".{}", format.extension());
            self.wait_for_drive(root, request.transfer, cancel)?;
            let file = self.store.stage(root, &suffix).await?;
            match self
                .stream(
                    &offer.url,
                    &file,
                    AUDIO_LIMIT,
                    request.transfer,
                    cancel,
                    progress,
                )
                .await
            {
                Ok(()) => match inspect(file.path(), format).await {
                    Ok(properties) => {
                        return Ok(StagedAudio {
                            file,
                            owner: request.transfer.attempt_key().0,
                            properties,
                            tier: offer.tier,
                        });
                    }
                    Err(error) => {
                        self.store.discard(file).await;
                        return Err(error);
                    }
                },
                Err(error) => {
                    self.store.discard(file).await;
                    if error.code == "media_pause_expired" && !refreshed_after_pause {
                        refreshed_after_pause = true;
                        let refreshed = negotiate(request, budget, cancel).await?;
                        #[cfg(target_os = "macos")]
                        ensure_same_offer(&offer, &refreshed)?;
                        offer = refreshed;
                        continue;
                    }
                    if !matches!(
                        error.code,
                        "media_network" | "media_response" | "media_expired"
                    ) {
                        return Err(error);
                    }
                    if !cancellable(cancel, async { Ok(budget.wait(&error.message).await) }).await?
                    {
                        return Err(error);
                    }
                    let refreshed = negotiate(request, budget, cancel).await?;
                        #[cfg(target_os = "macos")]
                        ensure_same_offer(&offer, &refreshed)?;
                        offer = refreshed;
                }
            }
        }
    }

    pub(super) async fn process_audio(
        &self,
        request: AudioRequest<'_>,
        source: &StagedAudio,
        reservation: Option<(PathBuf, OwnedMutexGuard<()>)>,
        cancel: &CancellationToken,
    ) -> Result<AudioOutput, MediaError> {
        #[cfg(target_os = "macos")]
        if request.require_full_duration {
            verify_full_duration(request.track, source.properties)?;
        }
        let quality = delivered_quality(source.properties, source.tier)?;
        let (path, layout) = self.output_path(request, &quality, cancel).await?;
        let guard = match reservation {
            Some((expected, guard)) if expected == path => guard,
            reservation => {
                drop(reservation);
                self.lock_path(&path, cancel).await?
            }
        };
        if let Some(enrichment) = self.verify_audio(request, &path, &quality, cancel).await? {
            drop(guard);
            return Ok(Self::finish_audio(
                path,
                layout,
                quality,
                true,
                #[cfg(target_os = "macos")]
                request.credentials.access_mode.is_preview(),
                enrichment,
            ));
        }

        let mut enrichment = Enrichment::default();
        if request.options.lyrics_mode == LyricsMode::Embed {
            match lookup_lyrics(request, cancel).await {
                Ok(lyrics) => enrichment.lyrics = Some(lyrics),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => enrichment.lyrics_warning = Some(error),
            }
        }

        let directory = path.parent().ok_or_else(bad_audio)?;
        if request.options.download_cover_art && request.options.embed_cover_art {
            match self
                .artwork_for_embedding(
                    request.track,
                    layout.cover_dir.as_deref().unwrap_or(Path::new("")),
                    directory,
                    request.options.artwork,
                    request.cover_basename,
                    request.transfer,
                    cancel,
                    request.artwork_cache,
                )
                .await
            {
                Ok(cover) => {
                    enrichment.cover_path = Some(cover);
                    enrichment.cover_embedded = true;
                }
                Err(error) if cancel.is_cancelled() || error.code == "disk_full" => {
                    return Err(error);
                }
                Err(error) => enrichment.cover_warning = Some(error),
            }
        }
        self.wait_for_drive(&path, request.transfer, cancel)?;
        let processing = cancellable(cancel, async {
            self.processing
                .acquire()
                .await
                .map_err(|_| MediaError::cancelled())
        })
        .await?;
        let repaired = self.repair_flac(source, directory, cancel).await?;
        let base = repaired.as_ref().map_or(source.file.path(), Stage::path);
        let result = self
            .tag_and_publish(
                request,
                &path,
                base,
                quality.clone(),
                repaired.is_some(),
                &mut enrichment,
                cancel,
            )
            .await;
        if let Some(repaired) = repaired {
            self.store.discard(repaired).await;
        }
        drop(processing);
        drop(guard);
        let reused = result?;
        Ok(Self::finish_audio(
            path,
            layout,
            quality,
            reused,
            #[cfg(target_os = "macos")]
            request.credentials.access_mode.is_preview(),
            enrichment,
        ))
    }

    pub(super) async fn repair_flac(
        &self,
        source: &StagedAudio,
        directory: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<Stage>, MediaError> {
        let AudioProperties::Flac(original) = source.properties else {
            return Ok(None);
        };
        if original.known_checksum() {
            return Ok(None);
        }
        let repaired = self.store.stage(directory, ".flac").await?;
        let prepared = async {
            codec::repair(source.file.path(), repaired.path(), cancel).await?;
            let AudioProperties::Flac(new) = inspect(repaired.path(), AudioFormat::Flac).await?
            else {
                return Err(bad_audio());
            };
            if !original.matches_pcm(new) {
                return Err(MediaError::new(
                    "flac_verification",
                    "FLAC repair changed the PCM properties or did not provide a checksum.",
                    true,
                ));
            }
            Ok(())
        }
        .await;
        match prepared {
            Ok(()) => Ok(Some(repaired)),
            Err(error) => {
                self.store.discard(repaired).await;
                Err(error)
            }
        }
    }

    pub(super) async fn tag_and_publish(
        &self,
        request: AudioRequest<'_>,
        path: &Path,
        base: &Path,
        quality: DeliveredQuality,
        repaired: bool,
        enrichment: &mut Enrichment,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let suffix = format!(".{}", quality.format.extension());
        let edited = self
            .store
            .stage(path.parent().ok_or_else(bad_audio)?, &suffix)
            .await?;
        let prepared = async {
            let expected = inspect(base, quality.format).await?;
            let mut variants = vec![(enrichment.lyrics.is_some(), enrichment.cover_embedded)];
            if enrichment.cover_embedded { variants.push((enrichment.lyrics.is_some(), false)); }
            if enrichment.lyrics.is_some() && enrichment.cover_embedded { variants.push((false, true)); }
            if enrichment.lyrics.is_some() { variants.push((false, false)); }
            let mut variants = variants.into_iter().peekable();
            while let Some((with_lyrics, with_cover)) = variants.next() {
                let lyrics = with_lyrics.then(|| enrichment.lyrics.clone()).flatten();
                let cover = with_cover.then(|| enrichment.cover_path.clone()).flatten();
                match tag_copy(request, base, &edited, quality.format, lyrics, cover, cancel).await {
                    Ok(()) => {
                        if !with_lyrics && enrichment.lyrics.take().is_some() {
                            enrichment.lyrics_warning = Some(MediaError::new("lyrics_failed", "Lyrics failed. Embedded lyrics could not be saved.", true));
                        }
                        if !with_cover && enrichment.cover_embedded {
                            enrichment.cover_embedded = false;
                            enrichment.cover_warning = Some(MediaError::new("artwork_embed", "The cover could not be embedded.", true).at_path(path));
                        }
                        break;
                    }
                    Err(error) if variants.peek().is_none() || cancel.is_cancelled() || error.code == "disk_full" => return Err(error),
                    Err(_) => {}
                }
            }
            if let AudioProperties::Flac(expected) = expected {
                let AudioProperties::Flac(final_info) = inspect(edited.path(), AudioFormat::Flac).await? else { return Err(bad_audio()); };
                if !expected.matches_pcm(final_info) {
                    return Err(MediaError::new("flac_verification", "The finalized FLAC did not retain its PCM checksum and properties after tagging.", true));
                }
                if repaired {
                    codec::verify(edited.path(), cancel).await?;
                }
            }
            self.wait_for_drive(path, request.transfer, cancel)
        }.await;
        if let Err(error) = prepared {
            self.store.discard(edited).await;
            return Err(error);
        }
        let receipt = audio_receipt(
            request,
            &quality,
            enrichment.lyrics.is_some(),
            enrichment.cover_embedded,
            enrichment
                .lyrics
                .as_ref()
                .map(|lyrics| (lyrics.source, lyrics.timed)),
        )?;
        if let Some(approval) = request.replacement {
            self.store.publish_replacement(edited, path, &receipt, approval, cancel).await
        } else {
            self.store.publish(edited, path, &receipt, cancel).await
        }
    }

    fn finish_audio(
        path: PathBuf,
        layout: ReleaseLayout,
        quality: DeliveredQuality,
        reused: bool,
        #[cfg(target_os = "macos")]
        is_preview: bool,
        enrichment: Enrichment,
    ) -> AudioOutput {
        let finalized_at = (!reused).then(crate::search::activity::event_time);
        let lyrics_timed = enrichment.lyrics.as_ref().map(|lyrics| lyrics.timed);
        let lyrics_source = enrichment.lyrics.map(|lyrics| lyrics.source);
        AudioOutput {
            #[cfg(target_os = "macos")]
            is_preview,
            finalized_at,
            path,
            layout,
            quality,
            warning: enrichment.lyrics_warning,
            lyrics_timed,
            lyrics_source,
            reused,
            cover_path: enrichment.cover_path,
            cover_warning: enrichment.cover_warning,
            cover_embedded: enrichment.cover_embedded,
        }
    }

    pub async fn download_external_lyrics(
        &self,
        request: AudioRequest<'_>,
        audio_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<LyricsOutput, MediaError> {
        let lyrics = lookup_lyrics(request, cancel).await?;
        let suffix = if lyrics.timed { ".lrc" } else { ".txt" };
        let directory = audio_path.parent().ok_or_else(bad_audio)?;
        let target = audio_path.with_extension(&suffix[1..]);
        let _path_lock = self.lock_path(&target, cancel).await?;
        let receipt = Receipt {
            kind: "lyrics",
            identity: request.track.id.clone(),
            variant: format!("{:?}:{suffix}", lyrics.source),
        };
        let result = async {
            if self.store.verify(&target, &receipt, cancel).await? {
                return Ok(());
            }
            self.wait_for_drive(&target, request.transfer, cancel)?;
            let stage = self.store.stage(directory, suffix).await?;
            if let Err(error) = tokio::fs::write(stage.path(), &lyrics.text).await {
                self.store.discard(stage).await;
                return Err(MediaError::disk(error));
            }
            self.store.publish(stage, &target, &receipt, cancel).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if error.code == "disk_full" {
                request.transfer.disk_full(&target);
            }
            let code = if error.code == "output_conflict" {
                error.code
            } else {
                "lyrics_failed"
            };
            return Err(MediaError::new(
                code,
                format!("Lyrics failed. {}", error.message),
                error.retryable,
            )
            .at_path(&target));
        }
        Ok(LyricsOutput {
            path: target,
            timed: lyrics.timed,
            source: lyrics.source,
        })
    }
}


async fn tag_copy(
    request: AudioRequest<'_>,
    base: &Path,
    edited: &Stage,
    format: AudioFormat,
    lyrics: Option<Lyrics>,
    cover: Option<PathBuf>,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    copy_stage(base, edited.path(), cancel).await?;
    let path = edited.path().to_owned();
    let track = request.track.clone();
    let options = request.options.clone();
    blocking(move || {
        metadata::write_tags(
            &path,
            format,
            &track,
            &options,
            lyrics.as_ref(),
            cover.as_deref(),
        )
    })
    .await
}
