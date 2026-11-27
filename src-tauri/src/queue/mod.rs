mod actions;
mod dto;
mod planning;
mod playlists;
mod releases;
mod runner;
mod scheduler;
mod telemetry;
#[cfg(test)]
mod tests;

pub use dto::{
    ClearReport, Confirmation, FileDto, FileKind, FilePhase, JobDto, JobMode, QueueEvent,
    QueueSnapshot, Status,
};
pub use releases::QueueEntryRequest;
pub use scheduler::scheduler;
pub(crate) use telemetry::TransferMeter;

use crate::{
    media::{self, AudioReplacement, ReleaseLayout},
    qobuz::{CanonicalResource, ResolvedResource},
    recovery::RecoveryDto,
    storage::{DownloadMode, MaxConnections, Settings},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use tauri::ipc::Channel;
use tokio_util::sync::CancellationToken;

use actions::retryable_file;
use dto::release_progress;
use playlists::{PlaylistGroup, PlaylistMember};
use telemetry::RateWindow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Waiting,
    Planning,
    Held,
    Eligible,
    Active,
    Finished,
}

#[derive(Clone)]
pub(super) enum Extra {
    Artwork {
        track: usize,
        directory: PathBuf,
        basename: String,
        final_target: bool,
    },
    Booklet {
        track: usize,
        directory: PathBuf,
        source: media::BookletSource,
    },
    Lyrics {
        track: usize,
        audio: PathBuf,
    },
    Playlist,
}

#[derive(Clone)]
pub struct Job {
    pub link: CanonicalResource,
    pub(super) dto: JobDto,
    pub(super) settings: Settings,
    pub(super) phase: Phase,
    pub(super) batch: u64,
    pub(super) generation: u64,
    pub(super) cancel: CancellationToken,
    pub(super) running: bool,
    pub(super) plan: Option<Arc<ResolvedResource>>,
    pub(super) pinned: BTreeMap<String, ReleaseLayout>,
    pub(super) paths: BTreeMap<usize, PathBuf>,
    pub(super) extras: BTreeMap<u64, Extra>,
    pub(super) asset_roots: BTreeMap<u64, PathBuf>,
    pub(super) only_file: Option<u64>,
    pub(super) retrying: bool,
    pub(super) drive_retry: BTreeSet<PathBuf>,
    pub(super) active_drive_blocks: BTreeSet<PathBuf>,
    pub(super) blocked_destination: Option<PathBuf>,
    pub(super) playlist_path: Option<PathBuf>,
    pub(super) playlist_reserved: bool,
    pub(super) playlist_only_status: Option<(Status, Option<String>)>,
    pub(super) artwork_failures: BTreeMap<(String, PathBuf), media::MediaError>,
    pub(super) missing_embedded: BTreeSet<usize>,
    pub(super) quality_override: Option<u8>,
    pub(super) activity_at: Option<i64>,
    pub(super) search_coverage: Option<Arc<HashMap<String, String>>>,
    pub(super) search_origin: bool,
    pub(super) replacements: HashMap<u64, AudioReplacement>,
    pub(super) approved_replacement: Option<(u64, AudioReplacement)>,
    pub(super) row_order: (u64, usize),
    pub(super) album_row: bool,
    pub(super) playlist: Option<PlaylistMember>,
    pub(in crate::queue) release_id: Option<String>,
    pub(in crate::queue) release_scope: Option<releases::ReleaseScope>,
    pub(in crate::queue) release_total_tracks: Option<usize>,
    pub(in crate::queue) release_artist: Option<String>,
    pub(in crate::queue) audio_file_ids: BTreeMap<usize, u64>,
    pub(in crate::queue) pending_additions: Option<releases::ReleaseScope>,
    pub(in crate::queue) resolving_additions: bool,
}

impl Job {
    pub(super) fn audio_file_id(&self, index: usize) -> u64 {
        self.audio_file_ids
            .get(&index)
            .copied()
            .unwrap_or(index as u64 + 1)
    }

    pub(super) fn audio_track_index(&self, file_id: u64) -> Option<usize> {
        if !self.audio_file_ids.is_empty() {
            return self
                .audio_file_ids
                .iter()
                .find_map(|(index, id)| (*id == file_id).then_some(*index));
        }
        usize::try_from(file_id.checked_sub(1)?).ok()
    }

    pub(super) fn has_full_release(&self) -> bool {
        matches!(self.release_scope, Some(releases::ReleaseScope::Full))
            || matches!(self.pending_additions, Some(releases::ReleaseScope::Full))
            || (self.link.key.kind == crate::qobuz::ResourceKind::Album
                && self.release_scope.is_none())
    }

    pub(super) fn contains_track_id(&self, track_id: &str) -> bool {
        self.release_scope
            .as_ref()
            .is_some_and(|scope| scope.contains_track(track_id))
            || self
                .pending_additions
                .as_ref()
                .is_some_and(|scope| scope.contains_track(track_id))
            || self
                .plan
                .as_ref()
                .is_some_and(|plan| plan.tracks.iter().any(|track| track.id == track_id))
    }

    pub(super) fn has_queued_primary_files(&self) -> bool {
        let primary = self.dto.mode.primary_file();
        self.dto
            .files
            .iter()
            .any(|file| file.kind == primary && file.status == Status::Queued)
    }

    pub(super) fn can_retry_drive(&self) -> bool {
        matches!(self.phase, Phase::Active | Phase::Eligible) && self.dto.drive_blocked
    }

    pub(super) fn has_unfinished_files(&self) -> bool {
        self.dto.files.iter().any(retryable_file)
    }

    pub(super) fn snapshot(&self) -> JobDto {
        let mut dto = self.dto.clone();
        dto.generation = self.generation;
        dto.progress = release_progress(self);
        dto.clearable = self.phase != Phase::Active;
        dto.can_retry = self.can_retry_drive()
            || (!self.running && self.phase == Phase::Finished
                && (self.dto.status != Status::Completed || self.has_unfinished_files()));
        dto
    }
}

pub(super) struct Batch {
    pub(super) jobs: Vec<u64>,
    pub(super) confirmation: Option<Confirmation>,
    pub(super) planning: bool,
}

#[derive(Default)]
pub struct Queue {
    pub jobs: BTreeMap<u64, Job>,
    pub(super) batches: BTreeMap<u64, Batch>,
    pub(super) playlists: BTreeMap<u64, PlaylistGroup>,
    pub(super) next_id: u64,
    pub(super) next_batch: u64,
    pub(super) revision: u64,
    pub(super) channels: Vec<Channel<QueueEvent>>,
    pub(super) last_start: Option<Instant>,
    pub(super) recovery: RecoveryDto,
    pub(super) rate: RateWindow,
    pub(super) bytes_per_second: u64,
    pub(super) effective_mode: DownloadMode,
    pub(super) requested_mode: DownloadMode,
    pub(super) saved_limit: MaxConnections,
    pub(super) admitted: BTreeSet<u64>,
    pub(super) user_paused: bool,
    pub(super) wake: Arc<tokio::sync::Notify>,
}

pub(super) fn is_pending_audio(job: &Job, file: &FileDto) -> bool {
    file.kind == FileKind::Audio
        && file.status == Status::Queued
        && job.only_file.is_none_or(|id| id == file.id)
}

pub(super) fn prune_orphan_extras(job: &mut Job) {
    let ids: BTreeSet<_> = job.dto.files.iter().map(|file| file.id).collect();
    job.extras.retain(|id, _| ids.contains(id));
}

pub(super) fn drive_full_reason(drive: &str) -> String {
    format!("Drive {drive} is full. Free space and choose Retry.")
}

pub(super) fn mark_file_unavailable(file: &mut FileDto, reason: impl Into<String>) {
    file.status = Status::Unavailable;
    file.phase = FilePhase::Unavailable;
    file.reason = Some(reason.into());
    file.independent_retry = false;
}
