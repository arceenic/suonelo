use crate::{
    app::App,
    media::{TransferPhase, TransferProgress},
    recovery::{RecoveryNotice, TransferContext},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tauri::ipc::Channel;

use super::{
    FilePhase, Job, Phase, Queue, QueueEvent, QueueSnapshot, Status,
    dto::release_progress,
    runner::update_file,
    scheduler::RunTask,
};

#[derive(Default)]
pub(crate) struct TransferMeter {
    pending_bytes: AtomicU64,
}

impl TransferMeter {
    pub(crate) fn record(&self, bytes: u64) {
        self.pending_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn drain(&self) -> u64 {
        self.pending_bytes.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending_bytes.load(Ordering::Relaxed) > 0
    }
}

#[derive(Default)]
pub(crate) struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl RateWindow {
    pub(super) fn record(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
    }

    pub(super) fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub(super) fn bytes_per_second(&mut self, now: Instant) -> u64 {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_secs(1))
        {
            self.samples.pop_front();
        }
        self.samples
            .iter()
            .fold(0_u64, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }
}

impl Queue {
    fn emit(&mut self, event: QueueEvent) {
        if self.channels.len() == 1 {
            if self.channels[0].send(event).is_err() {
                self.channels.clear();
            }
            return;
        }
        if !self.channels.is_empty() {
            self.channels
                .retain(|channel| channel.send(event.clone()).is_ok());
        }
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            revision: self.revision,
            jobs: self.ordered_jobs().into_iter().map(Job::snapshot).collect(),
            confirmations: self
                .batches
                .values()
                .filter_map(|batch| batch.confirmation.clone())
                .collect(),
            recovery: self.recovery,
            bytes_per_second: self.bytes_per_second,
            can_clear: self.jobs.values().any(|job| job.phase != Phase::Active),
            protected_count: self
                .jobs
                .values()
                .filter(|job| job.phase == Phase::Active)
                .count(),
            effective_mode: self.effective_mode,
            requested_mode: self.requested_mode,
            live_limit: self.live_limit(),
            paused: self.user_paused,
            can_pause: self.admission_head().is_some(),
        }
    }

    pub fn subscribe(&mut self, channel: Channel<QueueEvent>) {
        if channel.send(QueueEvent::Snapshot(self.snapshot())).is_ok() {
            self.channels.push(channel);
        }
    }

    pub fn changed(&mut self) {
        self.revision += 1;
        if !self.channels.is_empty() {
            let snapshot = QueueEvent::Snapshot(self.snapshot());
            self.emit(snapshot);
        }
        self.wake.notify_one();
    }

    pub(super) fn progress(&mut self, job_id: u64, generation: u64, file_id: u64, progress: TransferProgress) {
        let Some(job) = self
            .jobs
            .get_mut(&job_id)
            .filter(|job| job.generation == generation && !job.cancel.is_cancelled())
        else {
            return;
        };
        let Some(file) = job.dto.files.iter_mut().find(|file| file.id == file_id) else {
            return;
        };
        if !matches!(file.status, Status::Downloading | Status::Retrying)
            || progress.attempt < file.attempt
        {
            return;
        }
        let phase = match progress.phase {
            TransferPhase::Connecting => FilePhase::Connecting,
            TransferPhase::Transferring => FilePhase::Transferring,
            TransferPhase::Processing => FilePhase::Processing,
        };
        let amount = progress.total.map(|total| {
            if total == 0 {
                0
            } else {
                (progress.received.saturating_mul(100) / total).min(99) as u8
            }
        });
        if file.attempt == progress.attempt
            && file.phase == phase
            && file.received_bytes == progress.received
            && file.total_bytes == progress.total
            && file.progress == amount
        {
            return;
        }
        if file.attempt == progress.attempt && progress.received < file.received_bytes {
            return;
        }
        file.status = Status::Downloading;
        file.phase = phase;
        file.progress = amount;
        file.generation = generation;
        file.attempt = progress.attempt;
        file.received_bytes = progress.received;
        file.total_bytes = progress.total;
        file.final_size_bytes = None;
        let attempt = file.attempt;
        let received_bytes = file.received_bytes;
        let total_bytes = file.total_bytes;
        let final_size_bytes = file.final_size_bytes;
        let job_progress = release_progress(job);
        self.revision += 1;
        let event = QueueEvent::Progress {
            revision: self.revision,
            job_id,
            file_id,
            generation,
            attempt,
            progress: amount,
            phase,
            received_bytes,
            total_bytes,
            final_size_bytes,
            job_progress,
        };
        self.emit(event);
    }

    pub(super) fn refresh_rate(&mut self, now: Instant) {
        let bytes_per_second = self.rate.bytes_per_second(now);
        if self.bytes_per_second == bytes_per_second {
            return;
        }
        self.bytes_per_second = bytes_per_second;
        self.revision += 1;
        let event = QueueEvent::Speed {
            revision: self.revision,
            bytes_per_second,
        };
        self.emit(event);
    }
}

pub(super) fn progress<'a>(
    app: &'a App,
    task: &'a RunTask,
    id: u64,
) -> impl Fn(TransferProgress) + Send + Sync + 'a {
    let last = Mutex::new(None::<(Instant, u64, TransferPhase)>);
    move |progress| {
        if task.cancel.is_cancelled() {
            return;
        }
        let mut last = last.lock().unwrap();
        if last.is_some_and(|(last, attempt, phase)| {
            last.elapsed() < Duration::from_millis(170)
                && attempt == progress.attempt
                && phase == progress.phase
        }) {
            return;
        }
        *last = Some((Instant::now(), progress.attempt, progress.phase));
        app.state()
            .queue
            .progress(task.id, task.generation, id, progress);
    }
}

pub(super) fn transfer_context(app: &Arc<App>, task: &RunTask, id: u64) -> TransferContext {
    let owner = app.clone();
    let meter = app.meter.clone();
    let job_id = task.id;
    let generation = task.generation;
    app.qobuz.recovery().transfer(
        job_id,
        generation,
        task.settings.max_connections.limit(),
        move |notice| {
            if let RecoveryNotice::NetworkBytes { bytes } = notice {
                meter.record(bytes);
                return;
            }
            let mut state = owner.state();
            let queue = &mut state.queue;
            let Some(job) = queue
                .jobs
                .get_mut(&job_id)
                .filter(|job| job.generation == generation && !job.cancel.is_cancelled())
            else {
                return;
            };
            update_file(job, id, |file| match notice {
                RecoveryNotice::Transferring => {
                    if file.status == Status::Retrying {
                        file.progress = Some(0);
                    }
                    file.status = Status::Downloading;
                    if matches!(file.phase, FilePhase::Queued | FilePhase::Retrying) {
                        file.phase = FilePhase::Connecting;
                    }
                    file.reason = None;
                }
                RecoveryNotice::Retrying { reason } => {
                    file.status = Status::Retrying;
                    file.phase = FilePhase::Retrying;
                    file.reason = Some(reason);
                }
                RecoveryNotice::RateLimited { seconds } => {
                    file.status = Status::Retrying;
                    file.phase = FilePhase::Retrying;
                    file.reason = Some(format!(
                        "This file host is limiting requests. Retrying in {seconds}s."
                    ));
                }
                RecoveryNotice::NetworkBytes { .. } => {
                    unreachable!("byte notices are handled before file state")
                }
            });
            queue.changed();
        },
    )
}
