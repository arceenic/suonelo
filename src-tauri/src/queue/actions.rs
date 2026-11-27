use crate::{
    error::{AppError, Result},
    qobuz::{CanonicalResource, ResourceKind},
    storage::{DownloadMode, MaxConnections, Settings},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

use super::{
    Batch, ClearReport, Confirmation, Extra, FileDto, FilePhase, Job, JobMode,
    Phase, Queue, QueueEntryRequest, Status, is_pending_audio,
    planning::{apply_album_plan, plan_file_rows},
    releases::{ReleaseScope, effective_settings, update_release_subtitle},
};

impl Queue {
    pub fn new(settings: &Settings) -> Self {
        Self::new_with_wake(settings, Arc::new(tokio::sync::Notify::new()))
    }

    pub fn new_with_wake(settings: &Settings, wake: Arc<tokio::sync::Notify>) -> Self {
        Self {
            effective_mode: settings.download_mode,
            requested_mode: settings.download_mode,
            saved_limit: settings.max_connections,
            wake,
            ..Self::default()
        }
    }

    pub fn settings_saved(
        &mut self,
        settings: &Settings,
        apply_live: bool,
        recovery: &crate::recovery::Recovery,
    ) {
        let policy_changed = self.requested_mode != settings.download_mode
            || self.saved_limit != settings.max_connections;
        self.requested_mode = settings.download_mode;
        self.saved_limit = settings.max_connections;
        if self.admitted.is_empty() {
            self.effective_mode = self.requested_mode;
        }
        if apply_live {
            for job in self
                .jobs
                .values_mut()
                .filter(|job| job.phase != Phase::Finished)
            {
                job.settings.max_connections = self.saved_limit;
            }
            recovery.update_media_limits(self.saved_limit.limit());
        }
        if policy_changed || apply_live {
            self.changed();
        }
    }

    pub(super) fn admission_head(&self) -> Option<u64> {
        self.jobs
            .values()
            .filter(|job| job.phase != Phase::Finished)
            .min_by_key(|job| job.row_order)
            .map(|job| job.dto.id)
    }

    pub(super) fn ordered_jobs(&self) -> Vec<&Job> {
        let mut jobs: Vec<_> = self.jobs.values().collect();
        jobs.sort_unstable_by_key(|job| job.row_order);
        jobs
    }

    pub fn set_user_pause(&mut self, paused: bool, recovery: &crate::recovery::Recovery) {
        let next = paused && self.admission_head().is_some();
        if next != self.user_paused {
            self.user_paused = next;
            recovery.set_media_paused(next);
            self.changed();
        }
    }

    pub fn reset_idle_pause(&mut self, recovery: &crate::recovery::Recovery) {
        self.set_user_pause(self.user_paused, recovery);
    }

    pub(super) fn finish_admission(&mut self, id: u64, recovery: &crate::recovery::Recovery) {
        recovery.finish_media(id);
        self.admitted.remove(&id);
        if self.admitted.is_empty() && self.effective_mode != self.requested_mode {
            self.effective_mode = self.requested_mode;
            self.wake.notify_one();
        }
    }

    pub(super) fn live_limit(&self) -> Option<MaxConnections> {
        let mut queued = self
            .jobs
            .values()
            .filter(|job| job.phase != Phase::Finished);
        if self.effective_mode == DownloadMode::Sequential {
            let head = self
                .admitted
                .first()
                .and_then(|id| self.jobs.get(id))
                .or_else(|| queued.min_by_key(|job| job.row_order));
            return Some(head.map_or(self.saved_limit, |job| job.settings.max_connections));
        }
        let limit = queued
            .next()
            .map_or(self.saved_limit, |job| job.settings.max_connections);
        queued
            .all(|job| job.settings.max_connections == limit)
            .then_some(limit)
    }
    #[cfg(test)]
    pub fn enqueue(&mut self, links: Vec<CanonicalResource>, settings: &Settings, reason: &str) {
        self.enqueue_with_quality(links, settings, reason, None);
    }

    #[cfg(test)]
    pub fn enqueue_with_quality(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) {
        self.insert(links, settings, reason, quality_override);
        self.changed();
    }

    pub fn enqueue_requests(
        &mut self,
        requests: Vec<QueueEntryRequest>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) {
        if !self
            .enqueue_grouped(requests, settings, reason, quality_override)
            .is_empty()
        {
            self.changed();
        }
    }

    pub fn enqueue_search_grouped(
        &mut self,
        requests: Vec<QueueEntryRequest>,
        settings: &Settings,
        reason: &str,
        releases: HashSet<String>,
        track_parents: HashMap<String, String>,
    ) {
        self.enqueue_grouped(requests, settings, reason, None);
        let parents = Arc::new(track_parents);
        for job in self.jobs.values_mut() {
            if job.link.key.kind == ResourceKind::Album
                && releases.contains(&job.link.key.id)
                && job.has_full_release()
                && job.settings.output == settings.output
                && job.settings.output_location == settings.output_location
            {
                job.search_coverage = Some(parents.clone());
            }
        }
        self.apply_search_coverage();
        self.changed();
    }

    #[cfg(test)]
    pub fn enqueue_search(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        releases: HashSet<String>,
        track_parents: HashMap<String, String>,
    ) {
        let requests = links
            .into_iter()
            .map(|link| {
                let mut req = QueueEntryRequest::from_link(link);
                req.search_origin = true;
                req
            })
            .collect();
        self.enqueue_search_grouped(requests, settings, reason, releases, track_parents);
    }

    pub(super) fn insert(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) {
        if links.is_empty() {
            return;
        }
        let settings = effective_settings(settings, quality_override);
        self.next_batch += 1;
        let batch = self.next_batch;
        let ids = links
            .into_iter()
            .map(|link| {
                let is_album = link.key.kind == ResourceKind::Album;
                let release_id = is_album.then(|| link.key.id.clone());
                let release_scope = is_album.then_some(ReleaseScope::Full);
                self.insert_single_job(
                    link,
                    &settings,
                    batch,
                    reason,
                    quality_override,
                    false,
                    false,
                    release_id,
                    release_scope,
                    None,
                    None,
                    None,
                )
            })
            .collect();
        self.batches.insert(
            batch,
            Batch {
                jobs: ids,
                confirmation: None,
                planning: false,
            },
        );
    }

    pub(super) fn apply_search_coverage(&mut self) -> bool {
        if !self.jobs.values().any(|job| job.search_coverage.is_some()) {
            return false;
        }
        let mut removed = BTreeSet::new();
        for parent in self.jobs.values() {
            let Some(parents) = &parent.search_coverage else { continue; };
            if !matches!(parent.phase, Phase::Eligible | Phase::Active)
                || parent.only_file.is_some() || parent.dto.mode != JobMode::Audio
            {
                continue;
            }
            let Some(plan) = &parent.plan else { continue; };
            for (id, child) in &self.jobs {
                if child.link.key.kind != ResourceKind::Track
                    || !matches!(child.phase, Phase::Waiting | Phase::Planning | Phase::Held | Phase::Eligible)
                    || child.dto.files.iter().any(|file| file.status == Status::Completed)
                    || child.settings.output_location != parent.settings.output_location
                    || child.settings.output != parent.settings.output
                    || !child.pinned.is_empty()
                {
                    continue;
                }
                let track_id = &child.link.key.id;
                if parents.get(track_id).is_some_and(|album| album != &parent.link.key.id) { continue; }
                if plan.tracks.iter().any(|track| &track.id == track_id && track.unavailable.is_none()) {
                    removed.insert(*id);
                }
            }
        }
        let changed = !removed.is_empty();
        let mut batches = BTreeSet::new();
        for id in removed {
            if let Some(job) = self.jobs.remove(&id) {
                job.cancel.cancel();
                batches.insert(job.batch);
            }
        }
        for batch in batches { self.reconsider(batch); }
        changed
    }
    pub fn retry(
        &mut self,
        id: u64,
        settings: &Settings,
        reason: &str,
        selected_file: Option<u64>,
    ) -> Result<()> {
        let job = self.jobs.get_mut(&id).ok_or_else(missing_job)?;
        if selected_file.is_none() && job.can_retry_drive() {
            job.drive_retry
                .extend(job.blocked_destination.iter().cloned());
            job.drive_retry
                .extend(job.active_drive_blocks.iter().cloned());
            self.wake.notify_one();
            return Ok(());
        }
        if job.running || job.phase != Phase::Finished {
            return Err(AppError::invalid(
                "queue",
                "Wait for the current attempt to finish before retrying.",
            ));
        }
        if let Some(file) = selected_file {
            if !job.dto.files.iter().any(|item| {
                item.id == file && item.independent_retry && item.status != Status::Completed
            }) {
                return Err(AppError::invalid(
                    "queue",
                    "This item has no independent retry action.",
                ));
            }
        } else if job.dto.status == Status::Completed && !job.has_unfinished_files() {
            return Err(AppError::invalid(
                "queue",
                "Use Download again to start a fresh attempt.",
            ));
        }
        job.playlist_only_status = selected_file
            .filter(|id| matches!(job.extras.get(id), Some(Extra::Playlist)) && job.playlist.is_some())
            .map(|_| (job.dto.status, job.dto.reason.clone()));
        self.next_batch += 1;
        job.batch = self.next_batch;
        job.generation += 1;
        job.phase = Phase::Waiting;
        job.settings = compatible_settings(job, settings);
        job.cancel = CancellationToken::new();
        job.only_file = selected_file;
        job.retrying = true;
        job.replacements.clear();
        job.approved_replacement = None;
        job.drive_retry
            .extend(job.blocked_destination.iter().cloned());
        job.artwork_failures.clear();
        job.dto.status = Status::Queued;
        job.dto.reason = Some(reason.into());
        for file in &mut job.dto.files {
            file.replacement_token = None;
            let selected = selected_file.map_or_else(|| retryable_file(file), |id| id == file.id);
            if selected {
                file.status = Status::Queued;
                file.phase = FilePhase::Queued;
                file.progress = Some(0);
                file.generation = job.generation;
                file.attempt = 0;
                file.received_bytes = 0;
                file.total_bytes = None;
                file.final_size_bytes = None;
                file.reason = None;
                file.conflict_path = None;
            }
        }
        self.batches.insert(
            job.batch,
            Batch {
                jobs: vec![id],
                confirmation: None,
                planning: false,
            },
        );
        self.changed();
        Ok(())
    }

    pub fn approve_replacement(
        &mut self, job_id: u64, file_id: u64, token: &str, settings: &Settings, reason: &str,
    ) -> Result<()> {
        let job = self.jobs.get(&job_id).ok_or_else(missing_job)?;
        let approval = job.replacements.get(&file_id).filter(|approval| approval.token == token
            && job.search_origin && job.settings.output == settings.output
            && job.settings.output_location == settings.output_location)
            .ok_or_else(|| AppError::invalid("queue", "The replacement offer is stale. Retry the track to verify the current file."))?
            .clone();
        self.retry(job_id, settings, reason, Some(file_id))?;
        self.jobs.get_mut(&job_id).expect("retried job").approved_replacement = Some((file_id, approval));
        Ok(())
    }

    pub fn release_folder_evidence(&self, job_id: u64) -> Result<Vec<crate::release_folder::Evidence>> {
        // Lexical only: filesystem canonicalization remains outside the queue lock.
        fn recorded_contains(path: &std::path::Path, root: &std::path::Path) -> bool {
            #[cfg(windows)]
            {
                use std::path::{Component, Prefix};
                let mut path = path.components();
                let mut root = root.components();
                let same_prefix = match (path.next(), root.next()) {
                    (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => {
                        match (a.kind(), b.kind()) {
                            (
                                Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                                Prefix::Disk(b) | Prefix::VerbatimDisk(b),
                            ) => a.eq_ignore_ascii_case(&b),
                            (
                                Prefix::UNC(a, s) | Prefix::VerbatimUNC(a, s),
                                Prefix::UNC(b, t) | Prefix::VerbatimUNC(b, t),
                            ) => {
                                a.as_encoded_bytes()
                                    .eq_ignore_ascii_case(b.as_encoded_bytes())
                                    && s.as_encoded_bytes()
                                        .eq_ignore_ascii_case(t.as_encoded_bytes())
                            }
                            (a, b) => a == b,
                        }
                    }
                    (a, b) => a == b,
                };
                same_prefix && root.all(|part| path.next() == Some(part))
            }
            #[cfg(not(windows))]
            {
                path.starts_with(root)
            }
        }

        let job = self.jobs.get(&job_id).ok_or_else(missing_job)?;
        Ok(job.dto
            .files
            .iter()
            .filter(|file| {
                file.kind == job.dto.mode.primary_file() && file.status == Status::Completed
            })
            .filter_map(|file| {
                let path = file.path.as_ref()?;
                let root = job
                    .asset_roots
                    .get(&file.id)
                    .or_else(|| {
                        let index = job.audio_track_index(file.id)?;
                        let track = job.plan.as_ref()?.tracks.get(index)?;
                        job.pinned.get(&track.album_id).map(|layout| &layout.album_root)
                    })
                    .or_else(|| {
                        let roots = job
                            .pinned
                            .values()
                            .filter(|layout| recorded_contains(path, &layout.album_root))
                            .collect::<Vec<_>>();
                        (roots.len() == 1).then(|| &roots[0].album_root)
                    })?;
                Some(crate::release_folder::Evidence { path: path.clone(), root: root.clone() })
            })
            .collect())
    }

    pub fn conflict_path(&self, job_id: u64, file_id: u64) -> Result<PathBuf> {
        self.jobs
            .get(&job_id)
            .and_then(|job| job.dto.files.iter().find(|file| file.id == file_id))
            .and_then(|file| file.conflict_path.clone())
            .ok_or_else(|| AppError::invalid("queue", "This file has no recorded conflict path."))
    }

    pub fn download_again(&mut self, id: u64, settings: &Settings, reason: &str) -> Result<()> {
        let job = self.jobs.get(&id).ok_or_else(missing_job)?;
        if job.phase != Phase::Finished || job.running {
            return Err(AppError::invalid("queue", "This job has not finished."));
        }
        let album_plan = job.album_row.then(|| job.plan.clone()).flatten();
        let release_id = job.release_id.clone();
        let release_scope = job.release_scope.clone();
        let release_total_tracks = job.release_total_tracks;
        let release_artist = job.release_artist.clone();
        let thumb = job.dto.thumbnail.clone();
        self.insert(vec![job.link.clone()], settings, reason, None);
        if let Some(plan) = album_plan {
            let job = self.jobs.get_mut(&self.next_id).unwrap();
            job.album_row = true;
            job.release_id = release_id;
            job.release_scope = release_scope;
            job.release_total_tracks = release_total_tracks;
            job.release_artist = release_artist;
            apply_album_plan(job, "Album", plan, None, thumb);
        }
        self.changed();
        Ok(())
    }

    pub fn cancel(&mut self, id: u64, remove: bool) -> Result<()> {
        let job = self.jobs.get_mut(&id).ok_or_else(missing_job)?;
        if !remove && job.phase == Phase::Finished {
            return Err(AppError::invalid("queue", "This job has already finished."));
        }
        let batch = job.batch;
        cancel(job);
        if remove {
            self.remove_job_row(id);
        }
        self.reconsider(batch);
        self.changed();
        Ok(())
    }

    pub fn clear_downloads(&mut self) -> ClearReport {
        let before = self.jobs.len();
        let mut affected = BTreeSet::new();
        let removed: Vec<_> = self.jobs.values().filter(|job| job.phase != Phase::Active)
            .map(|job| (job.dto.id, job.batch)).collect();
        for (id, batch) in removed {
            self.jobs[&id].cancel.cancel();
            affected.insert(batch);
            self.remove_job_row(id);
        }
        self.batches.retain(|id, batch| {
            if affected.contains(id) {
                batch.confirmation = None;
            }
            batch
                .jobs
                .retain(|job_id| self.jobs.get(job_id).is_some_and(|job| job.batch == *id));
            !batch.jobs.is_empty()
        });
        for id in affected {
            self.reconsider(id);
        }
        let report = ClearReport {
            removed: before - self.jobs.len(),
            protected: self.jobs.len(),
        };
        if report.removed > 0 {
            self.changed();
        }
        report
    }

    pub fn pending(&self) -> bool {
        !self.admitted.is_empty()
            || !self.ready_playlist_exports().is_empty()
            || self
                .jobs
                .values()
                .any(|job| job.phase != Phase::Finished || job.running)
    }

    pub fn active(&self) -> bool {
        !self.admitted.is_empty()
            || self
                .jobs
                .values()
                .any(|job| matches!(job.phase, Phase::Planning | Phase::Active))
    }

    pub fn account_changed(&mut self, reason: &str, cancel_active: bool) {
        for job in self.jobs.values_mut() {
            if cancel_active {
                job.approved_replacement = None;
                job.replacements.clear();
                for file in &mut job.dto.files { file.replacement_token = None; }
            }
            match job.phase {
                Phase::Active if cancel_active => cancel(job),
                Phase::Waiting | Phase::Held | Phase::Eligible | Phase::Planning => {
                    if job.phase == Phase::Planning {
                        job.cancel.cancel();
                        job.cancel = CancellationToken::new();
                        job.generation += 1;
                        job.running = false;
                    }
                    job.phase = Phase::Waiting;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some(reason.into());
                    if !job.retrying && !job.album_row {
                        job.plan = None;
                        job.dto.files.clear();
                        job.extras.clear();
                    }
                }
                _ => (),
            }
        }
        for batch in self.batches.values_mut() {
            batch.confirmation = None;
        }
        self.changed();
    }

    pub fn apply(&mut self, ids: Option<Vec<u64>>, settings: &Settings) {
        let mut changed_batches = BTreeSet::new();
        for (id, job) in &mut self.jobs {
            if !matches!(job.phase, Phase::Active | Phase::Finished)
                && ids.as_ref().is_none_or(|ids| ids.contains(id))
            {
                let mut next = if job.retrying {
                    compatible_settings(job, settings)
                } else {
                    settings.clone()
                };
                next.output.quality = settings.output.quality;
                job.quality_override = None;
                if !job.retrying
                    && (JobMode::from_settings(&next) != job.dto.mode
                        || (!job.album_row && (next.output.limited_to_album_releases
                            != job.settings.output.limited_to_album_releases
                            || next.output.smart_discography != job.settings.output.smart_discography)))
                {
                    changed_batches.insert(job.batch);
                    job.cancel.cancel();
                    job.cancel = CancellationToken::new();
                    job.generation += 1;
                    job.running = false;
                    job.extras.clear();
                    job.dto.files.clear();
                    job.dto.mode = JobMode::from_settings(&next);
                    if job.album_row {
                        job.settings = next.clone();
                        if let Some(plan) = job.plan.clone() {
                            plan_file_rows(job, &plan);
                            update_release_subtitle(job);
                        }
                    } else {
                        job.plan = None;
                    }
                    job.phase = Phase::Waiting;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some("Waiting to refresh the submission plan.".into());
                }
                job.settings = next;
            }
        }
        for batch_id in changed_batches {
            if let Some(batch) = self.batches.get_mut(&batch_id) {
                batch.confirmation = None;
                for id in &batch.jobs {
                    if let Some(job) = self
                        .jobs
                        .get_mut(id)
                        .filter(|job| job.batch == batch_id && job.phase == Phase::Eligible)
                    {
                        job.phase = Phase::Held;
                        job.dto.reason = Some("Waiting for the submission plan.".into());
                    }
                }
            }
        }
        self.changed();
    }

    pub fn decide(&mut self, id: u64, plan_revision: u64, decision: bool) -> Result<()> {
        let batch = self.batches.get_mut(&id).ok_or_else(stale_plan)?;
        if !batch
            .confirmation
            .as_ref()
            .is_some_and(|plan| plan.plan_revision == plan_revision)
        {
            return Err(stale_plan());
        }
        batch.confirmation = None;
        for job_id in &batch.jobs {
            let Some(job) = self.jobs.get_mut(job_id).filter(|job| job.batch == id) else {
                continue;
            };
            if decision {
                if job.phase == Phase::Held {
                    job.phase = Phase::Eligible;
                    job.dto.reason = Some("Waiting to start.".into());
                }
            } else {
                cancel(job);
            }
        }
        self.changed();
        Ok(())
    }

    pub fn shutdown(&mut self) {
        for job in self.jobs.values_mut() {
            job.cancel.cancel();
        }
        self.jobs.clear();
        self.batches.clear();
        self.changed();
    }

    pub(super) fn reconsider(&mut self, batch_id: u64) {
        let Some(batch) = self.batches.get_mut(&batch_id) else {
            return;
        };
        if batch.planning {
            return;
        }
        let jobs = &batch.jobs;
        let held = || {
            jobs.iter()
                .filter_map(|id| self.jobs.get(id))
                .filter(|job| job.batch == batch_id && job.phase == Phase::Held)
        };
        if jobs.iter().filter_map(|id| self.jobs.get(id)).any(|job| {
            job.batch == batch_id && matches!(job.phase, Phase::Waiting | Phase::Planning)
        }) {
            return;
        }
        let tracks = held()
            .filter(|job| job.dto.mode == JobMode::Audio)
            .map(|job| {
                job.dto
                    .files
                    .iter()
                    .filter(|file| is_pending_audio(job, file))
                    .count()
            })
            .sum::<usize>();
        let mut pdfs = BTreeSet::new();
        for job in held().filter(|job| job.dto.mode == JobMode::Booklet) {
            if let Some(plan) = &job.plan {
                for (id, extra) in &job.extras {
                    if let Extra::Booklet { track, source, .. } = extra
                        && job.only_file.is_none_or(|selected| *id == selected)
                        && job
                            .dto
                            .files
                            .iter()
                            .any(|file| file.id == *id && file.status != Status::Completed)
                    {
                        pdfs.insert((plan.tracks[*track].album_id.as_str(), source.id.as_str()));
                    }
                }
            }
        }
        let (count, unit) = if tracks >= 100 || pdfs.len() < 100 {
            (tracks, "tracks")
        } else {
            (pdfs.len(), "pdfs")
        };
        batch.confirmation = if count >= 100 {
            Some(Confirmation {
                id: batch_id,
                plan_revision: self.revision + 1,
                track_count: count,
                unit,
            })
        } else {
            for id in jobs {
                if let Some(job) = self
                    .jobs
                    .get_mut(id)
                    .filter(|j| j.batch == batch_id && j.phase == Phase::Held)
                {
                    job.phase = Phase::Eligible;
                    job.dto.reason = Some("Waiting to start.".into());
                }
            }
            None
        };
    }
}

pub(super) fn cancel(job: &mut Job) {
    job.cancel.cancel();
    job.pending_additions = None;
    job.resolving_additions = false;
    job.phase = Phase::Finished;
    (job.dto.status, job.dto.reason) = job.playlist_only_status.take()
        .unwrap_or((Status::Cancelled, Some("Cancelled.".into())));
    for file in &mut job.dto.files {
        if matches!(
            file.status,
            Status::Downloading | Status::Queued | Status::Retrying
        ) {
            file.status = Status::Cancelled;
            file.phase = FilePhase::Cancelled;
            file.reason = Some("Cancelled.".into());
        }
    }
}

pub(super) fn compatible_settings(job: &Job, current: &Settings) -> Settings {
    let mut next = current.clone();
    next.output.booklet_only = job.dto.mode == JobMode::Booklet;
    next.output.limited_to_album_releases = job.settings.output.limited_to_album_releases;
    next.output.smart_discography = job.settings.output.smart_discography;
    next.output.lyrics_mode = job.settings.output.lyrics_mode;
    next.output.download_cover_art = job.settings.output.download_cover_art;
    next.output.create_playlist = job.settings.output.create_playlist;
    if let Some(quality) = job.quality_override {
        next.output.quality = quality;
    }
    next
}

pub(super) fn retryable_file(file: &FileDto) -> bool {
    !matches!(file.status, Status::Completed | Status::Unavailable)
}

pub(super) fn missing_job() -> AppError {
    AppError::invalid("queue", "This job is no longer in the queue.")
}
pub(super) fn stale_plan() -> AppError {
    AppError::new(
        "stale_plan",
        "queue",
        "The submission changed. Review the current count.",
        true,
    )
}
