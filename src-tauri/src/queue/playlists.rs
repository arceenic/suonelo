use crate::{
    app::App,
    qobuz::{ResolvedResource, ResourceKind},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

use super::{
    Extra, FileKind, FilePhase, Job, JobMode, Phase, Queue, Status,
    planning::{apply_album_plan, hold_planned_job, item, next_file, release_groups},
    releases::ReleaseScope,
    runner::{complete, file_error, final_size, run_file_stage, update_file, update_job},
    scheduler::RunTask,
    telemetry::transfer_context,
};

#[derive(Clone)]
pub(crate) struct PlaylistMember {
    pub(super) group: u64,
    pub(super) positions: Vec<usize>,
}

pub(crate) struct PlaylistGroup {
    pub(super) source_id: String,
    pub(super) title: String,
    pub(super) members: Vec<u64>,
    pub(super) saved: BTreeMap<usize, PathBuf>,
    pub(super) output: Option<PathBuf>,
    pub(super) pending: bool,
}

impl Queue {
    pub(super) fn install_album_rows(&mut self, id: u64, plan: &ResolvedResource) {
        let template = self.jobs[&id].clone();
        let parent_kind = template.link.key.kind;
        let playlist = parent_kind == ResourceKind::Playlist;
        let mut members = Vec::new();

        for (index, (album, positions)) in release_groups(plan).into_iter().enumerate() {
            let row_id = if index == 0 { id } else {
                self.next_id += 1;
                self.next_id
            };
            let tracks: Vec<_> = positions.iter().map(|index| plan.tracks[*index].clone()).collect();
            let metadata = &tracks[0].album;
            let title = metadata["title"].as_str().filter(|title| !title.is_empty())
                .unwrap_or(&tracks[0].title).to_owned();
            let edition = crate::qobuz::album_version(metadata);
            let artist = metadata["artist"]["name"].as_str().filter(|artist| !artist.is_empty()).map(str::to_owned);
            let total_tracks = tracks.first().and_then(|t| t.album_total_tracks());
            let scope = if playlist {
                ReleaseScope::Tracks(tracks.iter().map(|t| t.id.clone()).collect())
            } else {
                ReleaseScope::Full
            };
            let album_plan = Arc::new(ResolvedResource {
                title,
                edition,
                tracks,
                thumbnail_url: None,
                thumbnail: None,
            });
            let mut job = template.clone();
            if let Ok(link) = crate::qobuz::normalize_link(&format!("https://play.qobuz.com/album/{album}")) {
                job.link = link;
                job.dto.url.clone_from(&job.link.original_url);
            }
            job.dto.id = row_id;
            job.dto.resource_kind = ResourceKind::Album;
            job.row_order = (id, index);
            job.album_row = true;
            job.release_id = Some(album.to_owned());
            job.release_artist = artist;
            job.release_total_tracks = total_tracks;
            job.release_scope = Some(scope);
            job.playlist = playlist.then_some(PlaylistMember { group: id, positions });
            job.cancel = CancellationToken::new();
            job.activity_at = None;
            apply_album_plan(&mut job, "Album", album_plan, None, None);
            hold_planned_job(&mut job);
            self.jobs.insert(row_id, job);
            members.push(row_id);
        }

        if let Some(batch) = self.batches.get_mut(&template.batch)
            && let Some(index) = batch.jobs.iter().position(|job| *job == id)
        {
            batch.jobs.splice(index..=index, members.iter().copied());
        }
        if playlist {
            self.playlists.insert(id, PlaylistGroup {
                source_id: template.link.key.id,
                title: plan.title.clone(),
                members,
                saved: BTreeMap::new(),
                output: None,
                pending: false,
            });
        }
    }

    pub(super) fn remember_playlist_paths(&mut self, id: u64) {
        let Some(job) = self.jobs.get(&id) else { return };
        let Some(member) = &job.playlist else { return };
        let Some(group) = self.playlists.get_mut(&member.group) else { return };
        for (local, path) in &job.paths {
            if let Some(position) = member.positions.get(*local)
                && group.saved.get(position) != Some(path)
            {
                group.saved.insert(*position, path.clone());
                group.pending = true;
            }
        }
    }

    pub(super) fn playlist_owner(&self, group: &PlaylistGroup) -> Option<u64> {
        group.members.iter().copied().find(|id| {
            self.jobs.get(id).is_some_and(|job| job.dto.mode == JobMode::Audio)
        })
    }

    pub(super) fn playlist_ready(&self, group: &PlaylistGroup, running: Option<u64>) -> bool {
        group.members.iter().all(|id| {
            running == Some(*id) || self.jobs.get(id)
                .is_none_or(|job| job.phase == Phase::Finished && !job.running)
        })
    }

    pub(super) fn ready_playlist_exports(&self) -> Vec<(u64, u64)> {
        self.playlists.iter().filter_map(|(group_id, group)| {
            if !group.pending || group.saved.is_empty() || !self.playlist_ready(group, None) {
                return None;
            }
            let owner = self.playlist_owner(group)?;
            let job = &self.jobs[&owner];
            (job.settings.output.create_playlist || job.dto.files.iter().any(|file| file.kind == FileKind::Playlist))
                .then_some((*group_id, owner))
        }).collect()
    }

    pub(super) fn queue_playlist_exports(&mut self) {
        for (group_id, id) in self.ready_playlist_exports() {
            let job = self.jobs.get_mut(&id).unwrap();
            let file_id = playlist_file(job);
            update_file(job, file_id, |file| file.status = Status::Queued);
            let settings = job.settings.clone();
            if self.retry(id, &settings, "Waiting to export the playlist.", Some(file_id)).is_ok() {
                self.playlists.get_mut(&group_id).unwrap().pending = false;
            }
        }
    }

    pub(super) fn remove_job_row(&mut self, id: u64) {
        self.remember_playlist_paths(id);
        let Some(job) = self.jobs.remove(&id) else { return };
        let Some(member) = job.playlist else { return };
        let Some(group) = self.playlists.get_mut(&member.group) else { return };
        group.members.retain(|member| *member != id);
        if group.members.is_empty() {
            self.playlists.remove(&member.group);
            return;
        }
        if let Some(mut file) = job.dto.files.into_iter().find(|file| file.kind == FileKind::Playlist) {
            let members = group.members.clone();
            group.pending |= matches!(file.status, Status::Queued | Status::Cancelled | Status::Downloading | Status::Retrying);
            if let Some(owner) = members.into_iter().filter_map(|id| self.jobs.get(&id))
                .find(|job| job.dto.mode == JobMode::Audio).map(|job| job.dto.id)
            {
                let job = self.jobs.get_mut(&owner).unwrap();
                file.id = next_file(job);
                file.generation = job.generation;
                job.extras.insert(file.id, Extra::Playlist);
                job.dto.files.push(file);
            }
        }
    }
}

pub(super) fn should_write_playlist(job: &Job, task: &RunTask) -> bool {
    let pending_export = job.dto.files.iter().any(|file| {
        file.kind == FileKind::Playlist && file.status == Status::Queued
            && task.only_file.is_none_or(|id| id == file.id)
    });
    if job.playlist.is_some() {
        return pending_export;
    }
    job.link.key.kind == ResourceKind::Playlist
        && !job.paths.is_empty()
        && (pending_export || task.settings.output.create_playlist
            && task.audio.iter().any(|index| job.paths.contains_key(index)))
}

pub(super) fn playlist_file(job: &mut Job) -> u64 {
    if let Some(file) = job.dto.files.iter().find(|file| file.kind == FileKind::Playlist) {
        return file.id;
    }
    let id = next_file(job);
    let mut file = item(id, FileKind::Playlist, "Playlist export".into(), &job.plan.as_ref().unwrap().tracks[0], None);
    file.independent_retry = true;
    job.dto.files.push(file);
    job.extras.insert(id, Extra::Playlist);
    id
}

pub(super) struct PlaylistInput {
    pub(super) title: String,
    pub(super) source_id: String,
    recorded: Option<PathBuf>,
    pub(super) paths: Vec<PathBuf>,
    group: Option<u64>,
}

pub(super) fn playlist_input(queue: &mut Queue, task: &RunTask) -> Option<PlaylistInput> {
    let job = queue.jobs.get(&task.id).filter(|job| job.generation == task.generation)?;
    if task.cancel.is_cancelled() || !should_write_playlist(job, task) {
        return None;
    }
    let Some(member) = &job.playlist else {
        return Some(PlaylistInput {
            title: task.plan.title.clone(),
            source_id: job.link.key.id.clone(),
            recorded: job.playlist_path.clone(),
            paths: job.paths.values().cloned().collect(),
            group: None,
        });
    };
    let group = queue.playlists.get(&member.group)?;
    let ready = queue.playlist_owner(group) == Some(task.id) && queue.playlist_ready(group, Some(task.id));
    let group_id = member.group;
    let group = queue.playlists.get_mut(&group_id)?;
    if !ready {
        group.pending = true;
        return None;
    }
    group.pending = false;
    Some(PlaylistInput {
        title: group.title.clone(),
        source_id: group.source_id.clone(),
        recorded: group.output.clone(),
        paths: group.saved.values().cloned().collect(),
        group: Some(group_id),
    })
}

pub(super) async fn write_playlist(app: &Arc<App>, task: &RunTask) {
    if task.cancel.is_cancelled() || task.settings.output.booklet_only {
        return;
    }
    let input = playlist_input(&mut app.state().queue, task);
    let Some(input) = input else {
        update_job(app, task, |job| job.playlist_reserved = false);
        return;
    };
    let gate = transfer_context(app, task, 0);
    if !gate.wait_for_media(&task.cancel).await {
        return;
    }
    let request = update_job(app, task, |job| {
        let file_id = playlist_file(job);
        job.playlist_reserved = false;
        update_file(job, file_id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Processing;
            file.reason = None;
        });
        file_id
    });
    let Some(file_id) = request else {
        return;
    };
    let transfer = transfer_context(app, task, file_id);
    let destination = input.recorded
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(&task.root);
    let result = run_file_stage(app, task, file_id, &transfer, || async {
        app.media
            .wait_for_drive(destination, &transfer, &task.cancel)?;
        app.media
            .write_playlist(
                &input.title,
                &input.source_id,
                &task.root,
                input.recorded.as_deref(),
                &input.paths,
                task.settings.output.safe_mode,
                &task.cancel,
            )
            .await
    })
    .await;
    let size = match &result {
        Ok(path) => final_size(path).await,
        Err(_) => None,
    };
    if let (Some(group_id), Ok(path)) = (input.group, &result)
        && let Some(group) = app.state().queue.playlists.get_mut(&group_id)
    {
        group.output = Some(path.clone());
    }
    update_job(app, task, |job| match result {
        Ok(path) => {
            job.playlist_path = Some(path.clone());
            update_file(job, file_id, |file| complete(file, path, None, size));
        }
        Err(error) if !task.cancel.is_cancelled() => {
            update_file(job, file_id, |file| file_error(file, error));
        }
        Err(_) => (),
    });
}
