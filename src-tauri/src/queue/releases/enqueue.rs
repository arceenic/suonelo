use crate::{
    qobuz::{CanonicalResource, ResourceKind, normalize_link},
    storage::Settings,
};
use std::collections::{BTreeSet, HashMap};
use tokio_util::sync::CancellationToken;

use super::{
    QueueEntryRequest, ReleaseScope, effective_settings,
    merge::{detach_from_batch, filter_needed_scope, union_pending},
    update_release_subtitle,
};
use crate::queue::{Batch, JobMode, Phase, Queue, Status};

enum GroupedEntry {
    Release {
        album_id: String,
        link: CanonicalResource,
        scope: ReleaseScope,
        album_title: Option<String>,
        album_artist: Option<String>,
        total_tracks: Option<usize>,
        search_origin: bool,
    },
    Standalone(QueueEntryRequest),
}

fn group_requests(requests: Vec<QueueEntryRequest>) -> Vec<GroupedEntry> {
    let mut grouped: Vec<GroupedEntry> = Vec::new();
    let mut release_slots: HashMap<String, usize> = HashMap::new();

    for request in requests {
        let album_id = match request.link.key.kind {
            ResourceKind::Album => Some(request.link.key.id.clone()),
            ResourceKind::Track => request.release_id.clone(),
            _ => None,
        };
        let Some(album_id) = album_id else {
            grouped.push(GroupedEntry::Standalone(request));
            continue;
        };
        let scope = match request.link.key.kind {
            ResourceKind::Album => ReleaseScope::Full,
            ResourceKind::Track => {
                ReleaseScope::Tracks(BTreeSet::from([request.link.key.id.clone()]))
            }
            _ => unreachable!(),
        };
        if let Some(&slot) = release_slots.get(&album_id) {
            if let GroupedEntry::Release {
                scope: existing_scope,
                album_title,
                album_artist,
                total_tracks,
                search_origin,
                ..
            } = &mut grouped[slot]
            {
                existing_scope.union(scope);
                if album_title.is_none() {
                    *album_title = request.album_title;
                }
                if album_artist.is_none() {
                    *album_artist = request.album_artist;
                }
                if total_tracks.is_none() {
                    *total_tracks = request.total_tracks;
                }
                *search_origin |= request.search_origin;
            }
        } else {
            let link = if request.link.key.kind == ResourceKind::Album {
                request.link
            } else {
                normalize_link(&format!("https://play.qobuz.com/album/{album_id}"))
                    .unwrap_or(request.link)
            };
            release_slots.insert(album_id.clone(), grouped.len());
            grouped.push(GroupedEntry::Release {
                album_id,
                link,
                scope,
                album_title: request.album_title,
                album_artist: request.album_artist,
                total_tracks: request.total_tracks,
                search_origin: request.search_origin,
            });
        }
    }
    grouped
}

impl Queue {
    pub fn enqueue_grouped(
        &mut self,
        requests: Vec<QueueEntryRequest>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) -> Vec<u64> {
        if requests.is_empty() {
            return Vec::new();
        }
        let settings = effective_settings(settings, quality_override);
        let mode = JobMode::from_settings(&settings);
        let grouped = group_requests(requests);

        self.next_batch += 1;
        let batch = self.next_batch;
        let mut batch_jobs = Vec::new();
        let mut touched_jobs = Vec::new();
        let mut reconsidered = BTreeSet::new();

        for entry in grouped {
            match entry {
                GroupedEntry::Release {
                    album_id,
                    link,
                    scope,
                    album_title,
                    album_artist,
                    total_tracks,
                    search_origin,
                } => {
                    if let Some(target_id) =
                        self.find_compatible_release_job(&album_id, &settings, mode, None)
                    {
                        let (needs_batch, old_batch) = self.merge_scope_into_job(
                            target_id,
                            scope,
                            batch,
                            reason,
                            search_origin,
                            album_title,
                            album_artist,
                            total_tracks,
                        );
                        if let Some(old) = old_batch {
                            reconsidered.insert(old);
                        }
                        if needs_batch {
                            batch_jobs.push(target_id);
                        }
                        touched_jobs.push(target_id);
                    } else {
                        let id = self.insert_single_job(
                            link,
                            &settings,
                            batch,
                            reason,
                            quality_override,
                            search_origin,
                            true,
                            Some(album_id),
                            Some(scope),
                            album_title,
                            album_artist,
                            total_tracks,
                        );
                        batch_jobs.push(id);
                        touched_jobs.push(id);
                    }
                }
                GroupedEntry::Standalone(request) => {
                    let id = self.insert_single_job(
                        request.link,
                        &settings,
                        batch,
                        reason,
                        quality_override,
                        request.search_origin,
                        false,
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    batch_jobs.push(id);
                    touched_jobs.push(id);
                }
            }
        }

        if !batch_jobs.is_empty() {
            self.batches.insert(
                batch,
                Batch {
                    jobs: batch_jobs,
                    confirmation: None,
                    planning: false,
                },
            );
        }
        for old_batch in reconsidered {
            self.reconsider(old_batch);
        }
        touched_jobs
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_scope_into_job(
        &mut self,
        target_id: u64,
        scope: ReleaseScope,
        new_batch: u64,
        reason: &str,
        search_origin: bool,
        album_title: Option<String>,
        album_artist: Option<String>,
        total_tracks: Option<usize>,
    ) -> (bool, Option<u64>) {
        let Some(job) = self.jobs.get_mut(&target_id) else {
            return (false, None);
        };
        job.search_origin |= search_origin;
        if job.release_artist.is_none() {
            job.release_artist = album_artist;
        }
        if job.release_total_tracks.is_none() {
            job.release_total_tracks = total_tracks;
        }
        if job.plan.is_none()
            && job.dto.title == job.dto.url
            && let Some(title) = album_title.filter(|t| !t.trim().is_empty())
        {
            job.dto.title = title;
        }
        match &mut job.release_scope {
            Some(existing) => existing.union(scope.clone()),
            None => job.release_scope = Some(scope.clone()),
        }

        let Some(needed) = filter_needed_scope(job, scope) else {
            update_release_subtitle(job);
            return (false, None);
        };

        if job.plan.is_none() {
            return match job.phase {
                Phase::Waiting => {
                    update_release_subtitle(job);
                    (false, None)
                }
                Phase::Planning | Phase::Active => {
                    union_pending(&mut job.pending_additions, needed);
                    update_release_subtitle(job);
                    (false, None)
                }
                Phase::Finished | Phase::Held | Phase::Eligible => {
                    let old_batch = (job.batch != new_batch).then_some(job.batch);
                    job.batch = new_batch;
                    job.generation += 1;
                    job.cancel = CancellationToken::new();
                    job.only_file = None;
                    job.phase = Phase::Waiting;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some(reason.into());
                    update_release_subtitle(job);
                    detach_from_batch(&mut self.batches, old_batch, target_id);
                    (true, old_batch)
                }
            };
        }

        union_pending(&mut job.pending_additions, needed);
        match job.phase {
            Phase::Waiting | Phase::Planning | Phase::Active => (false, None),
            Phase::Held | Phase::Eligible | Phase::Finished => {
                let old_batch = (job.batch != new_batch).then_some(job.batch);
                job.batch = new_batch;
                job.generation += 1;
                job.cancel = CancellationToken::new();
                job.only_file = None;
                job.phase = Phase::Waiting;
                job.dto.status = Status::Queued;
                job.dto.reason = Some(reason.into());
                detach_from_batch(&mut self.batches, old_batch, target_id);
                (true, old_batch)
            }
        }
    }
}
