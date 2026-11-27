use crate::{
    media::DeliveredQuality,
    qobuz::{LyricsSource, ResourceKind},
    recovery::RecoveryDto,
    storage::{DownloadMode, MaxConnections, Settings},
};
use serde::Serialize;
use std::{path::PathBuf, sync::Arc};

use super::{Job, Phase};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Retrying,
    Failed,
    Unavailable,
    Incomplete,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Audio,
    Artwork,
    Booklet,
    Lyrics,
    Playlist,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePhase {
    Queued,
    Connecting,
    Transferring,
    Processing,
    Retrying,
    Completed,
    Failed,
    Cancelled,
    Unavailable,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobMode {
    Audio,
    Booklet,
}

impl JobMode {
    pub(super) fn from_settings(settings: &Settings) -> Self {
        if settings.output.booklet_only {
            Self::Booklet
        } else {
            Self::Audio
        }
    }

    pub(super) fn primary_file(self) -> FileKind {
        match self {
            Self::Audio => FileKind::Audio,
            Self::Booklet => FileKind::Booklet,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDto {
    pub id: u64,
    pub kind: FileKind,
    pub title: String,
    pub album: Option<String>,
    pub edition: Option<String>,
    pub disc: Option<u32>,
    pub status: Status,
    pub progress: Option<u8>,
    pub generation: u64,
    pub attempt: u64,
    pub phase: FilePhase,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub final_size_bytes: Option<u64>,
    pub quality: Option<String>,
    pub path: Option<PathBuf>,
    pub conflict_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
    pub reason: Option<String>,
    pub reused: bool,
    pub independent_retry: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobDto {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub subtitle: String,
    pub edition: Option<String>,
    pub mode: JobMode,
    pub generation: u64,
    pub progress: Option<u8>,
    pub clearable: bool,
    pub thumbnail: Option<Arc<str>>,
    pub resource_kind: ResourceKind,
    pub status: Status,
    pub reason: Option<String>,
    pub drive_blocked: bool,
    pub can_retry: bool,
    pub files: Vec<FileDto>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Confirmation {
    pub id: u64,
    pub plan_revision: u64,
    pub track_count: usize,
    pub unit: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub revision: u64,
    pub jobs: Vec<JobDto>,
    pub confirmations: Vec<Confirmation>,
    pub recovery: RecoveryDto,
    pub bytes_per_second: u64,
    pub can_clear: bool,
    pub protected_count: usize,
    pub effective_mode: DownloadMode,
    pub requested_mode: DownloadMode,
    pub live_limit: Option<MaxConnections>,
    pub paused: bool,
    pub can_pause: bool,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum QueueEvent {
    Snapshot(QueueSnapshot),
    #[serde(rename_all = "camelCase")]
    Progress {
        revision: u64,
        job_id: u64,
        file_id: u64,
        generation: u64,
        attempt: u64,
        progress: Option<u8>,
        phase: FilePhase,
        received_bytes: u64,
        total_bytes: Option<u64>,
        final_size_bytes: Option<u64>,
        job_progress: Option<u8>,
    },
    #[serde(rename_all = "camelCase")]
    Speed {
        revision: u64,
        bytes_per_second: u64,
    },
}

#[derive(Serialize)]
pub struct ClearReport {
    pub removed: usize,
    pub protected: usize,
}


pub(super) fn aggregate(finished: usize, total: usize) -> Status {
    if finished == 0 {
        Status::Failed
    } else if finished < total {
        Status::Incomplete
    } else {
        Status::Completed
    }
}

pub(super) fn release_progress(job: &Job) -> Option<u8> {
    let count = job.dto.files.len() + usize::from(job.playlist_reserved);
    if count == 0 {
        return Some(0);
    }
    let sum: usize = job
        .dto
        .files
        .iter()
        .map(|file| match file.status {
            Status::Completed => 100,
            Status::Failed | Status::Cancelled | Status::Unavailable => 0,
            _ => usize::from(file.progress.unwrap_or(0).min(99)),
        })
        .sum();
    let average = (sum / count) as u8;
    let finalized = job.phase == Phase::Finished
        && job.dto.status == Status::Completed
        && !job.playlist_reserved
        && job
            .dto
            .files
            .iter()
            .all(|file| file.status == Status::Completed);
    Some(if finalized { average } else { average.min(99) })
}

pub(super) fn quality_label(quality: &DeliveredQuality) -> String {
    if quality.tier == 1 {
        return "MP3 · 320 kbps".into();
    }
    match (quality.bit_depth, quality.sample_rate) {
        (Some(bits), Some(rate)) => format!("FLAC · {bits}-bit / {rate} kHz"),
        _ => format!("FLAC · tier {}", quality.tier),
    }
}

pub(super) fn lyrics_label(source: LyricsSource, timed: bool) -> String {
    let provider = match source {
        LyricsSource::Qobuz => "Qobuz",
        LyricsSource::Lrclib => "LRCLIB",
    };
    format!(
        "{provider} · {}",
        if timed { "Timed" } else { "Plain text" }
    )
}
