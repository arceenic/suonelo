use crate::{
    app::App,
    media::{self, ArtworkCache, AudioRequest, LyricsMode},
    qobuz::Credentials,
    recovery::{CONCURRENT_MEDIA_LIMIT, SEQUENTIAL_MEDIA_LIMIT, TransferContext},
    search::activity::Credit,
    storage::DownloadMode,
};
use futures_util::{Stream, StreamExt, stream};
use std::{future::Future, path::{Path, PathBuf}, sync::Arc};
use tokio_util::sync::CancellationToken;

use super::{
    Extra, FileDto, FileKind, FilePhase, Job, JobMode, Phase, Status, drive_full_reason,
    mark_file_unavailable,
    actions::cancel,
    dto::{aggregate, lyrics_label, quality_label},
    planning::{fallback_sequence, lyric_file, plan_assets_for, refresh_cover_warnings, release_groups},
    playlists::write_playlist,
    scheduler::RunTask,
    telemetry::{progress, transfer_context},
};

pub(super) fn update_job<R>(app: &App, task: &RunTask, operation: impl FnOnce(&mut Job) -> R) -> Option<R> {
    let mut state = app.state();
    let queue = &mut state.queue;
    let job = queue
        .jobs
        .get_mut(&task.id)
        .filter(|job| job.generation == task.generation)?;
    let value = operation(job);
    queue.changed();
    Some(value)
}

pub(super) fn update_file(job: &mut Job, id: u64, operation: impl FnOnce(&mut FileDto)) {
    if let Some(file) = job.dto.files.iter_mut().find(|file| file.id == id) {
        file.generation = job.generation;
        operation(file);
    }
}


pub(super) async fn run_file_stage<T, F: Future<Output = std::result::Result<T, media::MediaError>>>(
    app: &App,
    task: &RunTask,
    id: u64,
    transfer: &TransferContext,
    mut work: impl FnMut() -> F,
) -> std::result::Result<T, media::MediaError> {
    loop {
        if !transfer.wait_for_media(&task.cancel).await {
            return Err(media::MediaError::cancelled());
        }
        let result = work().await;
        let Err(error) = &result else {
            return result;
        };
        if error.code != "disk_full" || task.cancel.is_cancelled() {
            return result;
        }
        let destination = error.path.as_deref().unwrap_or(&task.root);
        transfer.disk_full(destination);
        let Some(drive) = app.qobuz.recovery().drive_blocked(destination) else {
            return result;
        };
        let reason = drive_full_reason(&drive);
        update_job(app, task, |job| {
            job.active_drive_blocks.insert(destination.to_owned());
            job.blocked_destination = Some(destination.to_owned());
            job.dto.drive_blocked = true;
            job.dto.reason = Some(reason.clone());
            update_file(job, id, |file| {
                file.status = Status::Retrying;
                file.phase = FilePhase::Retrying;
                file.reason = Some(reason);
            });
        });
        if !transfer
            .wait_for_drive_release(destination, &task.cancel)
            .await
        {
            return Err(media::MediaError::cancelled());
        }
        update_job(app, task, |job| {
            job.active_drive_blocks.remove(destination);
            job.blocked_destination = job.active_drive_blocks.first().cloned();
            if job.active_drive_blocks.is_empty() {
                job.dto.drive_blocked = false;
                job.dto.reason = None;
            }
            update_file(job, id, |file| {
                file.status = Status::Downloading;
                file.phase = FilePhase::Connecting;
                file.reason = None;
            });
        });
    }
}

pub(super) async fn run_job(app: Arc<App>, account: Credentials, task: RunTask) {
    let groups = release_groups(&task.plan);
    let group_limit = if task.mode == DownloadMode::Sequential {
        1
    } else {
        CONCURRENT_MEDIA_LIMIT
    };
    stream::iter(groups)
        .for_each_concurrent(group_limit, |(album, tracks)| {
            run_release(&app, &task, &account, album, tracks)
        })
        .await;
    app.state().queue.remember_playlist_paths(task.id);
    write_playlist(&app, &task).await;
    if task.cancel.is_cancelled() {
        app.media.discard_retained_jobs(&[task.id]).await;
    }
    let mut state = app.state();
    let queue = &mut state.queue;
    queue.finish_admission(task.id, app.qobuz.recovery());
    if let Some(job) = queue
        .jobs
        .get_mut(&task.id)
        .filter(|job| job.generation == task.generation)
    {
        job.running = false;
        job.active_drive_blocks.clear();
        job.dto.drive_blocked = false;
        if task.cancel.is_cancelled() {
            if job.phase != Phase::Finished {
                cancel(job);
            }
            queue.reset_idle_pause(app.qobuz.recovery());
            queue.changed();
            return;
        }
        if job.resolving_additions {
            job.phase = Phase::Planning;
            job.dto.status = Status::Downloading;
            job.dto.reason = Some("Resolving newly added tracks…".into());
            queue.reset_idle_pause(app.qobuz.recovery());
            queue.changed();
            return;
        }
        if queue.remaining_pending_additions(task.id).is_some() {
            queue.requeue_for_additions(task.id, "Waiting to resolve newly added tracks.");
            queue.reset_idle_pause(app.qobuz.recovery());
            queue.changed();
            return;
        }
        let job = queue.jobs.get_mut(&task.id).unwrap();
        if job.has_queued_primary_files() {
            job.phase = Phase::Eligible;
            job.generation += 1;
            job.only_file = None;
            job.dto.status = Status::Queued;
            job.dto.reason = Some("Waiting to start.".into());
            for file in &mut job.dto.files {
                if file.status == Status::Queued {
                    file.generation = job.generation;
                }
            }
            queue.reset_idle_pause(app.qobuz.recovery());
            queue.changed();
            return;
        }
        finalize_job(job);
    }
    queue.reset_idle_pause(app.qobuz.recovery());
    queue.changed();
}

pub(super) fn finalize_job(job: &mut Job) {
    job.phase = Phase::Finished;
    if let Some(result) = job.playlist_only_status.take() {
        (job.dto.status, job.dto.reason) = result;
        return;
    }
    let primary = job
        .dto
        .files
        .iter()
        .filter(|file| file.kind == job.dto.mode.primary_file());
    let total = primary.clone().count();
    let finished = primary
        .filter(|file| file.status == Status::Completed)
        .count();
    job.dto.status = aggregate(finished, total);
    job.dto.reason = if finished == 0 {
        Some(if job.dto.mode == JobMode::Booklet {
            if total == 0 {
                "Qobuz returned no downloadable booklets for this resource.".into()
            } else {
                "No booklets could be finalized. Expand this row for details.".into()
            }
        } else if total == 0 {
            "This resource has no downloadable audio tracks.".into()
        } else {
            "No audio tracks could be finalized. Expand this row for details.".into()
        })
    } else {
        None
    };
}

async fn download_audio(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    index: usize,
    artwork_cache: &ArtworkCache,
    first_pass: bool,
) -> bool {
    let id = task.file_id(index);
    let track = &task.plan.tracks[index];
    if task.cancel.is_cancelled() {
        return false;
    }
    let transfer = transfer_context(app, task, id);
    if !transfer.wait_for_media(&task.cancel).await {
        return false;
    }
    let (pinned, cover_basename) = update_job(app, task, |job| {
        update_file(job, id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Connecting;
            file.reason = None;
        });
        let pinned = job.pinned.get(&track.album_id).cloned();
        let basename = pinned.as_ref().and_then(|layout| {
            job.extras.values().find_map(|extra| match extra {
                Extra::Artwork {
                    track: cover_track,
                    directory,
                    basename,
                    final_target: true,
                } if task.plan.tracks[*cover_track].album_id == track.album_id
                    && layout.cover_dir.as_ref() == Some(directory) =>
                {
                    Some(basename.clone())
                }
                _ => None,
            })
        });
        (pinned, basename)
    })
    .unwrap_or_default();
    let request = AudioRequest {
        #[cfg(target_os = "macos")]
        require_full_duration: false,
        client: &app.qobuz,
        credentials: account,
        track,
        options: &task.settings.output,
        root: &task.root,
        pinned: pinned.as_ref(),
        transfer: &transfer,
        sequence: fallback_sequence(&task.plan, index),
        replacement: task
            .replacement
            .as_ref()
            .filter(|(file_id, _)| *file_id == id && !account.access_mode.is_preview())
            .map(|(_, approval)| approval),
        artwork_cache,
        cover_basename: cover_basename
            .as_deref()
            .unwrap_or(&task.settings.output.cover_filename),
    };
    let result = run_file_stage(app, task, id, &transfer, || {
        app.media
            .download_audio(request, &task.cancel, progress(app, task, id))
    })
    .await;
    if first_pass
        && result
            .as_ref()
            .is_err_and(|error| error.code == "stream_no_response")
        && !task.cancel.is_cancelled()
    {
        update_job(app, task, |job| {
            update_file(job, id, |file| {
                file.status = Status::Retrying;
                file.phase = FilePhase::Retrying;
                file.reason =
                    Some("No response from Qobuz. Retrying after the other tracks.".into());
            })
        });
        return true;
    }
    if let Ok(output) = &result
        && let Some(at) = output.finalized_at
        && let Some(credit) = Credit::from_track(track, task.kind)
    {
        // Successful publication produced a verified file and receipt. The
        // profile cutoff still rejects finalization events older than a reset.
        let _ = app.storage.record_search_activity([&credit], 2, at);
    }
    let replacement = if let Err(error) = &result
        && error.code == "output_conflict" && task.search_origin && !account.access_mode.is_preview()
        && let Some(path) = &error.path
    {
        app.media.offer_audio_replacement(path, &track.id, &task.cancel).await.ok()
    } else {
        None
    };
    let audio_size = match &result {
        Ok(output) => final_size(&output.path).await,
        Err(_) => None,
    };
    if result
        .as_ref()
        .is_err_and(|error| error.code == "invalid_token")
    {
        app.reject_runtime_account(task.account_epoch);
    }
    update_job(app, task, |job| {
        let token = replacement.as_ref().map(|offer| offer.token.clone());
        if let Some(offer) = replacement {
            job.replacements.insert(id, offer);
        }
        update_file(job, id, |file| file.replacement_token = token);
        match result {
            Ok(output) => {
                #[cfg(not(target_os = "macos"))]
                let is_preview = account.access_mode.is_preview();
                #[cfg(target_os = "macos")]
                let is_preview = output.is_preview;
                let quality = if is_preview {
                    "MP3 preview".into()
                } else {
                    quality_label(&output.quality)
                };
                let lyrics_id = lyric_file(job, index);
                let warning = output.warning;
                let lyrics_quality = output
                    .lyrics_source
                    .map(|source| lyrics_label(source, output.lyrics_timed.unwrap_or(false)));
                update_file(job, id, |file| {
                    complete(file, output.path.clone(), Some(quality), audio_size);
                    file.reused = output.reused;
                    if lyrics_id.is_none() {
                        file.reason = warning.as_ref().map(|warning| warning.message.clone());
                        if task.settings.output.lyrics_mode == LyricsMode::Embed
                            && let Some(provider) = &lyrics_quality
                        {
                            append_reason(file, &format!("Lyrics · {provider}"));
                        }
                    }
                    if let Some(warning) = &output.cover_warning {
                        append_reason(file, &warning.message);
                    }
                });
                if task.settings.output.download_cover_art
                    && task.settings.output.embed_cover_art
                    && !output.cover_embedded
                {
                    job.missing_embedded.insert(index);
                    update_file(job, id, |file| {
                        append_reason(file, "Embedded artwork is missing.");
                    });
                } else if output.cover_embedded {
                    job.missing_embedded.remove(&index);
                }
                let cover_dir = output.layout.cover_dir.clone().unwrap_or_default();
                let cover_key = (track.album_id.clone(), cover_dir);
                if output.cover_path.is_none()
                    && let Some(warning) = output.cover_warning
                {
                    job.artwork_failures.insert(cover_key, warning);
                } else {
                    job.artwork_failures.remove(&cover_key);
                }
                job.paths.insert(index, output.path.clone());
                job.pinned.insert(track.album_id.clone(), output.layout);
                refresh_cover_warnings(job, &task.plan, &track.album_id);
                if let Some(lyrics_id) = lyrics_id {
                    job.extras.insert(
                        lyrics_id,
                        Extra::Lyrics {
                            track: index,
                            audio: output.path,
                        },
                    );
                }
            }
            Err(error) if !task.cancel.is_cancelled() => {
                if error.code == "preview_unavailable" {
                    update_file(job, id, |file| mark_file_unavailable(file, error.message));
                } else {
                    update_file(job, id, |file| file_error(file, error));
                }
                if let Some(lyrics) = lyric_file(job, index) {
                    update_file(job, lyrics, |file| {
                        fail(
                            file,
                            "Lyrics could not be saved because the audio is unfinished.".into(),
                        )
                    });
                }
            }
            Err(_) => (),
        }
    });
    false
}


fn append_reason(file: &mut FileDto, extra: &str) {
    let previous = file
        .reason
        .take()
        .map(|reason| format!("{reason} "))
        .unwrap_or_default();
    file.reason = Some(format!("{previous}{extra}"));
}

pub(super) fn release_extras(app: &App, task: &RunTask, album: &str, kind: FileKind) -> Vec<(u64, Extra)> {
    let state = app.state();
    let Some(job) = state
        .queue
        .jobs
        .get(&task.id)
        .filter(|job| job.generation == task.generation)
    else {
        return Vec::new();
    };
    job.dto
        .files
        .iter()
        .filter_map(|file| {
            if file.kind != kind || file.status != Status::Queued {
                return None;
            }
            let extra = job.extras.get(&file.id)?;
            let track = match extra {
                Extra::Artwork { track, .. } | Extra::Booklet { track, .. } => *track,
                Extra::Lyrics { track, audio } if !audio.as_os_str().is_empty() => *track,
                _ => return None,
            };
            let dependency = match kind {
                FileKind::Artwork => task
                    .audio
                    .iter()
                    .any(|index| task.plan.tracks[*index].album_id == album),
            FileKind::Lyrics => task.audio.binary_search(&track).is_ok(),
                _ => false,
            };
            (task.plan.tracks[track].album_id == album
                && task.only_file.is_none_or(|id| id == file.id || dependency))
            .then(|| (file.id, extra.clone()))
        })
        .collect()
}

async fn run_release(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    album: &str,
    tracks: Vec<usize>,
) {
    let artwork = ArtworkCache::default();
    for (id, extra) in release_extras(app, task, album, FileKind::Artwork) {
        if let Extra::Artwork {
            track,
            ref directory,
            ref basename,
            final_target: false,
        } = extra
        {
            if task.cancel.is_cancelled() {
                break;
            }
            let transfer = transfer_context(app, task, id);
            if !transfer.wait_for_media(&task.cancel).await {
                break;
            }
            update_job(app, task, |job| {
                update_file(job, id, |file| {
                    file.status = Status::Downloading;
                    file.phase = FilePhase::Connecting;
                })
            });
            let result = run_file_stage(app, task, id, &transfer, || {
                app.media.prestage_artwork(
                    &task.plan.tracks[track],
                    directory,
                    &task.root,
                    task.settings.output.artwork,
                    basename,
                    &transfer,
                    &task.cancel,
                    &artwork,
                    progress(app, task, id),
                )
            })
            .await;
            update_job(app, task, |job| {
                update_file(job, id, |file| match result {
                    Ok(()) => file.status = Status::Queued,
                    Err(error) if !task.cancel.is_cancelled() => file_error(file, error),
                    Err(_) => (),
                });
            });
        } else {
            download_extra_item(app, task, account, id, extra, Some(&artwork)).await;
        }
    }

    let mut audio = tracks
        .into_iter()
        .filter(|index| task.audio.binary_search(index).is_ok());
    let limit = if task.mode == DownloadMode::Sequential {
        SEQUENTIAL_MEDIA_LIMIT
    } else {
        CONCURRENT_MEDIA_LIMIT
    };
    let mut deferred = Vec::new();
    // The first successful audio pins the layout before other tracks write to it.
    while !task.cancel.is_cancelled() {
        let pinned = app
            .state()
            .queue
            .jobs
            .get(&task.id)
            .is_some_and(|job| job.pinned.contains_key(album));
        if pinned {
            break;
        }
        let Some(index) = audio.next() else {
            break;
        };
        if download_audio(app, task, account, index, &artwork, true).await {
            deferred.push(index);
        }
    }
    let pinned = app
        .state()
        .queue
        .jobs
        .get(&task.id)
        .is_some_and(|job| job.pinned.contains_key(album));
    if pinned {
        settle_release_cover(app, task, account, album, &artwork).await;
    }
    {
        let artwork = &artwork;
        let pending = stream::iter(audio.map(|index| async move {
            download_audio(app, task, account, index, artwork, true)
                .await
                .then_some(index)
        }))
        .buffer_unordered(limit);
        settle_audio_pass(pending, deferred, &task.cancel, |index| async move {
            let budget = crate::recovery::RetryBudget::default();
            tokio::select! {
                biased;
                _ = task.cancel.cancelled() => return,
                _ = budget.wait("") => {}
            }
            download_audio(app, task, account, index, artwork, false).await;
        })
        .await;
    }
    if !pinned {
        settle_release_cover(app, task, account, album, &artwork).await;
    }
    app.media.discard_artwork(artwork).await;
    for kind in [FileKind::Lyrics, FileKind::Booklet] {
        stream::iter(release_extras(app, task, album, kind))
            .for_each_concurrent(limit, |(id, extra)| {
                download_extra_item(app, task, account, id, extra, None)
            })
            .await;
    }
}

pub(super) async fn settle_audio_pass<F: Future<Output = ()>>(
    pending: impl Stream<Item = Option<usize>>,
    mut deferred: Vec<usize>,
    cancel: &CancellationToken,
    mut replay: impl FnMut(usize) -> F,
) {
    let mut pending = std::pin::pin!(pending);
    while let Some(index) = pending.next().await {
        deferred.extend(index);
    }
    deferred.sort_unstable();
    for index in deferred {
        if cancel.is_cancelled() {
            break;
        }
        replay(index).await;
    }
}

async fn settle_release_cover(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    album: &str,
    artwork: &ArtworkCache,
) {
    if task.cancel.is_cancelled() {
        return;
    }
    update_job(app, task, |job| {
        if task.only_file.is_none() || !task.audio.is_empty() {
            plan_assets_for(job, task, Some(album));
        }
    });
    for (id, extra) in release_extras(app, task, album, FileKind::Artwork) {
        if matches!(
            extra,
            Extra::Artwork {
                final_target: false,
                ..
            }
        ) {
            update_job(app, task, |job| {
                update_file(job, id, |file| mark_file_unavailable(file, "The cover has no verified output folder."))
            });
        } else {
            download_extra_item(app, task, account, id, extra, Some(artwork)).await;
        }
    }
}

async fn download_extra_item(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    id: u64,
    extra: Extra,
    artwork_cache: Option<&ArtworkCache>,
) {
    match extra {
        Extra::Artwork {
            track,
            directory,
            basename,
            final_target: true,
        } => {
            download_artwork(
                app,
                task,
                id,
                track,
                directory,
                basename,
                artwork_cache,
            )
            .await;
        }
        Extra::Artwork {
            final_target: false,
            ..
        } => (),
        Extra::Booklet {
            track,
            directory,
            source,
        } => download_booklet(app, task, id, track, directory, source).await,
        Extra::Lyrics { track, audio } => {
            download_lyrics(app, task, account, id, track, audio).await;
        }
        Extra::Playlist => unreachable!("Playlist export uses its own stage"),
    }
}

async fn download_artwork(
    app: &Arc<App>,
    task: &RunTask,
    id: u64,
    track: usize,
    directory: PathBuf,
    basename: String,
    artwork_cache: Option<&ArtworkCache>,
) {
    let Some(transfer) = start_extra_stage(app, task, id).await else {
        return;
    };
    let transfer_ref = &transfer;
    let directory = &directory;
    let basename = &basename;
    let result = run_file_stage(app, task, id, transfer_ref, || async move {
        app.media
            .download_artwork(
                &task.plan.tracks[track],
                directory,
                task.settings.output.artwork,
                basename,
                transfer_ref,
                &task.cancel,
                artwork_cache,
                progress(app, task, id),
            )
            .await
            .map(|output| {
                let size = output
                    .width
                    .zip(output.height)
                    .map(|(width, height)| format!("{width} × {height} px"))
                    .unwrap_or_else(|| output.resolution.label().to_owned());
                (output.path, Some(size), output.reused)
            })
    })
    .await;
    finish_extra_stage(app, task, id, result, Some(track)).await;
}

async fn download_booklet(
    app: &Arc<App>,
    task: &RunTask,
    id: u64,
    track: usize,
    directory: PathBuf,
    source: media::BookletSource,
) {
    let Some(transfer) = start_extra_stage(app, task, id).await else {
        return;
    };
    let transfer_ref = &transfer;
    let directory = &directory;
    let source = &source;
    let result = run_file_stage(app, task, id, transfer_ref, || async move {
        app.media
            .download_booklet(
                source,
                &task.plan.tracks[track].album_id,
                directory,
                transfer_ref,
                &task.cancel,
                progress(app, task, id),
            )
            .await
            .map(|path| (path, None, false))
    })
    .await;
    finish_extra_stage(app, task, id, result, None).await;
}

async fn download_lyrics(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    id: u64,
    track: usize,
    audio: PathBuf,
) {
    let Some(transfer) = start_extra_stage(app, task, id).await else {
        return;
    };
    let transfer_ref = &transfer;
    let audio = &audio;
    let result = run_file_stage(app, task, id, transfer_ref, || async move {
        let artwork_cache = ArtworkCache::default();
        let request = AudioRequest {
            #[cfg(target_os = "macos")]
            require_full_duration: false,
            replacement: None,
            artwork_cache: &artwork_cache,
            cover_basename: &task.settings.output.cover_filename,
            client: &app.qobuz,
            credentials: account,
            track: &task.plan.tracks[track],
            options: &task.settings.output,
            root: &task.root,
            pinned: None,
            transfer: transfer_ref,
            sequence: fallback_sequence(&task.plan, track),
        };
        app.media
            .download_external_lyrics(request, audio, &task.cancel)
            .await
            .map(|output| (output.path, Some(lyrics_label(output.source, output.timed)), false))
    })
    .await;
    finish_extra_stage(app, task, id, result, None).await;
}

async fn start_extra_stage(
    app: &Arc<App>,
    task: &RunTask,
    id: u64,
) -> Option<TransferContext> {
    if task.cancel.is_cancelled() {
        return None;
    }
    let transfer = transfer_context(app, task, id);
    if !transfer.wait_for_media(&task.cancel).await {
        return None;
    }
    update_job(app, task, |job| {
        update_file(job, id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Connecting;
            file.reason = None;
        })
    });
    Some(transfer)
}

async fn finish_extra_stage(
    app: &Arc<App>,
    task: &RunTask,
    id: u64,
    result: std::result::Result<(PathBuf, Option<String>, bool), media::MediaError>,
    artwork: Option<usize>,
) {
    let size = match &result {
        Ok((path, _, _)) => final_size(path).await,
        Err(_) => None,
    };
    update_job(app, task, |job| {
        update_file(job, id, |file| match result {
            Ok((path, quality, reused)) => {
                complete(file, path, quality, size);
                file.reused = reused;
            }
            Err(error) if !task.cancel.is_cancelled() && file.kind == FileKind::Lyrics => {
                lyric_failure(file, Some(error))
            }
            Err(error) if !task.cancel.is_cancelled() => file_error(file, error),
            Err(_) => (),
        });
        if let Some(index) = artwork {
            refresh_cover_warnings(job, &task.plan, &task.plan.tracks[index].album_id);
        }
    });
}

pub(super) async fn final_size(path: &Path) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

pub(super) fn complete(file: &mut FileDto, path: PathBuf, quality: Option<String>, size: Option<u64>) {
    file.status = Status::Completed;
    file.progress = Some(100);
    file.phase = FilePhase::Completed;
    file.final_size_bytes = size;
    file.path = Some(path);
    file.conflict_path = None;
    file.quality = quality;
    file.reason = None;
}

pub(super) fn fail(file: &mut FileDto, reason: String) {
    file.status = Status::Failed;
    file.phase = FilePhase::Failed;
    file.reason = Some(reason);
}

pub(super) fn file_error(file: &mut FileDto, error: media::MediaError) {
    if error.code == "path_conflict"
        || error.code == "file_conflict"
        || error.code == "output_conflict"
        || error.code == "artwork_conflict"
    {
        file.conflict_path = error.path;
    }
    fail(file, error.message);
}

pub(super) fn lyric_failure(file: &mut FileDto, error: Option<media::MediaError>) {
    if error
        .as_ref()
        .is_none_or(|error| error.code == "lyrics_unavailable")
    {
        mark_file_unavailable(file, "Lyrics unavailable");
    } else if let Some(error) = error {
        file.independent_retry = error.retryable;
        file_error(file, error);
    }
}
