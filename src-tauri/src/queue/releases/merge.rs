use crate::{
    media,
    qobuz::{ResolvedResource, ResourceKind, normalize_link},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

use super::{ReleaseScope, sort_job_files, update_release_subtitle};
use crate::queue::{
    Batch, Extra, FileKind, Job, JobMode, Phase, Queue, Status,
    planning::{apply_album_plan, item, next_file},
};

impl Queue {
    pub(in crate::queue) fn merge_resolved_into_job(
        &mut self,
        target_id: u64,
        resolved: &ResolvedResource,
        search_origin: bool,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&target_id) else {
            return false;
        };
        job.search_origin |= search_origin;
        if let Some(first) = resolved.tracks.first() {
            if job.release_artist.is_none() {
                job.release_artist = first.album_artist();
            }
            if let Some(total) = first.album_total_tracks() {
                job.release_total_tracks = Some(total);
            }
            if job.release_id.is_none() && !first.album_id.is_empty() {
                job.release_id = Some(first.album_id.clone());
            }
        }
        if job.dto.thumbnail.is_none() && resolved.thumbnail.is_some() {
            job.dto.thumbnail.clone_from(&resolved.thumbnail);
        }

        if job.plan.is_none() {
            let plan = Arc::new(resolved.clone());
            job.album_row = true;
            job.dto.resource_kind = ResourceKind::Album;
            let thumb = job.dto.thumbnail.clone().or_else(|| resolved.thumbnail.clone());
            apply_album_plan(job, "Album", plan, None, thumb);
            if let Some(ReleaseScope::Tracks(ids)) = &mut job.release_scope {
                for track in &resolved.tracks {
                    ids.insert(track.id.clone());
                }
            }
            update_release_subtitle(job);
            return !job.dto.files.is_empty();
        }

        let existing_ids: BTreeSet<String> = job
            .plan
            .as_ref()
            .unwrap()
            .tracks
            .iter()
            .map(|track| track.id.clone())
            .collect();
        let new_tracks: Vec<_> = resolved
            .tracks
            .iter()
            .filter(|track| !existing_ids.contains(&track.id))
            .cloned()
            .collect();

        let added_primary = if new_tracks.is_empty() {
            false
        } else {
            append_tracks_to_job(job, new_tracks)
        };

        update_release_subtitle(job);
        if added_primary {
            match job.phase {
                Phase::Finished => {
                    job.cancel = CancellationToken::new();
                    job.generation += 1;
                    job.only_file = None;
                    job.phase = Phase::Eligible;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some("Waiting to start.".into());
                    for file in &mut job.dto.files {
                        if file.status == Status::Queued {
                            file.generation = job.generation;
                        }
                    }
                }
                Phase::Eligible => {
                    job.only_file = None;
                }
                Phase::Waiting | Phase::Planning | Phase::Held | Phase::Active => {}
            }
        }
        added_primary
    }

    pub(in crate::queue) fn promote_track_to_release(
        &mut self,
        task_id: u64,
        plan: Arc<ResolvedResource>,
    ) -> Option<u64> {
        let first = plan.tracks.first()?;
        let album_id = first.album_id.clone();
        if album_id.is_empty() {
            return None;
        }
        let (settings, mode, search_origin, batch_id) = {
            let job = self.jobs.get(&task_id)?;
            (
                job.settings.clone(),
                job.dto.mode,
                job.search_origin,
                job.batch,
            )
        };

        if let Some(target_id) =
            self.find_compatible_release_job(&album_id, &settings, mode, Some(task_id))
        {
            self.merge_resolved_into_job(target_id, &plan, search_origin);
            self.jobs.remove(&task_id);
            detach_from_batch(&mut self.batches, Some(batch_id), task_id);
            return Some(target_id);
        }

        let job = self.jobs.get_mut(&task_id)?;
        if let Ok(link) = normalize_link(&format!("https://play.qobuz.com/album/{album_id}")) {
            job.link = link;
            job.dto.url.clone_from(&job.link.original_url);
        }
        let title = first.album_title().unwrap_or_else(|| plan.title.clone());
        let artist = first.album_artist();
        let total = first.album_total_tracks();
        let track_ids: BTreeSet<String> = plan.tracks.iter().map(|t| t.id.clone()).collect();
        let album_plan = Arc::new(ResolvedResource {
            title,
            edition: plan.edition.clone(),
            tracks: plan.tracks.clone(),
            thumbnail_url: plan.thumbnail_url.clone(),
            thumbnail: plan.thumbnail.clone(),
        });
        job.dto.resource_kind = ResourceKind::Album;
        job.album_row = true;
        job.release_id = Some(album_id);
        job.release_artist = artist;
        job.release_total_tracks = total;
        job.release_scope = Some(if total.is_some_and(|t| t > 0 && track_ids.len() >= t) {
            ReleaseScope::Full
        } else {
            ReleaseScope::Tracks(track_ids)
        });
        apply_album_plan(job, "Album", album_plan, None, plan.thumbnail.clone());
        None
    }

    pub(in crate::queue) fn remaining_pending_additions(&mut self, id: u64) -> Option<ReleaseScope> {
        let job = self.jobs.get_mut(&id)?;
        let pending = job.pending_additions.take()?;
        let filtered = filter_needed_scope(job, pending);
        job.pending_additions = filtered.clone();
        filtered
    }

    pub(in crate::queue) fn requeue_for_additions(&mut self, id: u64, reason: &str) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        let old_batch = Some(job.batch);
        self.next_batch += 1;
        let batch = self.next_batch;
        job.batch = batch;
        job.generation += 1;
        job.cancel = CancellationToken::new();
        job.only_file = None;
        job.running = false;
        job.phase = Phase::Waiting;
        job.dto.status = Status::Queued;
        job.dto.reason = Some(reason.into());
        detach_from_batch(&mut self.batches, old_batch, id);
        self.batches.insert(
            batch,
            Batch {
                jobs: vec![id],
                confirmation: None,
                planning: false,
            },
        );
    }
}

fn append_tracks_to_job(job: &mut Job, new_tracks: Vec<crate::qobuz::ResolvedTrack>) -> bool {
    let mut appended = Vec::with_capacity(new_tracks.len());
    {
        let plan = Arc::make_mut(job.plan.as_mut().unwrap());
        for track in new_tracks {
            let index = plan.tracks.len();
            plan.tracks.push(track);
            appended.push(index);
        }
    }
    let plan = job.plan.clone().unwrap();
    let mut added_primary = false;
    match job.dto.mode {
        JobMode::Audio => {
            for index in appended {
                let track = &plan.tracks[index];
                let file_id = next_file(job);
                let mut file = item(
                    file_id,
                    FileKind::Audio,
                    track.title.clone(),
                    track,
                    Some(track.disc),
                );
                file.generation = job.generation;
                job.audio_file_ids.insert(index, file_id);
                job.dto.files.push(file);
                if let Some(ReleaseScope::Tracks(ids)) = &mut job.release_scope {
                    ids.insert(track.id.clone());
                }
                added_primary = true;
            }
            sort_job_files(job);
        }
        JobMode::Booklet => {
            let existing_booklets: BTreeSet<(String, String)> = job
                .extras
                .values()
                .filter_map(|extra| match extra {
                    Extra::Booklet { track, source, .. } => {
                        Some((plan.tracks[*track].album_id.clone(), source.id.clone()))
                    }
                    _ => None,
                })
                .collect();
            for index in appended {
                let track = &plan.tracks[index];
                if track.unavailable.is_some() {
                    continue;
                }
                for source in media::resolve_booklets(&track.album, job.settings.output.safe_mode) {
                    if existing_booklets.contains(&(track.album_id.clone(), source.id.clone())) {
                        continue;
                    }
                    let file_id = next_file(job);
                    let mut file = item(file_id, FileKind::Booklet, source.name.clone(), track, None);
                    file.generation = job.generation;
                    job.dto.files.push(file);
                    job.extras.insert(
                        file_id,
                        Extra::Booklet {
                            track: index,
                            directory: PathBuf::new(),
                            source,
                        },
                    );
                    added_primary = true;
                }
            }
        }
    }
    added_primary
}

pub(super) fn filter_needed_scope(job: &Job, scope: ReleaseScope) -> Option<ReleaseScope> {
    let Some(plan) = &job.plan else {
        return Some(scope);
    };
    match scope {
        ReleaseScope::Full => {
            if job
                .release_total_tracks
                .is_some_and(|total| total > 0 && plan.tracks.len() >= total)
            {
                None
            } else {
                Some(ReleaseScope::Full)
            }
        }
        ReleaseScope::Tracks(ids) => {
            let present: BTreeSet<&str> = plan.tracks.iter().map(|t| t.id.as_str()).collect();
            let missing: BTreeSet<String> = ids
                .into_iter()
                .filter(|id| !present.contains(id.as_str()))
                .collect();
            (!missing.is_empty()).then_some(ReleaseScope::Tracks(missing))
        }
    }
}

pub(super) fn union_pending(slot: &mut Option<ReleaseScope>, incoming: ReleaseScope) {
    match slot {
        Some(existing) => existing.union(incoming),
        None => *slot = Some(incoming),
    }
}

pub(super) fn detach_from_batch(
    batches: &mut BTreeMap<u64, Batch>,
    batch_id: Option<u64>,
    job_id: u64,
) {
    if let Some(id) = batch_id
        && let Some(batch) = batches.get_mut(&id)
    {
        batch.jobs.retain(|existing| *existing != job_id);
        batch.confirmation = None;
    }
}
