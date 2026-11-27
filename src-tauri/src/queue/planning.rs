use crate::{
    media::{self, DeliveredQuality, LyricsMode},
    qobuz::{ResolvedResource, ResolvedTrack},
};
use std::{collections::{BTreeSet, HashMap}, path::PathBuf, sync::Arc};

use super::{
    Extra, FileDto, FileKind, FilePhase, Job, JobMode, Phase, Status,
    mark_file_unavailable, prune_orphan_extras,
    releases::update_release_subtitle,
    runner::{file_error, update_file},
    scheduler::RunTask,
};

pub(super) fn plan_file_rows(job: &mut Job, plan: &ResolvedResource) {
    job.audio_file_ids.clear();
    match job.dto.mode {
        JobMode::Audio => {
            job.dto.files = plan
                .tracks
                .iter()
                .enumerate()
                .map(|(index, track)| {
                    let id = index as u64 + 1;
                    job.audio_file_ids.insert(index, id);
                    item(id, FileKind::Audio, track.title.clone(), track, Some(track.disc))
                })
                .collect();
        }
        JobMode::Booklet => plan_booklet_rows(job, plan),
    }
}

pub(super) fn job_subtitle(label: &str, mode: JobMode, count: usize) -> String {
    let unit = match mode {
        JobMode::Audio if count == 1 => "track",
        JobMode::Audio => "tracks",
        JobMode::Booklet => "PDFs",
    };
    format!("{label} · {count} {unit}")
}

pub(super) fn hold_planned_job(job: &mut Job) {
    job.phase = Phase::Held;
    job.dto.status = Status::Queued;
    job.dto.reason = Some("Waiting for the submission plan.".into());
    if job.dto.mode == JobMode::Booklet && job.dto.files.is_empty() {
        job.phase = Phase::Finished;
        job.dto.status = Status::Failed;
        job.dto.reason = Some("Qobuz returned no downloadable booklets for this resource.".into());
    }
}

pub(super) fn item(
    id: u64,
    kind: FileKind,
    title: String,
    track: &ResolvedTrack,
    disc: Option<u32>,
) -> FileDto {
    FileDto {
        id,
        kind,
        title,
        disc,
        status: Status::Queued,
        progress: Some(0),
        generation: 0,
        attempt: 0,
        phase: FilePhase::Queued,
        received_bytes: 0,
        total_bytes: None,
        final_size_bytes: None,
        quality: None,
        path: None,
        conflict_path: None,
        replacement_token: None,
        reason: None,
        reused: false,
        independent_retry: matches!(
            kind,
            FileKind::Audio | FileKind::Artwork | FileKind::Booklet | FileKind::Playlist
        ),
        album: crate::qobuz::format_album_title(&track.album),
        edition: crate::qobuz::album_version(&track.album),
    }
}

pub(super) fn next_file(job: &Job) -> u64 {
    job.dto
        .files
        .iter()
        .map(|file| file.id)
        .max()
        .unwrap_or_default()
        + 1
}

pub(super) fn plan_booklet_rows(job: &mut Job, plan: &ResolvedResource) {
    let mut albums = BTreeSet::new();
    for (index, track) in plan.tracks.iter().enumerate() {
        if track.unavailable.is_some() || !albums.insert(&track.album_id) {
            continue;
        }
        for source in media::resolve_booklets(&track.album, job.settings.output.safe_mode) {
            let id = next_file(job);
            job.dto.files.push(item(
                id,
                FileKind::Booklet,
                source.name.clone(),
                track,
                None,
            ));
            job.extras.insert(
                id,
                Extra::Booklet {
                    track: index,
                    directory: PathBuf::new(),
                    source,
                },
            );
        }
    }
}

pub(super) fn configure_lyrics(job: &mut Job, plan: &ResolvedResource) {
    if job.dto.mode == JobMode::Booklet
        || job.only_file.is_some_and(|id| job.extras.contains_key(&id))
    {
        return;
    }
    if job.settings.output.lyrics_mode != LyricsMode::External {
        job.dto
            .files
            .retain(|file| file.kind != FileKind::Lyrics || file.status == Status::Completed);
        prune_orphan_extras(job);
        return;
    }
    for (index, track) in plan.tracks.iter().enumerate() {
        let audio_id = job.audio_file_id(index);
        if job.only_file.is_some_and(|id| id != audio_id) {
            continue;
        }
        let Some(audio) = job.dto.files.iter().find(|file| file.id == audio_id) else {
            continue;
        };
        if audio.status != Status::Queued {
            continue;
        }
        let title = format!("Lyrics · {}", track.title);
        if lyric_file(job, index).is_none() {
            let id = next_file(job);
            job.dto
                .files
                .push(item(id, FileKind::Lyrics, title, track, Some(track.disc)));
            job.extras.insert(
                id,
                Extra::Lyrics {
                    track: index,
                    audio: PathBuf::new(),
                },
            );
        }
    }
}

pub(super) fn lyric_file(job: &Job, index: usize) -> Option<u64> {
    job.extras.iter().find_map(|(id, extra)| {
        matches!(extra, Extra::Lyrics { track, .. } if *track == index).then_some(*id)
    })
}


pub(super) fn apply_album_plan(
    job: &mut Job,
    label: &str,
    plan: Arc<ResolvedResource>,
    subtitle: Option<String>,
    thumbnail: Option<Arc<str>>,
) {
    job.dto.title.clone_from(&plan.title);
    job.dto.edition.clone_from(&plan.edition);
    job.dto.thumbnail = thumbnail;
    if let Some(first) = plan.tracks.first() {
        if job.release_artist.is_none() {
            job.release_artist = first.album_artist();
        }
        if job.release_total_tracks.is_none() {
            job.release_total_tracks = first.album_total_tracks();
        }
    }
    plan_file_rows(job, &plan);
    job.plan = Some(plan);
    if let Some(subtitle) = subtitle {
        job.dto.subtitle = subtitle;
    } else if job.release_id.is_some() || label == "Album" {
        update_release_subtitle(job);
    } else {
        job.dto.subtitle = job_subtitle(label, job.dto.mode, job.dto.files.len());
    }
}

pub(super) fn fallback_sequence(plan: &ResolvedResource, index: usize) -> usize {
    let track = &plan.tracks[index];
    if track.number > 0 {
        return track.number as usize;
    }
    let original = if track.id.is_empty() {
        index
    } else {
        plan.tracks[..=index]
            .iter()
            .position(|candidate| candidate.id == track.id)
            .unwrap_or(index)
    };
    plan.tracks[..=original]
        .iter()
        .filter(|candidate| candidate.album_id == track.album_id && candidate.disc == track.disc)
        .count()
}

pub(super) fn release_groups(plan: &ResolvedResource) -> Vec<(&str, Vec<usize>)> {
    let mut by_album = HashMap::new();
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, track) in plan.tracks.iter().enumerate() {
        let album = track.album_id.as_str();
        let next = groups.len();
        let group = *by_album.entry(album).or_insert(next);
        if group == next {
            groups.push((album, Vec::new()));
        }
        groups[group].1.push(index);
    }
    groups
}

pub(super) fn plan_assets(job: &mut Job, task: &RunTask) {
    plan_assets_for(job, task, None);
}

pub(super) fn plan_assets_for(job: &mut Job, task: &RunTask, album: Option<&str>) {
    let mut cover_albums = BTreeSet::new();
    let mut booklet_dirs = BTreeSet::new();
    let mut proposed = Vec::new();
    let requested = DeliveredQuality::requested(task.settings.output.quality);
    for (index, track) in task.plan.tracks.iter().enumerate() {
        if track.unavailable.is_some() || album.is_some_and(|album| track.album_id != album) {
            continue;
        }
        let pinned = job.pinned.get(&track.album_id);
        let Ok((_, layout)) = media::plan_output(
            &task.root,
            &task.settings.output,
            track,
            &requested,
            pinned,
            fallback_sequence(&task.plan, index),
        ) else {
            continue;
        };
        let directory = layout.cover_dir.clone().unwrap_or_default();
        if job.dto.mode == JobMode::Audio
            && task.settings.output.download_cover_art
            && cover_albums.insert(track.album_id.as_str())
        {
            proposed.push((
                FileKind::Artwork,
                "Cover artwork".to_owned(),
                None,
                index,
                Extra::Artwork {
                    track: index,
                    directory,
                    basename: task.settings.output.cover_filename.clone(),
                    final_target: pinned.is_some(),
                },
                layout.album_root.clone(),
            ));
        }
        if booklet_dirs.insert((track.album_id.as_str(), layout.album_root.clone())) {
            for source in media::resolve_booklets(&track.album, task.settings.output.safe_mode) {
                proposed.push((
                    FileKind::Booklet,
                    source.name.clone(),
                    None,
                    index,
                    Extra::Booklet {
                        track: index,
                        directory: layout.album_root.clone(),
                        source,
                    },
                    layout.album_root.clone(),
                ));
            }
        }
    }
    let mut relevant = BTreeSet::new();
    for (kind, title, disc, index, mut extra, asset_root) in proposed {
        let existing = job.extras.iter().find_map(|(id, old)| {
            let same = match (old, &extra) {
                (Extra::Artwork { track: a, .. }, Extra::Artwork { track: b, .. }) => {
                    task.plan.tracks[*a].album_id == task.plan.tracks[*b].album_id
                }
                (
                    Extra::Booklet {
                        track: a,
                        source: x,
                        ..
                    },
                    Extra::Booklet {
                        track: b,
                        source: y,
                        ..
                    },
                ) => task.plan.tracks[*a].album_id == task.plan.tracks[*b].album_id && x.id == y.id,
                _ => false,
            };
            same.then_some(*id)
        });
        let id = existing.unwrap_or_else(|| {
            let id = next_file(job);
            job.dto
                .files
                .push(item(id, kind, title, &task.plan.tracks[index], disc));
            id
        });
        if let Some(Extra::Artwork {
            basename: previous,
            final_target: true,
            ..
        }) = job.extras.get(&id)
            && let Extra::Artwork { basename, .. } = &mut extra
        {
            basename.clone_from(previous);
        }
        if matches!(
            job.extras.get(&id),
            Some(Extra::Artwork {
                final_target: false,
                ..
            })
        ) && matches!(
            &extra,
            Extra::Artwork {
                final_target: true,
                ..
            }
        ) && let Some(row) = job
            .dto
            .files
            .iter_mut()
            .find(|row| row.id == id && row.status == Status::Unavailable)
        {
            *row = item(
                id,
                FileKind::Artwork,
                "Cover artwork".into(),
                &task.plan.tracks[index],
                None,
            );
            row.generation = job.generation;
        }
        if let Extra::Artwork {
            track, directory, ..
        } = &extra
            && let Some(error) = job
                .artwork_failures
                .get(&(task.plan.tracks[*track].album_id.clone(), directory.clone()))
                .cloned()
        {
            update_file(job, id, |file| {
                if file.status == Status::Queued {
                    file_error(file, error);
                }
            });
        }
        if job
            .dto
            .files
            .iter()
            .any(|file| file.id == id && file.status != Status::Completed)
        {
            job.asset_roots.insert(id, asset_root);
            job.extras.insert(id, extra);
        }
        relevant.insert(id);
    }
    if let Some(album) = album {
        for (id, extra) in &job.extras {
            if !relevant.contains(id)
                && matches!(extra, Extra::Artwork { track, .. } | Extra::Booklet { track, .. }
                if task.plan.tracks[*track].album_id == album)
                && let Some(file) = job.dto.files.iter_mut().find(|file| file.id == *id)
                && file.status == Status::Queued
            {
                mark_file_unavailable(file, "This file has no verified output folder.");
            }
        }
        return;
    }
    job.dto.files.retain(|file| {
        matches!(
            file.kind,
            FileKind::Audio | FileKind::Lyrics | FileKind::Playlist
        ) || relevant.contains(&file.id)
            || file.status == Status::Completed
    });
    prune_orphan_extras(job);
}

pub(super) fn refresh_cover_warnings(job: &mut Job, plan: &ResolvedResource, album: &str) {
    let Some(directory) = job
        .pinned
        .get(album)
        .and_then(|layout| layout.cover_dir.as_ref())
    else {
        return;
    };
    let count = job
        .missing_embedded
        .iter()
        .filter(|index| plan.tracks[**index].album_id == album)
        .count();
    let reason = (count > 0).then(|| {
        if count == 1 {
            "1 finalized track still lacks embedded artwork.".into()
        } else {
            format!("{count} finalized tracks still lack embedded artwork.")
        }
    });
    for file in &mut job.dto.files {
        if file.status == Status::Completed
            && matches!(job.extras.get(&file.id), Some(Extra::Artwork { track, directory: path, .. })
                if plan.tracks[*track].album_id == album && path == directory)
        {
            file.reason.clone_from(&reason);
        }
    }
}
