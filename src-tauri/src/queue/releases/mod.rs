mod enqueue;
mod merge;

use crate::{
    qobuz::{CanonicalResource, ResourceKey, ResourceKind},
    search::activity::event_time,
    storage::Settings,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio_util::sync::CancellationToken;

use super::{FileKind, Job, JobDto, JobMode, Phase, Queue, Status};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::queue) enum ReleaseScope {
    Full,
    Tracks(BTreeSet<String>),
}

impl ReleaseScope {
    pub(in crate::queue) fn union(&mut self, other: Self) {
        match (self, other) {
            (Self::Full, _) => {}
            (slot, Self::Full) => *slot = Self::Full,
            (Self::Tracks(existing), Self::Tracks(incoming)) => existing.extend(incoming),
        }
    }

    pub(in crate::queue) fn contains_track(&self, id: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Tracks(ids) => ids.contains(id),
        }
    }

    pub(in crate::queue) fn track_ids(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Full => None,
            Self::Tracks(ids) => Some(ids),
        }
    }
}

#[derive(Clone, Debug)]
pub struct QueueEntryRequest {
    pub link: CanonicalResource,
    pub release_id: Option<String>,
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub total_tracks: Option<usize>,
    pub search_origin: bool,
}

impl QueueEntryRequest {
    pub fn from_link(link: CanonicalResource) -> Self {
        let release_id = (link.key.kind == ResourceKind::Album).then(|| link.key.id.clone());
        Self {
            link,
            release_id,
            album_title: None,
            album_artist: None,
            total_tracks: None,
            search_origin: false,
        }
    }
}

pub(in crate::queue) fn release_subtitle(
    artist: Option<&str>,
    mode: JobMode,
    selected: usize,
    total: Option<usize>,
) -> String {
    let prefix = match artist.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{name} · Album"),
        None => "Album".to_owned(),
    };
    match mode {
        JobMode::Booklet => {
            let unit = if selected == 1 { "PDF" } else { "PDFs" };
            format!("{prefix} · {selected} {unit}")
        }
        JobMode::Audio => {
            if let Some(total) = total.filter(|total| *total > selected) {
                format!("{prefix} · {selected} of {total} tracks")
            } else {
                let unit = if selected == 1 { "track" } else { "tracks" };
                format!("{prefix} · {selected} {unit}")
            }
        }
    }
}

pub(in crate::queue) fn update_release_subtitle(job: &mut Job) {
    let primary = job.dto.mode.primary_file();
    let planned_count = job
        .dto
        .files
        .iter()
        .filter(|file| file.kind == primary)
        .count();
    let count = if planned_count > 0 || job.plan.is_some() {
        planned_count
    } else {
        match &job.release_scope {
            Some(ReleaseScope::Tracks(ids)) => ids.len(),
            Some(ReleaseScope::Full) => job.release_total_tracks.unwrap_or(0),
            None => 0,
        }
    };
    if job.dto.mode == JobMode::Audio
        && job.plan.is_some()
        && job
            .release_total_tracks
            .is_some_and(|total| total > 0 && planned_count >= total)
        && job.release_scope.is_some()
    {
        job.release_scope = Some(ReleaseScope::Full);
    }
    if count == 0 && job.plan.is_none() {
        job.dto.subtitle = job.release_artist.clone().unwrap_or_default();
        return;
    }
    job.dto.subtitle = release_subtitle(
        job.release_artist.as_deref(),
        job.dto.mode,
        count,
        job.release_total_tracks,
    );
}

pub(in crate::queue) fn sort_job_files(job: &mut Job) {
    let Some(plan) = &job.plan else {
        return;
    };
    let mut audio = Vec::new();
    let mut extras = Vec::new();
    for file in job.dto.files.drain(..) {
        if file.kind == FileKind::Audio {
            audio.push(file);
        } else {
            extras.push(file);
        }
    }
    audio.sort_by(|a, b| {
        let key_a = job
            .audio_track_index(a.id)
            .and_then(|idx| plan.tracks.get(idx))
            .map(|t| {
                (
                    t.disc,
                    if t.number > 0 { t.number } else { u32::MAX },
                    t.id.as_str(),
                    a.id,
                )
            });
        let key_b = job
            .audio_track_index(b.id)
            .and_then(|idx| plan.tracks.get(idx))
            .map(|t| {
                (
                    t.disc,
                    if t.number > 0 { t.number } else { u32::MAX },
                    t.id.as_str(),
                    b.id,
                )
            });
        key_a.cmp(&key_b)
    });
    job.dto.files = audio;
    job.dto.files.extend(extras);
}

pub(in crate::queue) fn compatible_release_settings(
    job: &Job,
    settings: &Settings,
    mode: JobMode,
) -> bool {
    job.playlist.is_none()
        && job.dto.mode == mode
        && job.settings.output == settings.output
        && job.settings.output_location == settings.output_location
}

pub(in crate::queue) fn effective_settings(
    settings: &Settings,
    quality_override: Option<u8>,
) -> Settings {
    let mut next = settings.clone();
    if let Some(quality) = quality_override {
        next.output.quality = quality;
    }
    next
}

impl Queue {
    pub fn is_duplicate_request(
        &self,
        key: &ResourceKey,
        release_id: Option<&str>,
        settings: &Settings,
    ) -> bool {
        let mode = JobMode::from_settings(settings);
        match key.kind {
            ResourceKind::Album => self.jobs.values().any(|job| {
                job.playlist.is_none()
                    && job.release_id.as_deref() == Some(key.id.as_str())
                    && job.has_full_release()
            }),
            ResourceKind::Track => self.jobs.values().any(|job| {
                if !compatible_release_settings(job, settings, mode) {
                    return false;
                }
                if job.link.key == *key {
                    return true;
                }
                if let Some(album) = release_id
                    && job.release_id.as_deref() != Some(album)
                {
                    return false;
                }
                job.contains_track_id(&key.id)
            }),
            _ => self.jobs.values().any(|job| job.link.key == *key),
        }
    }

    pub(in crate::queue) fn find_compatible_release_job(
        &self,
        album_id: &str,
        settings: &Settings,
        mode: JobMode,
        exclude_id: Option<u64>,
    ) -> Option<u64> {
        self.ordered_jobs()
            .into_iter()
            .find(|job| {
                exclude_id.is_none_or(|excluded| job.dto.id != excluded)
                    && compatible_release_settings(job, settings, mode)
                    && job.release_id.as_deref() == Some(album_id)
            })
            .map(|job| job.dto.id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::queue) fn insert_single_job(
        &mut self,
        link: CanonicalResource,
        settings: &Settings,
        batch: u64,
        reason: &str,
        quality_override: Option<u8>,
        search_origin: bool,
        album_row: bool,
        release_id: Option<String>,
        release_scope: Option<ReleaseScope>,
        album_title: Option<String>,
        album_artist: Option<String>,
        total_tracks: Option<usize>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let resource_kind = if release_id.is_some() {
            ResourceKind::Album
        } else {
            link.key.kind
        };
        let title = album_title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| link.original_url.clone());
        let mut job = Job {
            dto: JobDto {
                id,
                url: link.original_url.clone(),
                title,
                subtitle: String::new(),
                edition: None,
                mode: JobMode::from_settings(settings),
                generation: 0,
                progress: Some(0),
                clearable: true,
                thumbnail: None,
                resource_kind,
                status: Status::Queued,
                reason: Some(reason.into()),
                drive_blocked: false,
                can_retry: false,
                files: Vec::new(),
            },
            link,
            settings: settings.clone(),
            phase: Phase::Waiting,
            batch,
            generation: 0,
            cancel: CancellationToken::new(),
            running: false,
            plan: None,
            pinned: BTreeMap::new(),
            paths: BTreeMap::new(),
            extras: BTreeMap::new(),
            asset_roots: BTreeMap::new(),
            only_file: None,
            retrying: false,
            drive_retry: BTreeSet::new(),
            active_drive_blocks: BTreeSet::new(),
            blocked_destination: None,
            playlist_path: None,
            playlist_reserved: false,
            playlist_only_status: None,
            artwork_failures: BTreeMap::new(),
            missing_embedded: BTreeSet::new(),
            quality_override,
            activity_at: Some(event_time()),
            search_coverage: None,
            search_origin,
            replacements: HashMap::new(),
            approved_replacement: None,
            row_order: (id, 0),
            album_row,
            playlist: None,
            release_id,
            release_scope,
            release_total_tracks: total_tracks,
            release_artist: album_artist,
            audio_file_ids: BTreeMap::new(),
            pending_additions: None,
            resolving_additions: false,
        };
        if job.release_id.is_some() {
            update_release_subtitle(&mut job);
        }
        self.jobs.insert(id, job);
        id
    }
}
