use crate::{
    app::App,
    media::AudioReplacement,
    qobuz::{Credentials, ResolvedResource, ResourceKind},
    search::activity::Credit,
    storage::{DownloadMode, Settings},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use super::{
    Extra, Job, JobMode, Phase, Queue, Status, drive_full_reason, is_pending_audio,
    actions::cancel,
    planning::{apply_album_plan, configure_lyrics, hold_planned_job, plan_assets},
    releases::ReleaseScope,
    runner::{finalize_job, run_job},
};

pub(super) struct PlanTask {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) key: crate::qobuz::ResourceKey,
    pub(super) existing: Option<Arc<ResolvedResource>>,
    pub(super) release_id: Option<String>,
    pub(super) release_scope: Option<ReleaseScope>,
    pub(super) addition_scope: Option<ReleaseScope>,
    pub(super) limited_to_album_releases: bool,
    pub(super) smart_discography: bool,
    pub(super) cancel: CancellationToken,
}

struct ActiveAdditionTask {
    id: u64,
    album_id: String,
    scope: ReleaseScope,
    limited_to_album_releases: bool,
    cancel: CancellationToken,
}

pub(super) struct RunTask {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) settings: Settings,
    pub(super) root: PathBuf,
    pub(super) plan: Arc<ResolvedResource>,
    pub(super) cancel: CancellationToken,
    pub(super) audio: Vec<usize>,
    pub(super) audio_file_ids: HashMap<usize, u64>,
    pub(super) only_file: Option<u64>,
    pub(super) account_epoch: u64,
    pub(super) search_origin: bool,
    pub(super) kind: ResourceKind,
    pub(super) replacement: Option<(u64, AudioReplacement)>,
    pub(super) mode: DownloadMode,
}

impl RunTask {
    pub(super) fn file_id(&self, index: usize) -> u64 {
        self.audio_file_ids
            .get(&index)
            .copied()
            .unwrap_or(index as u64 + 1)
    }
}

pub async fn scheduler(app: Arc<App>) {
    loop {
        if app.stop.is_cancelled() {
            break;
        }
        let scheduled = app.clone();
        let should_tick = tokio::task::spawn_blocking(move || schedule(&scheduled))
            .await
            .unwrap_or(false);
        if should_tick {
            tokio::select! {
                _ = app.stop.cancelled() => break,
                _ = app.schedule_wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
        } else {
            tokio::select! {
                _ = app.stop.cancelled() => break,
                _ = app.schedule_wake.notified() => {},
            }
        }
    }
}

pub(super) fn schedule(app: &Arc<App>) -> bool {
    let (account, epoch, interval, check_paths) = {
        let mut state = app.state();
        let Some(account) = sync_recovery_and_rate(app, &mut state) else {
            return needs_periodic_tick(app, &state.queue);
        };
        let epoch = state.account_epoch;
        let interval =
            Duration::try_from_secs_f64(state.settings.interval_seconds).unwrap_or(Duration::MAX);
        let queue = &mut state.queue;
        if queue.apply_search_coverage() {
            queue.changed();
        }
        if let Some((batch_id, tasks)) = collect_plan_tasks(queue) {
            app.tasks.spawn(resolve_batch(
                app.clone(),
                account.clone(),
                epoch,
                batch_id,
                tasks,
            ));
        }
        for addition in collect_active_addition_tasks(queue) {
            app.tasks.spawn(resolve_active_additions(
                app.clone(),
                account.clone(),
                epoch,
                addition,
            ));
        }
        let paths = collect_candidate_paths(app, queue);
        (account, epoch, interval, paths)
    };
    let valid_dirs: HashSet<PathBuf> = check_paths
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    let mut state = app.state();
    if state.closing || state.account_epoch != epoch {
        return needs_periodic_tick(app, &state.queue);
    }
    if let Some(task) = select_run_tasks(
        app,
        &mut state.queue,
        &account,
        epoch,
        interval,
        &valid_dirs,
    ) {
        app.tasks.spawn(run_job(app.clone(), account, task));
    }
    needs_periodic_tick(app, &state.queue)
}

fn needs_periodic_tick(app: &App, queue: &Queue) -> bool {
    queue.jobs.values().any(|job| job.phase != Phase::Finished)
        || queue.bytes_per_second > 0
        || app.meter.has_pending()
        || !queue.rate.is_empty()
        || queue.effective_mode != queue.requested_mode
        || app.qobuz.recovery().has_active_timer()
}

fn collect_candidate_paths(app: &App, queue: &Queue) -> HashSet<PathBuf> {
    let mut paths = HashSet::new();
    for job in queue.jobs.values().filter(|job| job.phase == Phase::Eligible) {
        if let Some(dest) = job.settings.destination(&app.root) {
            paths.insert(dest);
        }
        for layout in job.pinned.values() {
            paths.insert(layout.album_root.clone());
        }
    }
    paths
}

fn sync_recovery_and_rate(app: &Arc<App>, state: &mut crate::app::AppState) -> Option<Credentials> {
    let now = Instant::now();
    let drained = app.meter.drain();
    if drained > 0 {
        state.queue.rate.record(now, drained);
    }
    state.queue.refresh_rate(now);
    state.queue.reset_idle_pause(app.qobuz.recovery());
    let recovery = app.qobuz.recovery_status(state.account.as_ref());
    if state.queue.recovery != recovery {
        state.queue.recovery = recovery;
        state.queue.changed();
    }
    for job in state.queue.jobs.values_mut() {
        for path in std::mem::take(&mut job.drive_retry) {
            app.qobuz.recovery().release_drive(&path);
        }
    }
    if state.closing {
        return None;
    }
    #[cfg(target_os = "linux")]
    if state
        .vault_write
        .as_ref()
        .is_some_and(|write| write.authorized)
    {
        return None;
    }
    let account = state.account.clone()?;
    let api_wait = app.qobuz.api_waiting_reason(&account);
    let mut waiting_changed = false;
    for job in state
        .queue
        .jobs
        .values_mut()
        .filter(|job| job.phase == Phase::Planning)
    {
        let reason = api_wait
            .as_deref()
            .unwrap_or("Resolving Qobuz links and counting tracks…");
        let status = if api_wait.is_some() {
            Status::Queued
        } else {
            Status::Downloading
        };
        if job.dto.status != status || job.dto.reason.as_deref() != Some(reason) {
            job.dto.status = status;
            job.dto.reason = Some(reason.into());
            waiting_changed = true;
        }
    }
    if waiting_changed {
        state.queue.changed();
    }
    Some(account)
}

fn collect_plan_tasks(queue: &mut Queue) -> Option<(u64, Vec<PlanTask>)> {
    let preparing = queue
        .batches
        .values()
        .filter(|batch| batch.planning)
        .count();
    let next_batch = (preparing < 2)
        .then(|| {
            queue
                .batches
                .iter()
                .find(|(id, batch)| {
                    !batch.planning
                        && batch.jobs.iter().any(|job_id| {
                            queue
                                .jobs
                                .get(job_id)
                                .is_some_and(|job| job.batch == **id && job.phase == Phase::Waiting)
                        })
                })
                .map(|(id, _)| *id)
        })
        .flatten()?;
    let batch = queue.batches.get_mut(&next_batch).unwrap();
    batch.planning = true;
    let mut tasks = Vec::new();
    for job_id in &batch.jobs {
        let Some(job) = queue
            .jobs
            .get_mut(job_id)
            .filter(|job| job.batch == next_batch && job.phase == Phase::Waiting)
        else {
            continue;
        };
        let addition_scope = if job.plan.is_some() {
            job.pending_additions.take()
        } else {
            None
        };
        job.phase = Phase::Planning;
        job.running = true;
        job.dto.status = Status::Downloading;
        job.dto.reason = Some("Resolving Qobuz links and counting tracks…".into());
        tasks.push(PlanTask {
            id: *job_id,
            generation: job.generation,
            key: job.link.key.clone(),
            existing: job.plan.clone(),
            release_id: job.release_id.clone(),
            release_scope: job.release_scope.clone(),
            addition_scope,
            limited_to_album_releases: job.settings.output.limited_to_album_releases,
            smart_discography: job.settings.output.smart_discography,
            cancel: job.cancel.clone(),
        });
    }
    queue.changed();
    Some((next_batch, tasks))
}

fn collect_active_addition_tasks(queue: &mut Queue) -> Vec<ActiveAdditionTask> {
    let mut tasks = Vec::new();
    for job in queue.jobs.values_mut() {
        if job.phase != Phase::Active || job.resolving_additions || job.cancel.is_cancelled() {
            continue;
        }
        let Some(album_id) = job.release_id.clone() else {
            continue;
        };
        let Some(scope) = job.pending_additions.take() else {
            continue;
        };
        job.resolving_additions = true;
        tasks.push(ActiveAdditionTask {
            id: job.dto.id,
            album_id,
            scope,
            limited_to_album_releases: job.settings.output.limited_to_album_releases,
            cancel: job.cancel.clone(),
        });
    }
    tasks
}

fn select_run_tasks(
    app: &Arc<App>,
    queue: &mut Queue,
    account: &Credentials,
    epoch: u64,
    interval: Duration,
    valid_dirs: &HashSet<PathBuf>,
) -> Option<RunTask> {
    let mode = queue.effective_mode;
    let sequential = mode == DownloadMode::Sequential;
    if queue.user_paused
        || mode != queue.requested_mode
        || (sequential && !queue.admitted.is_empty())
    {
        return None;
    }
    queue.queue_playlist_exports();
    let head = queue.admission_head();
    if queue
        .last_start
        .is_some_and(|last| last.elapsed() < interval)
    {
        return None;
    }
    let mut changed = false;
    let mut task = None;
    let candidates: Vec<_> = queue
        .ordered_jobs()
        .into_iter()
        .filter(|job| job.phase == Phase::Eligible && (!sequential || head == Some(job.dto.id)))
        .map(|job| job.dto.id)
        .collect();
    let active_releases: Option<HashSet<String>> = candidates
        .iter()
        .any(|id| {
            queue
                .jobs
                .get(id)
                .is_some_and(|job| job.search_coverage.is_some())
        })
        .then(|| {
            queue
                .jobs
                .values()
                .filter(|job| {
                    job.phase == Phase::Active || (job.running && job.phase == Phase::Finished)
                })
                .filter_map(|job| job.plan.as_ref())
                .flat_map(|plan| plan.tracks.iter().map(|track| track.album_id.clone()))
                .collect()
        });
    for id in candidates {
        let job = queue.jobs.get_mut(&id).unwrap();
        if job.search_coverage.is_some()
            && active_releases
                .as_ref()
                .is_some_and(|releases| releases.contains(&job.link.key.id))
        {
            let reason = "Waiting for an overlapping transfer to finish.";
            changed |= job.dto.reason.as_deref() != Some(reason);
            job.dto.reason = Some(reason.into());
            continue;
        }
        let candidate = job.settings.destination(&app.root);
        let pinned = job.plan.as_ref().and_then(|plan| {
            let planned = plan
                .tracks
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    let audio_id = job.audio_file_id(*index);
                    job.dto
                        .files
                        .iter()
                        .any(|file| is_pending_audio(job, file) && file.id == audio_id)
                })
                .map(|(_, track)| job.pinned.get(&track.album_id));
            let pinned = planned.collect::<Option<Vec<_>>>()?;
            pinned
                .iter()
                .all(|layout| valid_dirs.contains(&layout.album_root))
                .then(|| {
                    pinned
                        .first()
                        .copied()
                        .or_else(|| job.pinned.values().next())
                        .map(|layout| layout.album_root.clone())
                })
                .flatten()
        });
        let Some(root) = candidate
            .filter(|root| valid_dirs.contains(root))
            .or(pinned)
        else {
            let reason =
                "The saved destination is missing. Choose a folder and apply it to pending jobs.";
            changed |= job.dto.reason.as_deref() != Some(reason);
            job.dto.reason = Some(reason.into());
            continue;
        };
        let mut audio = job
            .dto
            .files
            .iter()
            .filter(|file| is_pending_audio(job, file))
            .filter_map(|file| job.audio_track_index(file.id))
            .collect::<Vec<_>>();
        audio.sort_unstable();
        let audio_file_ids = audio
            .iter()
            .map(|&index| (index, job.audio_file_id(index)))
            .collect();
        let destinations = write_destinations(job, &root, &audio);
        let blocked = destinations
            .iter()
            .map(|path| {
                app.qobuz
                    .recovery()
                    .drive_blocked(path)
                    .map(|drive| (path.to_owned(), drive))
            })
            .collect::<Option<Vec<_>>>()
            .and_then(|blocked| blocked.into_iter().next());
        if let Some((path, drive)) = blocked {
            let reason = drive_full_reason(&drive);
            changed |= job.dto.reason.as_deref() != Some(reason.as_str()) || !job.dto.drive_blocked;
            job.dto.drive_blocked = true;
            job.dto.reason = Some(reason);
            job.blocked_destination = Some(path);
            continue;
        }
        if job.dto.drive_blocked {
            job.dto.drive_blocked = false;
            changed = true;
        }
        let Some(plan) = job.plan.clone() else {
            continue;
        };
        job.phase = Phase::Active;
        job.running = true;
        job.dto.status = Status::Downloading;
        job.dto.reason = None;
        job.blocked_destination = None;
        configure_lyrics(job, &plan);
        job.playlist_reserved = job.link.key.kind == ResourceKind::Playlist
            && job.dto.mode == JobMode::Audio
            && job.only_file.is_none()
            && job.settings.output.create_playlist
            && !audio.is_empty()
            && !job
                .extras
                .values()
                .any(|extra| matches!(extra, Extra::Playlist));
        let mut settings = job.settings.clone();
        if account.access_mode.is_preview() {
            settings.output.quality = 1;
        }
        let start = RunTask {
            id: job.dto.id,
            generation: job.generation,
            settings,
            root,
            plan,
            cancel: job.cancel.clone(),
            audio,
            audio_file_ids,
            only_file: job.only_file,
            account_epoch: epoch,
            search_origin: job.search_origin,
            kind: job.link.key.kind,
            replacement: job.approved_replacement.take(),
            mode,
        };
        if start.only_file.is_none()
            || !start.audio.is_empty()
            || start.only_file.is_some_and(|id| {
                matches!(
                    job.extras.get(&id),
                    Some(Extra::Artwork { .. } | Extra::Booklet { .. })
                )
            })
        {
            plan_assets(job, &start);
        }
        queue.admitted.insert(start.id);
        app.qobuz.recovery().admit_media(
            start.id,
            sequential,
            start.settings.max_connections.limit(),
        );
        task = Some(start);
        queue.last_start = Some(Instant::now());
        changed = true;
        break;
    }
    if changed {
        queue.changed();
    }
    task
}

pub(super) fn write_destinations(job: &Job, root: &Path, audio: &[usize]) -> Vec<PathBuf> {
    let mut destinations = BTreeSet::new();
    if let Some(plan) = &job.plan {
        for index in audio {
            destinations.insert(
                job.pinned
                    .get(&plan.tracks[*index].album_id)
                    .map_or(root, |layout| layout.album_root.as_path())
                    .to_owned(),
            );
        }
        for (id, extra) in &job.extras {
            if job.only_file.is_some_and(|selected| selected != *id)
                || !job
                    .dto
                    .files
                    .iter()
                    .any(|file| file.id == *id && file.status != Status::Completed)
            {
                continue;
            }
            let path = match extra {
                Extra::Artwork { track, .. } | Extra::Booklet { track, .. } => job
                    .pinned
                    .get(&plan.tracks[*track].album_id)
                    .map_or(root, |layout| layout.album_root.as_path()),
                Extra::Lyrics { audio, .. } => audio.parent().unwrap_or(root),
                Extra::Playlist => job
                    .playlist_path
                    .as_deref()
                    .and_then(Path::parent)
                    .unwrap_or(root),
            };
            destinations.insert(path.to_owned());
        }
    }
    if job.link.key.kind == ResourceKind::Playlist && job.settings.output.create_playlist {
        destinations.insert(
            job.playlist_path
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or(root)
                .to_owned(),
        );
    }
    if destinations.is_empty() {
        destinations.insert(root.to_owned());
    }
    destinations.into_iter().collect()
}

async fn resolve_plan_task(
    app: &App,
    account: &Credentials,
    task: &PlanTask,
) -> Option<std::result::Result<Arc<ResolvedResource>, crate::qobuz::QobuzError>> {
    if task.cancel.is_cancelled() {
        return None;
    }
    if let Some(scope) = &task.addition_scope
        && let Some(album_id) = task.release_id.as_deref()
    {
        return tokio::select! {
            _ = task.cancel.cancelled() => None,
            result = async {
                let mut resource = app.qobuz.resolve_album_tracks(album_id, scope.track_ids(), account, task.limited_to_album_releases).await?;
                if let Some(url) = resource.thumbnail_url.as_deref() {
                    resource.thumbnail = app.media.thumbnail(url).await;
                }
                Ok::<_, crate::qobuz::QobuzError>(resource)
            } => Some(result.map(Arc::new)),
        };
    }
    if let Some(plan) = &task.existing {
        return Some(Ok(plan.clone()));
    }
    if let Some(album_id) = task.release_id.as_deref()
        && let Some(ReleaseScope::Tracks(ids)) = &task.release_scope
    {
        return tokio::select! {
            _ = task.cancel.cancelled() => None,
            result = async {
                let mut resource = app.qobuz.resolve_album_tracks(album_id, Some(ids), account, false).await?;
                if let Some(url) = resource.thumbnail_url.as_deref() {
                    resource.thumbnail = app.media.thumbnail(url).await;
                }
                Ok::<_, crate::qobuz::QobuzError>(resource)
            } => Some(result.map(Arc::new)),
        };
    }
    tokio::select! {
        _ = task.cancel.cancelled() => None,
        result = async {
            let mut resource = app.qobuz.resolve_resource_for_job(&task.key, account, task.limited_to_album_releases, task.smart_discography).await?;
            if matches!(task.key.kind, ResourceKind::Album | ResourceKind::Track)
                && let Some(url) = resource.thumbnail_url.as_deref()
            {
                resource.thumbnail = app.media.thumbnail(url).await;
            }
            Ok::<_, crate::qobuz::QobuzError>(resource)
        } => Some(result.map(Arc::new)),
    }
}

async fn resolve_batch(
    app: Arc<App>,
    account: Credentials,
    epoch: u64,
    batch_id: u64,
    tasks: Vec<PlanTask>,
) {
    for task in tasks {
        let task_id = task.id;
        let task_generation = task.generation;
        let was_addition = task.addition_scope.is_some();
        let outcome = resolve_plan_task(&app, &account, &task).await;
        if outcome.as_ref().is_some_and(|outcome| {
            outcome
                .as_ref()
                .is_err_and(|error| error.code == "invalid_token")
        }) {
            app.reject_runtime_account(epoch);
        }
        if let Some((job_id, generation, credits, at)) =
            apply_resolve_outcome(&app, epoch, task_id, task_generation, was_addition, outcome)
            && app.storage.record_search_activity(&credits, 1, at).is_err()
        {
            let mut state = app.state();
            if let Some(job) = state
                .queue
                .jobs
                .get_mut(&job_id)
                .filter(|job| job.generation == generation)
            {
                job.activity_at = Some(at);
            }
        }
    }
    let mut state = app.state();
    let queue = &mut state.queue;
    if let Some(batch) = queue.batches.get_mut(&batch_id) {
        batch.planning = false;
    }
    queue.reconsider(batch_id);
    queue.apply_search_coverage();
    queue.changed();
}

async fn resolve_active_additions(
    app: Arc<App>,
    account: Credentials,
    epoch: u64,
    task: ActiveAdditionTask,
) {
    let outcome = if task.cancel.is_cancelled() {
        None
    } else {
        tokio::select! {
            _ = task.cancel.cancelled() => None,
            result = async {
                let mut resource = app
                    .qobuz
                    .resolve_album_tracks(
                        &task.album_id,
                        task.scope.track_ids(),
                        &account,
                        task.limited_to_album_releases,
                    )
                    .await?;
                if let Some(url) = resource.thumbnail_url.as_deref() {
                    resource.thumbnail = app.media.thumbnail(url).await;
                }
                Ok::<_, crate::qobuz::QobuzError>(resource)
            } => Some(result.map(Arc::new)),
        }
    };
    if outcome.as_ref().is_some_and(|outcome| {
        outcome
            .as_ref()
            .is_err_and(|error| error.code == "invalid_token")
    }) {
        app.reject_runtime_account(epoch);
    }
    let mut state = app.state();
    let current_epoch = state.account_epoch;
    let queue = &mut state.queue;
    let Some(job) = queue.jobs.get_mut(&task.id) else {
        return;
    };
    job.resolving_additions = false;
    if current_epoch != epoch || task.cancel.is_cancelled() {
        queue.changed();
        return;
    }
    let waiting_for_finish = job.phase == Phase::Planning;
    match outcome {
        Some(Ok(plan)) => {
            queue.merge_resolved_into_job(task.id, &plan, false);
            if waiting_for_finish {
                if queue.remaining_pending_additions(task.id).is_some() {
                    queue.requeue_for_additions(task.id, "Waiting to resolve newly added tracks.");
                } else if let Some(job) = queue.jobs.get_mut(&task.id) {
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
                    } else {
                        finalize_job(job);
                    }
                }
            }
        }
        Some(Err(error)) => {
            if waiting_for_finish && let Some(job) = queue.jobs.get_mut(&task.id) {
                finalize_job(job);
                if job.dto.status == Status::Failed {
                    job.dto.reason = Some(error.message);
                }
            }
        }
        None => {
            if waiting_for_finish && let Some(job) = queue.jobs.get_mut(&task.id) {
                cancel(job);
            }
        }
    }
    queue.changed();
}

fn apply_resolve_outcome(
    app: &App,
    epoch: u64,
    task_id: u64,
    task_generation: u64,
    was_addition: bool,
    outcome: Option<std::result::Result<Arc<ResolvedResource>, crate::qobuz::QobuzError>>,
) -> Option<(u64, u64, HashSet<Credit>, i64)> {
    let mut state = app.state();
    let current_epoch = state.account_epoch;
    let queue = &mut state.queue;
    let job = queue
        .jobs
        .get_mut(&task_id)
        .filter(|job| job.generation == task_generation)?;
    job.running = false;
    if job.phase != Phase::Planning {
        return None;
    }
    if current_epoch != epoch {
        job.phase = Phase::Waiting;
        job.dto.status = Status::Queued;
        return None;
    }
    let mut pending_activity = None;
    match outcome {
        Some(Ok(plan)) => {
            for track in plan.tracks.iter().take(10) {
                app.media.observe_track_metadata(track);
            }
            if let Some(at) = job.activity_at.take() {
                let credits = plan
                    .tracks
                    .iter()
                    .filter_map(|track| Credit::from_track(track, job.link.key.kind))
                    .collect::<HashSet<_>>();
                pending_activity = Some((task_id, task_generation, credits, at));
            }
            if was_addition {
                queue.merge_resolved_into_job(task_id, &plan, false);
                if queue.remaining_pending_additions(task_id).is_some() {
                    queue.requeue_for_additions(task_id, "Waiting to resolve newly added tracks.");
                } else if let Some(job) = queue.jobs.get_mut(&task_id) {
                    if job.has_queued_primary_files() {
                        hold_planned_job(job);
                    } else {
                        finalize_job(job);
                    }
                }
                queue.changed();
                return pending_activity;
            }
            if job.plan.is_none() {
                match job.link.key.kind {
                    ResourceKind::Artist | ResourceKind::Label | ResourceKind::Playlist => {
                        queue.install_album_rows(task_id, &plan);
                        queue.changed();
                        return pending_activity;
                    }
                    ResourceKind::Track => {
                        if queue.promote_track_to_release(task_id, plan.clone()).is_some() {
                            queue.changed();
                            return pending_activity;
                        }
                    }
                    ResourceKind::Album => {
                        job.release_id = Some(job.link.key.id.clone());
                        job.album_row = true;
                        apply_album_plan(job, "Album", plan.clone(), None, plan.thumbnail.clone());
                    }
                }
            }
            if queue.remaining_pending_additions(task_id).is_some() {
                queue.requeue_for_additions(task_id, "Waiting to resolve newly added tracks.");
            } else if let Some(job) = queue.jobs.get_mut(&task_id) {
                hold_planned_job(job);
            }
        }
        Some(Err(error)) => {
            if was_addition {
                finalize_job(job);
                if job.dto.status == Status::Failed {
                    job.dto.reason = Some(error.message);
                }
            } else {
                job.phase = Phase::Finished;
                job.dto.status = Status::Failed;
                job.dto.reason = Some(error.message);
            }
        }
        None => cancel(job),
    }
    queue.changed();
    pending_activity
}

