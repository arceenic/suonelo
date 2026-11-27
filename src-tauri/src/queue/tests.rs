use super::*;
use super::{
    dto::{aggregate, release_progress},
    planning::{apply_album_plan, fallback_sequence, item, next_file, plan_assets, plan_booklet_rows, plan_file_rows, refresh_cover_warnings, release_groups},
    playlists::playlist_input,
    runner::{complete, fail, final_size, lyric_failure, run_file_stage, settle_audio_pass, update_file},
    scheduler::{RunTask, schedule, write_destinations},
    telemetry::{RateWindow, transfer_context},
};
use crate::{
    app::App,
    media::{self, DeliveredQuality, LyricsMode, TransferPhase, TransferProgress},
    qobuz::{ResolvedTrack, ResourceKey, ResourceKind},
};
use futures_util::{StreamExt, stream};
use std::{collections::HashSet, path::Path, time::Duration};

use crate::qobuz::normalize_link;

fn track(id: &str, album_id: &str, disc: u32, number: u32) -> ResolvedTrack {
    ResolvedTrack {
        id: id.into(),
        title: "Track".into(),
        album_id: album_id.into(),
        disc,
        number,
        track: serde_json::json!({}),
        album: Arc::new(
            serde_json::json!({"id": album_id, "title": "Album", "goodies": [
                {"id": "book", "file_format_id": 21, "name": "Booklet", "url": "https://example.test/book.pdf"}
            ]}),
        ),
        unavailable: None,
    }
}

fn active_queue() -> Queue {
    let mut queue = Queue::default();
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/1").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    let job = queue.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Active;
    job.running = true;
    job.dto.status = Status::Downloading;
    let mut file = item(
        1,
        FileKind::Audio,
        "Track".into(),
        &track("1", "album", 1, 1),
        Some(1),
    );
    file.status = Status::Downloading;
    file.phase = FilePhase::Connecting;
    job.dto.files.push(file);
    queue
}

#[test]
fn artwork_rows_follow_the_delivered_release_without_changing_audio_or_booklet_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let mut settings = Settings::default();
    settings.output.folder_template =
        "%album artist%/[%bit depth% Bit]/Disc %disc number%/%album name%".into();
    let mut tracks = Vec::new();
    for (id, album, disc) in [("1", "001", 1), ("2", "001", 2), ("3", "002", 2)] {
        let mut entry = track(id, album, disc, 1);
        let metadata = Arc::make_mut(&mut entry.album);
        metadata["artist"] = serde_json::json!({"name":"Artist"});
        metadata["media_count"] = serde_json::json!(2);
        tracks.push(entry);
    }
    let plan = Arc::new(ResolvedResource {
        edition: None,
        title: "Releases".into(),
        tracks,
        thumbnail: None,
        thumbnail_url: None,
    });
    let mut queue = Queue::default();
    queue.enqueue(
        vec![normalize_link("https://play.qobuz.com/artist/123").unwrap()],
        &settings,
        "Waiting",
    );
    let task = RunTask {
        id: 1,
        generation: 0,
        settings: settings.clone(),
        root: root.clone(),
        plan: plan.clone(),
        cancel: CancellationToken::new(),
        audio: vec![0, 1, 2],
        audio_file_ids: HashMap::new(),
        only_file: None,
        account_epoch: 0,
        search_origin: false,
        kind: ResourceKind::Artist,
        replacement: None,
        mode: DownloadMode::Sequential,
    };
    let job = queue.jobs.get_mut(&1).unwrap();
    job.plan = Some(plan.clone());
    plan_assets(job, &task);
    let cover_rows = |job: &Job| {
        job.extras
            .iter()
            .filter_map(|(id, extra)| match extra {
                Extra::Artwork {
                    track,
                    directory,
                    final_target,
                    ..
                } => Some((*id, *track, directory.clone(), *final_target)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let initial = cover_rows(job);
    assert_eq!(initial.len(), 2);
    assert!(initial.iter().all(|(_, _, _, final_target)| !final_target));
    let old_row = initial.iter().find(|(_, index, _, _)| *index == 0).unwrap();
    let (audio, layout) = media::plan_output(
        &root,
        &settings.output,
        &plan.tracks[0],
        &DeliveredQuality::requested(2),
        None,
        1,
    )
    .unwrap();
    let cover_dir = layout.cover_dir.clone().unwrap();
    assert_ne!(cover_dir, old_row.2);
    assert!(audio.parent().unwrap().ends_with("16 Bit/Disc 1/Album"));
    assert!(cover_dir.ends_with("16 Bit/Album [001]"));
    let release_root = layout.album_root.clone();
    job.pinned.insert("001".into(), layout);
    plan_assets(job, &task);
    let final_rows = cover_rows(job);
    assert!(final_rows.contains(&(old_row.0, 0, cover_dir.clone(), true)));
    assert_eq!(final_rows.len(), 2);
    assert!(job.extras.values().any(|extra| matches!(extra,
        Extra::Booklet { track:0, directory, .. } if directory == &release_root)));
    let mut retry = RunTask { settings, ..task };
    retry.settings.output.cover_filename = "Different".into();
    plan_assets(job, &retry);
    assert!(
        matches!(job.extras.get(&old_row.0), Some(Extra::Artwork { basename, directory, .. })
        if basename == "cover" && directory == &cover_dir)
    );
    let cover_path = cover_dir.join("cover.jpg");
    update_file(job, old_row.0, |file| {
        complete(file, cover_path.clone(), None, Some(1))
    });
    let audio_id = next_file(job);
    let mut row = item(
        audio_id,
        FileKind::Audio,
        "Song".into(),
        &plan.tracks[0],
        Some(1),
    );
    complete(&mut row, audio.clone(), None, Some(1));
    job.dto.files.push(row);
    job.missing_embedded.extend([0, 1]);
    refresh_cover_warnings(job, &plan, "001");
    assert_eq!(
        job.dto
            .files
            .iter()
            .find(|file| file.id == old_row.0)
            .unwrap()
            .reason
            .as_deref(),
        Some("2 finalized tracks still lack embedded artwork.")
    );
    plan_assets(job, &retry);
    assert!(job.dto.files.iter().any(|file| file.id == old_row.0
        && file.status == Status::Completed
        && file.path.as_ref() == Some(&cover_path)));
    job.phase = Phase::Finished;
    job.dto.status = Status::Incomplete;
    let evidence = queue.release_folder_evidence(1).unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].path, audio);
    assert_eq!(evidence[0].root, release_root);
}

#[test]
fn search_parent_only_removes_covered_pending_children_after_admission() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    let tracks = (1..=4).map(|id| normalize_link(&format!("https://play.qobuz.com/track/{id}")).unwrap()).collect();
    queue.enqueue(tracks, &settings, "Waiting");
    queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
    queue.jobs.get_mut(&3).unwrap().phase = Phase::Finished;
    queue.enqueue_search(
        vec![normalize_link("https://play.qobuz.com/album/album").unwrap()],
        &settings, "Waiting", HashSet::from(["album".into()]), HashMap::new(),
    );
    let parent = queue.jobs.get_mut(&5).unwrap();
    parent.plan = Some(Arc::new(ResolvedResource {
        edition: None,
        title: "Album".into(), thumbnail: None, thumbnail_url: None,
        tracks: (1..=3).map(|id| track(&id.to_string(), "album", 1, id)).collect(),
    }));
    parent.phase = Phase::Held;
    assert!(!queue.apply_search_coverage());
    assert_eq!(queue.jobs.len(), 5);
    queue.jobs.get_mut(&5).unwrap().phase = Phase::Eligible;
    assert!(queue.apply_search_coverage());
    assert_eq!(queue.jobs.keys().copied().collect::<Vec<_>>(), [2, 3, 4, 5]);
}

#[test]
fn release_folder_uses_saved_layouts_and_rejects_stale_or_unknown_jobs() {
    let mut queue = active_queue();
    assert!(queue.release_folder_evidence(1).unwrap().is_empty());
    assert!(queue.release_folder_evidence(999).is_err());
    let job = queue.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Finished;
    job.running = false;
    job.dto.status = Status::Completed;
    job.dto.files[0].status = Status::Completed;
    job.dto.files[0].path = Some(PathBuf::from("C:/saved/album/Disc 1/track.flac"));
    assert!(queue.release_folder_evidence(1).unwrap().is_empty());
    let job = queue.jobs.get_mut(&1).unwrap();
    job.pinned.insert(
        "album".into(),
        ReleaseLayout {
            album_root: PathBuf::from("C:/saved/album"),
            cover_dir: None,
            disc_dirs: BTreeMap::new(),
        },
    );
    job.settings.output_location = None;
    assert_eq!(
        queue.release_folder_evidence(1).unwrap()[0].root,
        PathBuf::from("C:/saved/album")
    );
    queue.jobs.get_mut(&1).unwrap().dto.status = Status::Incomplete;
    assert!(queue.release_folder_evidence(1).is_ok());
    queue.jobs.get_mut(&1).unwrap().dto.files[0].status = Status::Failed;
    assert!(queue.release_folder_evidence(1).unwrap().is_empty());
}

#[test]
fn simultaneous_downloads_find_their_own_folder_after_the_first_save() {
    let temp = tempfile::tempdir().unwrap();
    let mut queue = active_queue();
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/2").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    for (id, title) in [(1, "First"), (2, "Second")] {
        assert!(queue.release_folder_evidence(id).unwrap().is_empty());
        let root = temp.path().join(title);
        std::fs::create_dir_all(root.join("Disc 1")).unwrap();
        let path = root.join("Disc 1/track.flac");
        std::fs::write(&path, b"first saved track").unwrap();
        let job = queue.jobs.get_mut(&id).unwrap();
        job.phase = Phase::Active;
        job.running = true;
        job.dto.status = Status::Downloading;
        let track = track("1", title, 1, 1);
        let mut saved = item(1, FileKind::Audio, "Track".into(), &track, Some(1));
        complete(&mut saved, path, None, None);
        job.dto.files = vec![saved, item(2, FileKind::Audio, "Pending".into(), &track, Some(1))];
        job.pinned.insert(title.into(), ReleaseLayout {
            album_root: root,
            cover_dir: None,
            disc_dirs: BTreeMap::new(),
        });
    }
    for (id, title) in [(1, "First"), (2, "Second")] {
        let chosen = crate::release_folder::select(queue.release_folder_evidence(id).unwrap()).unwrap();
        assert_eq!(chosen.path, dunce::canonicalize(temp.path().join(title)).unwrap());
    }
}

#[test]
fn mixed_windows_paths_reach_the_real_filesystem_resolver() {
    let temp = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(temp.path()).unwrap().join("Album");
    std::fs::create_dir_all(root.join("Disc 1")).unwrap();
    let mut q = active_queue();
    let job = q.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Finished;
    job.running = false;
    job.dto.status = Status::Completed;
    job.dto.files.clear();
    for (i, name) in [
        "Disc 1/track.flac",
        "Disc 1/track.lrc",
        "cover.jpg",
        "booklet.pdf",
    ]
    .iter()
    .enumerate()
    {
        let path = root.join(name);
        std::fs::write(&path, b"saved").unwrap();
        let mut file = item(
            i as u64 + 1,
            FileKind::Audio,
            name.to_string(),
            &track("1", "album", 1, 1),
            Some(1),
        );
        file.kind = [
            FileKind::Audio,
            FileKind::Lyrics,
            FileKind::Artwork,
            FileKind::Booklet,
        ][i];
        file.status = Status::Completed;
        file.path = Some(std::fs::canonicalize(path).unwrap());
        job.dto.files.push(file);
    }
    job.pinned.insert(
        "album".into(),
        ReleaseLayout {
            album_root: root.clone(),
            cover_dir: None,
            disc_dirs: BTreeMap::new(),
        },
    );
    let evidence = q
        .release_folder_evidence(1)
        .expect("canonical saved output must match planned ordinary root");
    assert_eq!(
        crate::release_folder::select(evidence).unwrap().path,
        dunce::canonicalize(&root).unwrap()
    );
    q.jobs.get_mut(&1).unwrap().dto.status = Status::Incomplete;
    assert!(q.release_folder_evidence(1).is_ok());
    std::fs::remove_file(root.join("Disc 1/track.flac")).unwrap();
    assert!(crate::release_folder::select(q.release_folder_evidence(1).unwrap()).is_err());
}

#[cfg(windows)]
#[test]
fn windows_prefix_equivalence_retains_component_boundaries() {
    let mut q = active_queue();
    let job = q.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Finished;
    job.running = false;
    job.dto.status = Status::Completed;
    job.dto.files[0].status = Status::Completed;
    for (file, root, ok) in [
        (
            r"\\?\E:\Music\Album\Disc 1\track.flac",
            r"E:\Music\Album",
            true,
        ),
        (r"E:\Music\Album\track.flac", r"\\?\E:\Music\Album", true),
        (
            r"\\?\UNC\server\share\Album\track.flac",
            r"\\server\share\Album",
            true,
        ),
        (
            r"\\server\share\Album\track.flac",
            r"\\?\UNC\server\share\Album",
            true,
        ),
        (r"E:\Music\Album2\track.flac", r"E:\Music\Album", false),
        (r"F:\Music\Album\track.flac", r"E:\Music\Album", false),
        (
            r"\\?\UNC\server\share2\Album\track.flac",
            r"\\server\share\Album",
            false,
        ),
    ] {
        let job = q.jobs.get_mut(&1).unwrap();
        job.dto.files[0].path = Some(file.into());
        job.pinned.clear();
        job.pinned.insert(
            "album".into(),
            ReleaseLayout {
                album_root: root.into(),
                cover_dir: None,
                disc_dirs: BTreeMap::new(),
            },
        );
        assert_eq!(!q.release_folder_evidence(1).unwrap().is_empty(), ok, "{file} / {root}");
    }
}

#[cfg(windows)]
#[test]
#[ignore = "opens Explorer for an isolated completed-release fixture"]
fn native_completed_release_folder_action() {
    let root =
        PathBuf::from(std::env::var_os("SUONELO_FOLDER_SMOKE").expect("isolated test folder"));
    std::fs::create_dir_all(root.join("Disc 1")).unwrap();
    let saved = root.join("Disc 1/round7-track.flac");
    std::fs::write(&saved, b"fixture").unwrap();
    let mut q = active_queue();
    let job = q.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Finished;
    job.running = false;
    job.dto.status = Status::Completed;
    job.dto.files[0].status = Status::Completed;
    job.dto.files[0].path = Some(std::fs::canonicalize(saved).unwrap());
    job.pinned.insert(
        "album".into(),
        ReleaseLayout {
            album_root: root.clone(),
            cover_dir: None,
            disc_dirs: BTreeMap::new(),
        },
    );
    let folder = crate::release_folder::select(q.release_folder_evidence(1).unwrap()).unwrap().path;
    assert_eq!(folder, dunce::canonicalize(root).unwrap());
    crate::platform::shell::open_folder_sync(
        &folder,
        "The saved release folder is missing or inaccessible.",
        "Windows could not open the saved release folder.",
    )
    .unwrap();
}

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/implementation-fixtures.json"
    ))
    .unwrap()
}

#[test]
fn auxiliary_files_do_not_determine_completion() {
    assert!(aggregate(0, 0) == Status::Failed);
    assert!(aggregate(0, 2) == Status::Failed);
    assert!(aggregate(1, 2) == Status::Incomplete);
    assert!(aggregate(2, 2) == Status::Completed);
}

#[test]
fn progress_deltas_use_the_queue_ipc_fields() {
    let event = QueueEvent::Progress {
        revision: 5,
        job_id: 2,
        file_id: 3,
        generation: 1,
        attempt: 3,
        progress: Some(28),
        phase: FilePhase::Transferring,
        received_bytes: 28,
        total_bytes: Some(100),
        final_size_bytes: None,
        job_progress: Some(28),
    };
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"revision": 5, "jobId": 2, "fileId": 3, "generation": 1,
            "attempt":3, "progress": 28, "phase":"transferring", "receivedBytes":28,
            "totalBytes":100, "finalSizeBytes":null, "jobProgress":28})
    );
}

#[test]
fn large_submission_confirmation_covers_all_jobs_and_rejects_an_old_count() {
    let links = [
        "https://open.qobuz.com/playlist/100",
        "https://open.qobuz.com/playlist/101",
    ]
    .into_iter()
    .map(|url| normalize_link(url).unwrap())
    .collect();
    let mut queue = Queue::default();
    queue.enqueue(links, &Settings::default(), "Waiting");
    for (job, count) in queue.jobs.values_mut().zip([110, 20]) {
        job.phase = Phase::Held;
        let source = track("1", "album", 1, 1);
        job.dto.files.extend(
            (1..=count).map(|id| item(id, FileKind::Audio, "Track".into(), &source, None)),
        );
    }
    queue.reconsider(1);
    queue.changed();
    let old = queue.snapshot().confirmations.pop().unwrap();
    assert_eq!(old.track_count, 130);
    queue.cancel(2, true).unwrap();
    let next = queue.snapshot().confirmations.pop().unwrap();
    assert_eq!(next.track_count, 110);
    assert!(queue.decide(old.id, old.plan_revision, true).is_err());
    queue.decide(next.id, next.plan_revision, false).unwrap();
    assert!(
        queue
            .jobs
            .values()
            .all(|job| job.dto.status == Status::Cancelled)
    );
    assert!(queue.snapshot().confirmations.is_empty());
}

#[test]
fn retry_preserves_job_mode_and_resolved_selection_while_capturing_compatible_settings() {
    let mut original = Settings::default();
    original.output.booklet_only = true;
    original.output.limited_to_album_releases = true;
    let mut queue = Queue::default();
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/album").unwrap()],
        &original,
        "Waiting",
    );
    let plan = Arc::new(ResolvedResource {
        edition: None,
        title: "Release".into(),
        tracks: vec![track("1", "album", 1, 1)],
        thumbnail_url: None,
        thumbnail: None,
    });
    let job = queue.jobs.get_mut(&1).unwrap();
    job.phase = Phase::Finished;
    job.dto.status = Status::Failed;
    plan_booklet_rows(job, &plan);
    job.plan = Some(plan.clone());
    let mut current = Settings::default();
    current.output.quality = 2;
    current.output.smart_discography = false;
    current.max_connections = crate::storage::MaxConnections::Count(2);
    queue.retry(1, &current, "Waiting", None).unwrap();
    let job = &queue.jobs[&1];
    assert!(job.settings.output.booklet_only);
    assert!(job.settings.output.limited_to_album_releases);
    assert!(job.settings.output.smart_discography);
    assert_eq!(job.settings.output.quality, 2);
    assert_eq!(job.settings.max_connections.limit(), 2);
    assert!(Arc::ptr_eq(job.plan.as_ref().unwrap(), &plan));
}

#[test]
fn completed_audio_can_retry_all_unfinished_outputs_in_one_attempt() {
    let mut queue = active_queue();
    let job = queue.jobs.get_mut(&1).unwrap();
    job.settings.output.lyrics_mode = LyricsMode::External;
    job.settings.output.download_cover_art = true;
    job.settings.output.create_playlist = true;
    complete(&mut job.dto.files[0], "saved.flac".into(), None, Some(20));
    let track = track("1", "album", 1, 1);
    let kinds = [FileKind::Lyrics, FileKind::Lyrics, FileKind::Artwork, FileKind::Booklet, FileKind::Playlist];
    for (index, kind) in kinds.into_iter().enumerate() {
        let mut row = item(index as u64 + 2, kind, "Pending output".into(), &track, None);
        fail(&mut row, "Timed out".into());
        job.dto.files.push(row);
    }
    let mut unavailable = item(7, FileKind::Lyrics, "No lyrics".into(), &track, None);
    lyric_failure(&mut unavailable, None);
    job.dto.files.push(unavailable);
    let mut current = Settings::default();
    current.output.lyrics_mode = LyricsMode::Off;
    current.output.download_cover_art = false;
    current.output.create_playlist = false;
    assert!(!queue.snapshot().jobs[0].can_retry);
    assert!(queue.retry(1, &current, "Retry", None).is_err());

    let job = queue.jobs.get_mut(&1).unwrap();
    job.running = false;
    job.phase = Phase::Finished;
    job.dto.status = Status::Completed;
    assert!(queue.snapshot().jobs[0].can_retry);
    queue.retry(1, &current, "Retry", None).unwrap();
    let job = &queue.jobs[&1];
    assert!(job.settings.output.lyrics_mode == LyricsMode::External);
    assert!(job.settings.output.download_cover_art);
    assert!(job.settings.output.create_playlist);
    assert!(job.dto.files[0].status == Status::Completed);
    assert_eq!(job.dto.files[0].path.as_deref(), Some(Path::new("saved.flac")));
    assert_eq!(job.dto.files[0].generation, 0);
    assert!(job.dto.files[6].status == Status::Unavailable);
    assert!(job.dto.files[1..6].iter().all(|row| row.status == Status::Queued && row.generation == job.generation));
}

#[test]
fn pdf_confirmation_deduplicates_repeated_releases_across_a_submission() {
    let mut settings = Settings::default();
    settings.output.booklet_only = true;
    let mut queue = Queue::default();
    queue.enqueue(
        ["100", "101"]
            .map(|id| normalize_link(&format!("https://qobuz.com/playlist/{id}")).unwrap())
            .into(),
        &settings,
        "Waiting",
    );
    let tracks = (0..100)
        .map(|index| track(&index.to_string(), &format!("album{index}"), 1, 1))
        .collect();
    let plan = Arc::new(ResolvedResource {
        edition: None,
        title: "Set".into(),
        tracks,
        thumbnail_url: None,
        thumbnail: None,
    });
    for job in queue.jobs.values_mut() {
        plan_booklet_rows(job, &plan);
        job.plan = Some(plan.clone());
        job.phase = Phase::Held;
    }
    queue.reconsider(1);
    let confirmation = queue.snapshot().confirmations.pop().unwrap();
    assert_eq!(confirmation.unit, "pdfs");
    assert_eq!(confirmation.track_count, 100);
}

#[test]
fn missing_names_keep_the_original_release_sequence_and_repeated_identity() {
    let plan = ResolvedResource {
        edition: None,
        title: "Set".into(),
        thumbnail_url: None,
        thumbnail: None,
        tracks: vec![
            track("1", "a", 1, 0),
            track("2", "a", 1, 0),
            track("3", "a", 1, 0),
            track("4", "b", 1, 0),
            track("2", "a", 1, 0),
            track("5", "b", 2, 0),
        ],
    };
    assert_eq!(
        (0..plan.tracks.len())
            .map(|index| fallback_sequence(&plan, index))
            .collect::<Vec<_>>(),
        [1, 2, 3, 1, 2, 1]
    );
}

#[test]
fn applying_a_mode_change_invalidates_pending_confirmation_and_keeps_active_settings() {
    let mut queue = Queue::default();
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/1").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/2").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Held;
    queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
    let mut settings = Settings::default();
    settings.output.booklet_only = true;
    settings.max_connections = crate::storage::MaxConnections::Count(8);
    queue.apply(None, &settings);
    assert!(queue.jobs[&1].dto.mode == JobMode::Booklet);
    assert!(queue.jobs[&1].phase == Phase::Waiting);
    assert!(queue.jobs[&2].dto.mode == JobMode::Audio);
    assert_eq!(queue.jobs[&2].settings.max_connections.limit(), 1);
}

#[test]
fn sequential_head_uses_visible_order_across_pending_and_removed_jobs() {
    let mut queue = Queue::default();
    let mut settings = Settings::default();
    let recovery = crate::recovery::Recovery::default();
    queue.enqueue(
        (1..=3)
            .map(|id| normalize_link(&format!("https://qobuz.com/album/{id}")).unwrap())
            .collect(),
        &settings,
        "Waiting",
    );
    queue.jobs.get_mut(&2).unwrap().phase = Phase::Eligible;
    assert_eq!(queue.admission_head(), Some(1));
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Held;
    assert_eq!(queue.admission_head(), Some(1));
    queue.cancel(1, false).unwrap();
    assert_eq!(queue.admission_head(), Some(2));
    queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
    queue.admitted.insert(2);
    queue.set_user_pause(true, &recovery);
    settings.download_mode = DownloadMode::Concurrent;
    queue.settings_saved(&settings, false, &recovery);
    assert_eq!(queue.snapshot().effective_mode, DownloadMode::Sequential);
    assert_eq!(queue.snapshot().requested_mode, DownloadMode::Concurrent);
    assert!(queue.snapshot().paused);
    settings.download_mode = DownloadMode::Sequential;
    queue.settings_saved(&settings, false, &recovery);
    assert_eq!(
        queue.snapshot().requested_mode,
        queue.snapshot().effective_mode
    );
    settings.download_mode = DownloadMode::Concurrent;
    queue.settings_saved(&settings, false, &recovery);
    queue.cancel(2, true).unwrap();
    assert!(queue.active());
    assert_eq!(queue.admission_head(), Some(3));
    queue.finish_admission(2, &recovery);
    assert_eq!(queue.snapshot().effective_mode, DownloadMode::Concurrent);
    queue.cancel(3, true).unwrap();
    queue.reset_idle_pause(&recovery);
    assert!(!queue.snapshot().paused);
}

#[test]
fn saved_limit_is_distinct_from_the_mixed_live_queue_until_explicit_save() {
    let mut settings = Settings {
        download_mode: DownloadMode::Concurrent,
        ..Settings::default()
    };
    let recovery = crate::recovery::Recovery::default();
    let mut queue = Queue::new(&settings);
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/a").unwrap()],
        &settings,
        "Waiting",
    );
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Active;
    queue.admitted.insert(1);
    settings.max_connections = MaxConnections::Count(4);
    queue.settings_saved(&settings, false, &recovery);
    assert_eq!(queue.snapshot().live_limit, Some(MaxConnections::Count(1)));
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/b").unwrap()],
        &settings,
        "Waiting",
    );
    assert_eq!(queue.snapshot().live_limit, None);
    queue.apply(None, &settings);
    assert_eq!(queue.snapshot().live_limit, None);
    queue.settings_saved(&settings, true, &recovery);
    assert_eq!(queue.snapshot().live_limit, Some(MaxConnections::Count(4)));
    assert_eq!(
        queue.jobs[&1].settings.max_connections,
        MaxConnections::Count(4)
    );
}

#[test]
fn groups_follow_first_occurrence_and_keep_playlist_indices() {
    let plan = ResolvedResource {
        edition: None,
        title: "Playlist".into(),
        tracks: vec![
            track("1", "z", 1, 1),
            track("2", "a", 1, 1),
            track("3", "z", 2, 1),
        ],
        thumbnail_url: None,
        thumbnail: None,
    };
    assert_eq!(release_groups(&plan), [("z", vec![0, 2]), ("a", vec![1])]);
}

#[test]
fn artist_resolution_creates_independent_album_rows_in_queue_order() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    queue.enqueue(vec![
        normalize_link("https://play.qobuz.com/artist/123").unwrap(),
        normalize_link("https://play.qobuz.com/album/unrelated").unwrap(),
    ], &settings, "Waiting");
    let plan = ResolvedResource {
        edition: None,
        title: "Artist".into(),
        tracks: vec![track("1", "first", 1, 1), track("2", "second", 1, 1), track("3", "first", 2, 1)],
        thumbnail_url: None,
        thumbnail: None,
    };
    queue.install_album_rows(1, &plan);
    assert_eq!(queue.snapshot().jobs.iter().map(|job| job.id).collect::<Vec<_>>(), [1, 3, 2]);
    assert_eq!(queue.jobs[&1].link.key.id, "first");
    assert_eq!(queue.jobs[&3].link.key.id, "second");
    assert_eq!(queue.jobs[&1].dto.files.len(), 2);
    assert_eq!(queue.jobs[&3].dto.files.len(), 1);
    assert_eq!(queue.batches[&1].jobs, [1, 3, 2]);
    queue.reconsider(1);
    queue.cancel(1, false).unwrap();
    assert_eq!(queue.admission_head(), Some(3));
    assert!(queue.jobs[&1].dto.status == Status::Cancelled);
    assert!(queue.jobs[&3].dto.status == Status::Queued);
    queue.retry(1, &settings, "Retry", None).unwrap();
    assert_eq!(queue.jobs[&1].plan.as_ref().unwrap().tracks.len(), 2);
    assert!(matches!(queue.jobs[&3].phase, Phase::Held | Phase::Eligible));
    queue.cancel(3, true).unwrap();
    assert!(!queue.jobs.contains_key(&3));
    assert!(queue.jobs.contains_key(&1));
}

#[test]
fn playlist_exports_once_from_first_remaining_album_in_original_order() {
    let mut settings = Settings::default();
    settings.output.create_playlist = true;
    let mut queue = Queue::default();
    queue.enqueue(vec![normalize_link("https://play.qobuz.com/playlist/10").unwrap()], &settings, "Waiting");
    let plan = ResolvedResource {
        edition: None,
        title: "Original playlist".into(),
        tracks: vec![track("1", "first", 1, 1), track("2", "second", 1, 1), track("1", "first", 1, 1)],
        thumbnail_url: None,
        thumbnail: None,
    };
    queue.install_album_rows(1, &plan);
    assert_eq!(queue.jobs[&1].playlist.as_ref().unwrap().positions, [0, 2]);
    assert_eq!(queue.jobs[&2].playlist.as_ref().unwrap().positions, [1]);
    for (id, paths) in [(1, vec!["a.flac", "a.flac"]), (2, vec!["b.flac"])] {
        let job = queue.jobs.get_mut(&id).unwrap();
        for (index, path) in paths.into_iter().enumerate() {
            job.paths.insert(index, path.into());
            complete(&mut job.dto.files[index], path.into(), None, None);
        }
        job.phase = Phase::Finished;
        job.dto.status = Status::Completed;
        queue.remember_playlist_paths(id);
        if id == 1 {
            assert!(queue.ready_playlist_exports().is_empty());
        }
    }
    assert_eq!(queue.ready_playlist_exports(), [(1, 1)]);
    queue.queue_playlist_exports();
    assert!(queue.ready_playlist_exports().is_empty());
    assert!(!queue.jobs[&2].dto.files.iter().any(|file| file.kind == FileKind::Playlist));
    queue.cancel(1, true).unwrap();
    assert_eq!(queue.ready_playlist_exports(), [(1, 2)]);
    queue.queue_playlist_exports();
    let job = &queue.jobs[&2];
    let task = RunTask {
        id: 2,
        generation: job.generation,
        settings: job.settings.clone(),
        root: "test-root".into(),
        plan: job.plan.clone().unwrap(),
        cancel: job.cancel.clone(),
        audio: Vec::new(),
        audio_file_ids: HashMap::new(),
        only_file: job.only_file,
        account_epoch: 0,
        search_origin: false,
        kind: ResourceKind::Album,
        replacement: None,
        mode: DownloadMode::Sequential,
    };
    let export = playlist_input(&mut queue, &task).unwrap();
    assert_eq!(export.title, "Original playlist");
    assert_eq!(export.source_id, "10");
    assert_eq!(export.paths, ["a.flac", "b.flac", "a.flac"].map(PathBuf::from));
    assert_eq!(queue.jobs[&2].dto.files.iter().filter(|file| file.kind == FileKind::Playlist).count(), 1);
    assert!(queue.jobs[&2].dto.files[0].status == Status::Completed);
}

#[tokio::test]
async fn deferred_audio_waits_for_the_first_pass_then_replays_once_in_plan_order() {
    use futures_util::poll;
    use std::cell::RefCell;
    let started = RefCell::new(Vec::new());
    let replayed = RefCell::new(Vec::new());
    let (first, first_done) = tokio::sync::oneshot::channel();
    let (second, second_done) = tokio::sync::oneshot::channel();
    let (third, third_done) = tokio::sync::oneshot::channel();
    let pending = stream::iter([first_done, second_done, third_done].into_iter().enumerate())
        .map(|(index, settled)| {
            let started = &started;
            async move {
                started.borrow_mut().push(index + 1);
                settled.await.unwrap()
            }
        })
        .buffer_unordered(3);
    let cancel = CancellationToken::new();
    let mut stage = std::pin::pin!(settle_audio_pass(pending, vec![0], &cancel, |index| {
        replayed.borrow_mut().push(index);
        std::future::ready(())
    }));
    assert!(poll!(&mut stage).is_pending());
    assert_eq!(*started.borrow(), [1, 2, 3]);
    third.send(Some(3)).unwrap();
    first.send(Some(1)).unwrap();
    assert!(poll!(&mut stage).is_pending());
    assert!(replayed.borrow().is_empty());
    second.send(None).unwrap();
    stage.await;
    assert_eq!(*replayed.borrow(), [0, 1, 3]);
}

#[tokio::test]
async fn active_output_stage_waits_for_drive_retry_without_releasing_queue_ownership() {
    use futures_util::poll;
    use std::pin::pin;
    let directory = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(directory.path()).unwrap();
    let app = App::initialize_root(root.clone()).unwrap();
    let settings = Settings::default();
    let mut queue = active_queue();
    queue.admitted.insert(1);
    let job = queue.jobs.get_mut(&1).unwrap();
    let plan = Arc::new(ResolvedResource {
        edition: None,
        title: "Release".into(),
        tracks: vec![track("1", "album", 1, 1)],
        thumbnail_url: None,
        thumbnail: None,
    });
    job.plan = Some(plan.clone());
    let task = RunTask {
        id: 1,
        generation: 0,
        settings: settings.clone(),
        root: root.clone(),
        plan,
        cancel: job.cancel.clone(),
        audio: vec![0],
        audio_file_ids: HashMap::new(),
        only_file: None,
        account_epoch: 0,
        search_origin: false,
        kind: ResourceKind::Album,
        replacement: None,
        mode: DownloadMode::Sequential,
    };
    app.state().queue = queue;
    let transfer = transfer_context(&app, &task, 1);
    let mut attempts = 0;
    let mut run = pin!(run_file_stage(&app, &task, 1, &transfer, || {
        attempts += 1;
        std::future::ready(if attempts == 1 {
            Err(media::MediaError {
                code: "disk_full",
                message: "Disk full".into(),
                retryable: true,
                path: Some(root.clone()),
            })
        } else {
            Ok(())
        })
    }));
    assert!(poll!(&mut run).is_pending());
    {
        let mut state = app.state();
        assert!(state.queue.snapshot().jobs[0].drive_blocked);
        assert!(state.queue.active());
        state.queue.retry(1, &settings, "Retry", None).unwrap();
    }
    schedule(&app);
    assert!(run.await.is_ok());
    assert!(!app.state().queue.snapshot().jobs[0].drive_blocked);
}

#[test]
fn a_pinned_album_uses_its_established_drive_when_current_destination_differs() {
    let mut queue = Queue::default();
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/a").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    let job = queue.jobs.get_mut(&1).unwrap();
    job.plan = Some(Arc::new(ResolvedResource {
        edition: None,
        title: "Release".into(),
        tracks: vec![track("1", "a", 1, 1)],
        thumbnail_url: None,
        thumbnail: None,
    }));
    let pinned = PathBuf::from("C:\\Music\\Album");
    job.pinned.insert(
        "a".into(),
        ReleaseLayout {
            album_root: pinned.clone(),
            cover_dir: None,
            disc_dirs: BTreeMap::new(),
        },
    );
    assert_eq!(
        write_destinations(job, Path::new("D:\\New Destination"), &[0]),
        [pinned]
    );
}

#[test]
fn dialog_quality_follows_the_accepted_retry_apply_and_again_precedence() {
    let fixture = fixture();
    let lifecycle = &fixture["qualityLifecycle"];
    let mut settings = Settings::default();
    settings.output.quality = lifecycle["initialGlobalTier"].as_u64().unwrap() as u8;
    let mut queue = Queue::default();
    queue.enqueue_with_quality(
        vec![normalize_link("https://qobuz.com/album/1").unwrap()],
        &settings,
        "Waiting",
        Some(lifecycle["dialogTier"].as_u64().unwrap() as u8),
    );
    assert_eq!(settings.output.quality, 4);
    settings.output.quality = lifecycle["globalTierAfterSubmission"].as_u64().unwrap() as u8;
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.dto.status = Status::Failed;
    }
    queue.retry(1, &settings, "Waiting", None).unwrap();
    assert_eq!(
        u64::from(queue.jobs[&1].settings.output.quality),
        lifecycle["expectedOriginalJobAfterRetry"].as_u64().unwrap()
    );
    queue.apply(Some(vec![1]), &settings);
    assert_eq!(
        u64::from(queue.jobs[&1].settings.output.quality),
        lifecycle["expectedQueuedJobAfterExplicitApply"]
            .as_u64()
            .unwrap()
    );
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.dto.status = Status::Failed;
    }
    queue.retry(1, &settings, "Waiting", None).unwrap();
    assert_eq!(
        u64::from(queue.jobs[&1].settings.output.quality),
        lifecycle["expectedRetryAfterApply"].as_u64().unwrap()
    );
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.dto.status = Status::Failed;
    }
    queue.download_again(1, &settings, "Waiting").unwrap();
    assert_eq!(
        u64::from(queue.jobs[&2].settings.output.quality),
        lifecycle["expectedDownloadAgain"].as_u64().unwrap()
    );
    assert_eq!(queue.jobs[&2].quality_override, None);
}

#[test]
fn same_percentage_bytes_unknown_totals_and_stale_attempts_match_reference_telemetry() {
    let fixture = fixture();
    let mut queue = active_queue();
    let zero = &fixture["telemetry"][0];
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 1,
            phase: TransferPhase::Transferring,
            received: zero["receivedBytes"].as_u64().unwrap(),
            total: zero["totalBytes"].as_u64(),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].files[0].progress, Some(0));
    let same_percent = &fixture["telemetry"][2];
    for received in same_percent["receivedSequenceBytes"].as_array().unwrap() {
        let previous_revision = queue.revision;
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Transferring,
                received: received.as_u64().unwrap(),
                total: same_percent["totalBytes"].as_u64(),
            },
        );
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.jobs[0].files[0].progress, Some(42));
        assert_eq!(
            snapshot.jobs[0].files[0].received_bytes,
            received.as_u64().unwrap()
        );
        assert!(snapshot.revision > previous_revision);
    }
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 2,
            phase: TransferPhase::Connecting,
            received: 0,
            total: None,
        },
    );
    let unknown = &fixture["telemetry"][1];
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 2,
            phase: TransferPhase::Transferring,
            received: unknown["receivedBytes"].as_u64().unwrap(),
            total: None,
        },
    );
    let before = queue.revision;
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 1,
            phase: TransferPhase::Processing,
            received: 100_000_000,
            total: Some(100_000_000),
        },
    );
    queue.progress(
        1,
        8,
        1,
        TransferProgress {
            attempt: 3,
            phase: TransferPhase::Processing,
            received: 100_000_000,
            total: Some(100_000_000),
        },
    );
    assert_eq!(queue.revision, before);
    let file = &queue.snapshot().jobs[0].files[0];
    assert_eq!(file.attempt, 2);
    assert_eq!(file.progress, None);
    assert_eq!(file.received_bytes, 11_400_000);
}

#[test]
fn release_completion_waits_for_processing_all_requested_files_and_terminal_job_state() {
    let mut queue = active_queue();
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 1,
            phase: TransferPhase::Processing,
            received: 100,
            total: Some(100),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(99));
    let job = queue.jobs.get_mut(&1).unwrap();
    complete(
        &mut job.dto.files[0],
        PathBuf::from("track.flac"),
        None,
        Some(100),
    );
    let source = track("1", "album", 1, 1);
    let mut cover = item(2, FileKind::Artwork, "Cover".into(), &source, None);
    complete(&mut cover, PathBuf::from("cover.jpg"), None, Some(20));
    job.dto.files.push(cover);
    job.dto
        .files
        .push(item(3, FileKind::Booklet, "Booklet".into(), &source, None));
    assert_eq!(release_progress(job), Some(66));
    complete(
        &mut job.dto.files[2],
        PathBuf::from("booklet.pdf"),
        None,
        Some(30),
    );
    assert_eq!(release_progress(job), Some(99));
    job.phase = Phase::Finished;
    job.dto.status = Status::Completed;
    assert_eq!(release_progress(job), Some(100));
    fail(&mut job.dto.files[2], "Booklet failed".into());
    assert_eq!(release_progress(job), Some(66));
}

#[test]
fn release_progress_reserves_optional_slots_and_discards_partial_attempts() {
    let mut queue = active_queue();
    let source = track("1", "album", 1, 1);
    let job = queue.jobs.get_mut(&1).unwrap();
    complete(
        &mut job.dto.files[0],
        PathBuf::from("track.flac"),
        None,
        Some(100),
    );
    let mut second = item(2, FileKind::Audio, "Track 2".into(), &source, Some(1));
    second.status = Status::Downloading;
    job.dto.files.push(second);
    job.playlist_reserved = true;
    queue.progress(
        1,
        0,
        2,
        TransferProgress {
            attempt: 2,
            phase: TransferPhase::Transferring,
            received: 50,
            total: Some(100),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
    let job = queue.jobs.get_mut(&1).unwrap();
    job.playlist_reserved = false;
    job.dto.files.push(item(
        3,
        FileKind::Playlist,
        "Playlist".into(),
        &source,
        None,
    ));
    assert_eq!(release_progress(job), Some(50));
    job.dto.files.pop();
    queue.progress(
        1,
        0,
        2,
        TransferProgress {
            attempt: 2,
            phase: TransferPhase::Transferring,
            received: 80,
            total: Some(100),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(90));
    queue.progress(
        1,
        0,
        2,
        TransferProgress {
            attempt: 3,
            phase: TransferPhase::Connecting,
            received: 0,
            total: None,
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
    queue.progress(
        1,
        0,
        2,
        TransferProgress {
            attempt: 2,
            phase: TransferPhase::Transferring,
            received: 95,
            total: Some(100),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
    let job = queue.jobs.get_mut(&1).unwrap();
    for id in 2..=3 {
        if id == 3 {
            job.dto.files.push(item(
                id,
                FileKind::Audio,
                "Track 3".into(),
                &source,
                Some(1),
            ));
        }
        update_file(job, id, |file| {
            complete(file, PathBuf::from("track.flac"), None, Some(100))
        });
    }
    let mut booklet = item(4, FileKind::Booklet, "Booklet".into(), &source, None);
    booklet.status = Status::Downloading;
    job.dto.files.push(booklet);
    assert_eq!(release_progress(job), Some(75));
    queue.progress(
        1,
        0,
        4,
        TransferProgress {
            attempt: 4,
            phase: TransferPhase::Transferring,
            received: 60,
            total: Some(100),
        },
    );
    assert_eq!(queue.snapshot().jobs[0].progress, Some(90));
}
#[test]
fn rate_counts_concurrent_network_deltas_and_returns_to_zero_after_the_window() {
    let fixture = fixture();
    let telemetry = &fixture["telemetry"][3];
    let now = Instant::now();
    let mut window = RateWindow::default();
    for bytes in telemetry["activeTransferDeltasBytes"].as_array().unwrap() {
        window.record(now, bytes.as_u64().unwrap());
    }
    assert_eq!(
        window.bytes_per_second(now + Duration::from_millis(999)),
        telemetry["expectedBytesPerSecond"].as_u64().unwrap()
    );
    assert_eq!(
        window.bytes_per_second(now + Duration::from_millis(1000)),
        0
    );
    let mut queue = active_queue();
    complete(
        &mut queue.jobs.get_mut(&1).unwrap().dto.files[0],
        PathBuf::from("existing.flac"),
        None,
        Some(9_000_000),
    );
    queue.refresh_rate(now);
    assert_eq!(queue.snapshot().bytes_per_second, 0);
}

#[tokio::test]
async fn final_sizes_come_from_published_files_instead_of_transfer_length() {
    let fixture = fixture();
    let telemetry = &fixture["telemetry"][4];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("published.flac");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(telemetry["finalSizeBytes"].as_u64().unwrap())
        .unwrap();
    let mut queue = active_queue();
    queue.progress(
        1,
        0,
        1,
        TransferProgress {
            attempt: 1,
            phase: TransferPhase::Transferring,
            received: telemetry["receivedBytes"].as_u64().unwrap(),
            total: telemetry["totalBytes"].as_u64(),
        },
    );
    let size = final_size(&path).await;
    complete(
        &mut queue.jobs.get_mut(&1).unwrap().dto.files[0],
        path,
        None,
        size,
    );
    let file = &queue.snapshot().jobs[0].files[0];
    assert_eq!(file.final_size_bytes, Some(27_250_000));
    assert_eq!(file.received_bytes, 11_400_000);
    assert_eq!(file.total_bytes, Some(27_200_000));
}

#[test]
fn clear_downloads_classifies_under_lock_preserves_active_jobs_and_rejects_late_events() {
    let fixture = fixture();
    let mut queue = Queue {
        next_id: 100,
        ..Queue::default()
    };
    let links = (1..=9)
        .map(|id| normalize_link(&format!("https://qobuz.com/album/{id}")).unwrap())
        .collect();
    queue.enqueue(links, &Settings::default(), "Waiting");
    let source = track("1", "album", 1, 1);
    let mut removed_tokens = Vec::new();
    for case in fixture["selectiveClear"]["jobs"].as_array().unwrap() {
        let id = case["id"].as_u64().unwrap();
        let job = queue.jobs.get_mut(&id).unwrap();
        job.phase = match case["internalPhase"].as_str().unwrap() {
            "Active" => Phase::Active,
            "Eligible" => Phase::Eligible,
            "Planning" => Phase::Planning,
            "Held" => Phase::Held,
            "Finished" => Phase::Finished,
            _ => unreachable!(),
        };
        job.dto
            .files
            .push(item(1, FileKind::Audio, "Track".into(), &source, None));
        if !case["protected"].as_bool().unwrap() {
            removed_tokens.push(job.cancel.clone());
        }
    }
    queue.batches.get_mut(&1).unwrap().confirmation = Some(Confirmation {
        id: 1,
        plan_revision: 1,
        track_count: 100,
        unit: "tracks",
    });
    let report = queue.clear_downloads();
    assert_eq!((report.removed, report.protected), (6, 3));
    assert_eq!(
        queue.jobs.keys().copied().collect::<Vec<_>>(),
        [101, 102, 103]
    );
    assert!(removed_tokens.iter().all(CancellationToken::is_cancelled));
    assert!(queue.jobs.values().all(|job| !job.cancel.is_cancelled()));
    assert!(!queue.snapshot().can_clear);
    assert_eq!(queue.snapshot().protected_count, 3);
    assert!(queue.snapshot().confirmations.is_empty());
    let revision = queue.revision;
    queue.progress(
        105,
        0,
        1,
        TransferProgress {
            attempt: 1,
            phase: TransferPhase::Transferring,
            received: 1,
            total: Some(10),
        },
    );
    assert_eq!(queue.revision, revision);
    assert_eq!(queue.jobs.len(), 3);
}

#[test]
fn clear_keeps_a_queued_job_that_became_active_before_execution_and_preserves_files() {
    let directory = tempfile::tempdir().unwrap();
    let finalized = directory.path().join("saved.flac");
    std::fs::write(&finalized, b"finalized media").unwrap();
    let mut queue = active_queue();
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Eligible;
    queue.enqueue(
        vec![normalize_link("https://qobuz.com/album/2").unwrap()],
        &Settings::default(),
        "Waiting",
    );
    assert!(queue.snapshot().can_clear);
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Active;
    queue.jobs.get_mut(&2).unwrap().dto.files.push(item(
        1,
        FileKind::Audio,
        "Saved".into(),
        &track("2", "album", 1, 1),
        None,
    ));
    complete(
        &mut queue.jobs.get_mut(&2).unwrap().dto.files[0],
        finalized.clone(),
        None,
        Some(15),
    );
    let report = queue.clear_downloads();
    assert_eq!((report.removed, report.protected), (1, 1));
    assert!(queue.jobs.contains_key(&1));
    assert_eq!(std::fs::read(finalized).unwrap(), b"finalized media");
    queue.shutdown();
    assert!(!queue.snapshot().can_clear);
}

#[test]
fn queue_dtos_expose_album_version_for_albums_tracks_and_split_rows() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    queue.enqueue(
        vec![
            normalize_link("https://play.qobuz.com/album/album1").unwrap(),
            normalize_link("https://play.qobuz.com/track/track9").unwrap(),
            normalize_link("https://play.qobuz.com/artist/artist1").unwrap(),
        ],
        &settings,
        "Waiting",
    );

    let versioned_album = Arc::new(serde_json::json!({
        "id": "album1",
        "title": "Moonlight",
        "version": "Deluxe Edition",
        "artist": {"name": "Aster Vale"},
        "media_count": 1
    }));
    let album_track = ResolvedTrack {
        id: "1".into(),
        title: "Starlight".into(),
        album_id: "album1".into(),
        disc: 1,
        number: 1,
        track: serde_json::json!({"id": 1, "title": "Starlight"}),
        album: versioned_album.clone(),
        unavailable: None,
    };
    let album_plan = Arc::new(ResolvedResource {
        title: "Moonlight".into(),
        edition: crate::qobuz::album_version(&versioned_album),
        tracks: vec![album_track],
        thumbnail_url: None,
        thumbnail: None,
    });
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        assert_eq!(job.dto.edition, None);
        job.dto.title = album_plan.title.clone();
        job.dto.edition.clone_from(&album_plan.edition);
        plan_file_rows(job, &album_plan);
        job.plan = Some(album_plan);
    }

    let single_album = Arc::new(serde_json::json!({
        "id": "album2",
        "title": "Original Score",
        "version": "(20th Anniversary)",
        "artist": {"name": "Composer"},
        "media_count": 1
    }));
    let single_track = ResolvedTrack {
        id: "9".into(),
        title: "Opening Theme (Radio Edit)".into(),
        album_id: "album2".into(),
        disc: 1,
        number: 1,
        track: serde_json::json!({"id": 9, "title": "Opening Theme", "version": "Radio Edit"}),
        album: single_album.clone(),
        unavailable: None,
    };
    let track_plan = Arc::new(ResolvedResource {
        title: "Opening Theme".into(),
        edition: crate::qobuz::album_version(&single_album),
        tracks: vec![single_track],
        thumbnail_url: None,
        thumbnail: None,
    });
    {
        let job = queue.jobs.get_mut(&2).unwrap();
        job.dto.title = track_plan.title.clone();
        job.dto.edition.clone_from(&track_plan.edition);
        plan_file_rows(job, &track_plan);
        job.plan = Some(track_plan);
    }

    let first_split_album = Arc::new(serde_json::json!({
        "id": "first",
        "title": "First Album",
        "version": "Remastered",
        "artist": {"name": "Artist"},
        "media_count": 1
    }));
    let second_split_album = Arc::new(serde_json::json!({
        "id": "second",
        "title": "Second Album (Live)",
        "version": "live",
        "artist": {"name": "Artist"},
        "media_count": 1
    }));
    let third_split_album = Arc::new(serde_json::json!({
        "id": "third",
        "title": "Plain Album",
        "version": null,
        "artist": {"name": "Artist"},
        "media_count": 1
    }));
    let artist_plan = ResolvedResource {
        title: "Artist".into(),
        edition: None,
        tracks: vec![
            ResolvedTrack {
                id: "101".into(),
                title: "One".into(),
                album_id: "first".into(),
                disc: 1,
                number: 1,
                track: serde_json::json!({"id": 101, "title": "One"}),
                album: first_split_album,
                unavailable: None,
            },
            ResolvedTrack {
                id: "102".into(),
                title: "Two".into(),
                album_id: "second".into(),
                disc: 1,
                number: 1,
                track: serde_json::json!({"id": 102, "title": "Two"}),
                album: second_split_album,
                unavailable: None,
            },
            ResolvedTrack {
                id: "103".into(),
                title: "Three".into(),
                album_id: "third".into(),
                disc: 1,
                number: 1,
                track: serde_json::json!({"id": 103, "title": "Three"}),
                album: third_split_album,
                unavailable: None,
            },
        ],
        thumbnail_url: None,
        thumbnail: None,
    };
    queue.install_album_rows(3, &artist_plan);

    let snapshot = queue.snapshot();
    let album_job = snapshot.jobs.iter().find(|job| job.id == 1).unwrap();
    assert_eq!(album_job.title, "Moonlight");
    assert_eq!(album_job.edition.as_deref(), Some("(Deluxe Edition)"));
    assert_eq!(album_job.files[0].album.as_deref(), Some("Moonlight (Deluxe Edition)"));
    assert_eq!(album_job.files[0].edition.as_deref(), Some("(Deluxe Edition)"));

    let track_job = snapshot.jobs.iter().find(|job| job.id == 2).unwrap();
    assert_eq!(track_job.title, "Opening Theme");
    assert_eq!(track_job.edition.as_deref(), Some("(20th Anniversary)"));
    assert_eq!(track_job.files[0].album.as_deref(), Some("Original Score (20th Anniversary)"));
    assert_eq!(track_job.files[0].edition.as_deref(), Some("(20th Anniversary)"));

    let split_first = snapshot.jobs.iter().find(|job| job.id == 3).unwrap();
    assert_eq!(split_first.title, "First Album");
    assert_eq!(split_first.edition.as_deref(), Some("(Remastered)"));
    assert_eq!(split_first.files[0].album.as_deref(), Some("First Album (Remastered)"));
    assert_eq!(split_first.files[0].edition.as_deref(), Some("(Remastered)"));

    let split_second = snapshot.jobs.iter().find(|job| job.id == 4).unwrap();
    assert_eq!(split_second.title, "Second Album (Live)");
    assert_eq!(split_second.edition.as_deref(), Some("(live)"));
    assert_eq!(split_second.files[0].album.as_deref(), Some("Second Album (Live)"));
    assert_eq!(split_second.files[0].edition.as_deref(), Some("(live)"));

    let split_third = snapshot.jobs.iter().find(|job| job.id == 5).unwrap();
    assert_eq!(split_third.title, "Plain Album");
    assert_eq!(split_third.edition, None);
    assert_eq!(split_third.files[0].album.as_deref(), Some("Plain Album"));
    assert_eq!(split_third.files[0].edition, None);

    let serialized_job = serde_json::to_value(split_first).unwrap();
    assert_eq!(serialized_job["edition"], "(Remastered)");
    assert_eq!(serialized_job["files"][0]["album"], "First Album (Remastered)");
    assert_eq!(serialized_job["files"][0]["edition"], "(Remastered)");

    let split_job = queue.jobs.get_mut(&3).unwrap();
    split_job.phase = Phase::Finished;
    split_job.running = false;
    split_job.dto.status = Status::Completed;
    queue.download_again(3, &settings, "Waiting").unwrap();
    let redownload = queue.snapshot().jobs.into_iter().find(|job| job.id == 6).unwrap();
    assert_eq!(redownload.title, "First Album");
    assert_eq!(redownload.edition.as_deref(), Some("(Remastered)"));
    assert_eq!(redownload.files[0].album.as_deref(), Some("First Album (Remastered)"));
    assert_eq!(redownload.files[0].edition.as_deref(), Some("(Remastered)"));

    let mut booklet_settings = Settings::default();
    booklet_settings.output.booklet_only = true;
    queue.enqueue(
        vec![normalize_link("https://play.qobuz.com/playlist/playlist9").unwrap()],
        &booklet_settings,
        "Waiting",
    );
    let booklet_album = Arc::new(serde_json::json!({
        "id": "bookalbum",
        "title": "Booklet Album",
        "version": "Collector's Edition",
        "artist": {"name": "Artist"},
        "media_count": 1,
        "goodies": [
            {"id": "book1", "file_format_id": 21, "name": "Digital Booklet", "url": "https://example.test/booklet.pdf"}
        ]
    }));
    let booklet_plan = ResolvedResource {
        title: "Playlist".into(),
        edition: None,
        tracks: vec![ResolvedTrack {
            id: "201".into(),
            title: "Track".into(),
            album_id: "bookalbum".into(),
            disc: 1,
            number: 1,
            track: serde_json::json!({"id": 201, "title": "Track"}),
            album: booklet_album,
            unavailable: None,
        }],
        thumbnail_url: None,
        thumbnail: None,
    };
    queue.install_album_rows(7, &booklet_plan);
    let booklet_job = queue.snapshot().jobs.into_iter().find(|job| job.id == 7).unwrap();
    assert_eq!(booklet_job.title, "Booklet Album");
    assert_eq!(booklet_job.edition.as_deref(), Some("(Collector's Edition)"));
    assert!(booklet_job.files[0].kind == FileKind::Booklet);
    assert_eq!(
        booklet_job.files[0].album.as_deref(),
        Some("Booklet Album (Collector's Edition)")
    );
    assert_eq!(
        booklet_job.files[0].edition.as_deref(),
        Some("(Collector's Edition)")
    );
}

#[tokio::test]
async fn scheduler_wakes_immediately_on_enqueue_and_counts_meter_down_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let app = App::initialize_root(root).unwrap();
    let handle = tokio::spawn(crate::queue::scheduler(app.clone()));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let start = Instant::now();
    app.meter.record(1_024 * 1_024);
    app.wake_scheduler();
    let deadline = Instant::now() + Duration::from_millis(120);
    while app.state().queue.snapshot().bytes_per_second == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        start.elapsed() < Duration::from_millis(150),
        "scheduler should wake immediately via Notify without waiting a 200ms tick"
    );
    assert_eq!(app.state().queue.snapshot().bytes_per_second, 1_024 * 1_024);
    let zero_deadline = Instant::now() + Duration::from_millis(1_600);
    while app.state().queue.snapshot().bytes_per_second > 0 && Instant::now() < zero_deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(app.state().queue.snapshot().bytes_per_second, 0);
    app.shutdown().await;
    let _ = handle.await;
}

fn release_track(
    id: &str,
    album_id: &str,
    artist: &str,
    album_title: &str,
    disc: u32,
    number: u32,
    total_tracks: usize,
) -> ResolvedTrack {
    ResolvedTrack {
        id: id.into(),
        title: format!("Track {number}"),
        album_id: album_id.into(),
        disc,
        number,
        track: serde_json::json!({"id": id, "title": format!("Track {number}"), "media_number": disc, "track_number": number}),
        album: Arc::new(serde_json::json!({
            "id": album_id,
            "title": album_title,
            "artist": {"name": artist},
            "tracks_count": total_tracks,
            "media_count": 1,
        })),
        unavailable: None,
    }
}

#[test]
fn single_and_multiple_tracks_merge_into_one_release_row_sorted_with_stable_file_ids() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    let req = QueueEntryRequest {
        link: normalize_link("https://play.qobuz.com/track/505").unwrap(),
        release_id: Some("alb1".into()),
        album_title: Some("Moonlight".into()),
        album_artist: Some("Aster Vale".into()),
        total_tracks: Some(12),
        search_origin: true,
    };
    queue.enqueue_requests(vec![req], &settings, "Waiting", None);
    assert_eq!(queue.jobs.len(), 1);
    let snap = queue.snapshot();
    assert_eq!(snap.jobs[0].title, "Moonlight");
    assert_eq!(snap.jobs[0].resource_kind, ResourceKind::Album);
    assert_eq!(snap.jobs[0].subtitle, "Aster Vale · Album · 1 of 12 tracks");

    let t5 = release_track("505", "alb1", "Aster Vale", "Moonlight", 1, 5, 12);
    let plan1 = Arc::new(ResolvedResource {
        title: "Moonlight".into(),
        edition: None,
        tracks: vec![t5.clone()],
        thumbnail_url: None,
        thumbnail: None,
    });
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        apply_album_plan(job, "Album", plan1, None, None);
        let cover_id = next_file(job);
        let mut cover = item(cover_id, FileKind::Artwork, "Cover artwork".into(), &t5, None);
        cover.status = Status::Completed;
        job.dto.files.push(cover);
        job.extras.insert(
            cover_id,
            Extra::Artwork {
                track: 0,
                directory: PathBuf::from("album"),
                basename: "cover".into(),
                final_target: true,
            },
        );
        job.dto.files[0].status = Status::Completed;
        job.phase = Phase::Finished;
        job.dto.status = Status::Completed;
    }

    let t2 = release_track("502", "alb1", "Aster Vale", "Moonlight", 1, 2, 12);
    let plan2 = ResolvedResource {
        title: "Moonlight".into(),
        edition: None,
        tracks: vec![t2],
        thumbnail_url: None,
        thumbnail: None,
    };
    assert!(queue.merge_resolved_into_job(1, &plan2, true));
    let job = &queue.jobs[&1];
    assert_eq!(job.phase, Phase::Eligible);
    assert_eq!(job.dto.subtitle, "Aster Vale · Album · 2 of 12 tracks");
    // Audio tracks sorted by album disc/track order, artwork remains after audio with no ID collision.
    let titles: Vec<&str> = job.dto.files.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, ["Track 2", "Track 5", "Cover artwork"]);
    let ids: BTreeSet<u64> = job.dto.files.iter().map(|f| f.id).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(job.dto.files[0].status, Status::Queued);
    assert_eq!(job.dto.files[1].status, Status::Completed);
    assert_eq!(job.dto.files[2].status, Status::Completed);
    assert_eq!(job.audio_track_index(job.dto.files[0].id), Some(1));
    assert_eq!(job.audio_track_index(job.dto.files[1].id), Some(0));
}

#[test]
fn merging_into_cancelled_and_failed_release_rows_only_queues_new_tracks() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    let t1 = release_track("1", "alb2", "Artist", "Record", 1, 1, 5);
    let t2 = release_track("2", "alb2", "Artist", "Record", 1, 2, 5);
    queue.enqueue_requests(
        vec![
            QueueEntryRequest {
                link: normalize_link("https://play.qobuz.com/track/1").unwrap(),
                release_id: Some("alb2".into()),
                album_title: Some("Record".into()),
                album_artist: Some("Artist".into()),
                total_tracks: Some(5),
                search_origin: false,
            },
            QueueEntryRequest {
                link: normalize_link("https://play.qobuz.com/track/2").unwrap(),
                release_id: Some("alb2".into()),
                album_title: Some("Record".into()),
                album_artist: Some("Artist".into()),
                total_tracks: Some(5),
                search_origin: false,
            },
        ],
        &settings,
        "Waiting",
        None,
    );
    let plan = Arc::new(ResolvedResource {
        title: "Record".into(),
        edition: None,
        tracks: vec![t1, t2],
        thumbnail_url: None,
        thumbnail: None,
    });
    {
        let job = queue.jobs.get_mut(&1).unwrap();
        apply_album_plan(job, "Album", plan, None, None);
        job.dto.files[0].status = Status::Cancelled;
        job.dto.files[1].status = Status::Failed;
        job.phase = Phase::Finished;
        job.dto.status = Status::Cancelled;
    }

    let t3 = release_track("3", "alb2", "Artist", "Record", 1, 3, 5);
    let extra = ResolvedResource {
        title: "Record".into(),
        edition: None,
        tracks: vec![t3],
        thumbnail_url: None,
        thumbnail: None,
    };
    assert!(queue.merge_resolved_into_job(1, &extra, false));
    let job = &queue.jobs[&1];
    assert_eq!(job.phase, Phase::Eligible);
    assert_eq!(job.dto.files[0].status, Status::Cancelled);
    assert_eq!(job.dto.files[1].status, Status::Failed);
    assert_eq!(job.dto.files[2].status, Status::Queued);
    let pending: Vec<u64> = job
        .dto
        .files
        .iter()
        .filter(|file| is_pending_audio(job, file))
        .map(|file| file.id)
        .collect();
    assert_eq!(pending, [job.dto.files[2].id]);
}

#[test]
fn whole_release_upgrades_partial_row_and_settings_or_playlist_keep_rows_separate() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    let req = QueueEntryRequest {
        link: normalize_link("https://play.qobuz.com/track/10").unwrap(),
        release_id: Some("alb3".into()),
        album_title: Some("Horizon".into()),
        album_artist: Some("Luna".into()),
        total_tracks: Some(3),
        search_origin: true,
    };
    queue.enqueue_requests(vec![req], &settings, "Waiting", None);

    let t10 = release_track("10", "alb3", "Luna", "Horizon", 1, 1, 3);
    let t11 = release_track("11", "alb3", "Luna", "Horizon", 1, 2, 3);
    let t12 = release_track("12", "alb3", "Luna", "Horizon", 1, 3, 3);
    apply_album_plan(
        queue.jobs.get_mut(&1).unwrap(),
        "Album",
        Arc::new(ResolvedResource {
            title: "Horizon".into(),
            edition: None,
            tracks: vec![t10.clone()],
            thumbnail_url: None,
            thumbnail: None,
        }),
        None,
        None,
    );
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Eligible;

    // Duplicate track is detected; new track and whole-album upgrade are NOT duplicates.
    assert!(queue.is_duplicate_request(
        &ResourceKey { kind: ResourceKind::Track, id: "10".into() },
        Some("alb3"),
        &settings
    ));
    assert!(!queue.is_duplicate_request(
        &ResourceKey { kind: ResourceKind::Track, id: "11".into() },
        Some("alb3"),
        &settings
    ));
    assert!(!queue.is_duplicate_request(
        &ResourceKey { kind: ResourceKind::Album, id: "alb3".into() },
        Some("alb3"),
        &settings
    ));

    // Different quality override creates a separate release row instead of merging.
    let mp3_req = QueueEntryRequest {
        link: normalize_link("https://play.qobuz.com/track/11").unwrap(),
        release_id: Some("alb3".into()),
        album_title: Some("Horizon".into()),
        album_artist: Some("Luna".into()),
        total_tracks: Some(3),
        search_origin: false,
    };
    queue.enqueue_requests(vec![mp3_req], &settings, "Waiting", Some(1));
    assert_eq!(queue.jobs.len(), 2);

    // Whole album with matching settings upgrades job 1 in place.
    queue.enqueue_requests(
        vec![QueueEntryRequest::from_link(
            normalize_link("https://play.qobuz.com/album/alb3").unwrap(),
        )],
        &settings,
        "Waiting",
        None,
    );
    assert_eq!(queue.jobs.len(), 2);
    assert!(queue.jobs[&1].has_full_release());
    assert!(queue.is_duplicate_request(
        &ResourceKey { kind: ResourceKind::Album, id: "alb3".into() },
        Some("alb3"),
        &settings
    ));
    assert!(queue.merge_resolved_into_job(
        1,
        &ResolvedResource {
            title: "Horizon".into(),
            edition: None,
            tracks: vec![t10, t11, t12],
            thumbnail_url: None,
            thumbnail: None,
        },
        true,
    ));
    assert_eq!(queue.jobs[&1].dto.files.len(), 3);
    assert_eq!(queue.jobs[&1].dto.subtitle, "Luna · Album · 3 tracks");

    // Playlist-originated album row stays separate from manual additions.
    let mut playlist_queue = Queue::default();
    playlist_queue.enqueue(
        vec![normalize_link("https://play.qobuz.com/playlist/77").unwrap()],
        &settings,
        "Waiting",
    );
    playlist_queue.install_album_rows(
        1,
        &ResolvedResource {
            title: "Mix".into(),
            edition: None,
            tracks: vec![release_track("10", "alb3", "Luna", "Horizon", 1, 1, 3)],
            thumbnail_url: None,
            thumbnail: None,
        },
    );
    playlist_queue.enqueue_requests(
        vec![QueueEntryRequest {
            link: normalize_link("https://play.qobuz.com/track/11").unwrap(),
            release_id: Some("alb3".into()),
            album_title: Some("Horizon".into()),
            album_artist: Some("Luna".into()),
            total_tracks: Some(3),
            search_origin: false,
        }],
        &settings,
        "Waiting",
        None,
    );
    assert_eq!(playlist_queue.jobs.len(), 2);
}

#[test]
fn pasted_tracks_coalesce_on_resolution_and_active_rows_accept_additions_cleanly() {
    let settings = Settings::default();
    let mut queue = Queue::default();
    queue.enqueue_requests(
        vec![
            QueueEntryRequest::from_link(normalize_link("https://play.qobuz.com/track/405").unwrap()),
            QueueEntryRequest::from_link(normalize_link("https://play.qobuz.com/track/402").unwrap()),
        ],
        &settings,
        "Waiting",
        None,
    );
    assert_eq!(queue.jobs.len(), 2);

    let p5 = Arc::new(ResolvedResource {
        title: "Track 5".into(),
        edition: None,
        tracks: vec![release_track("405", "alb4", "Echo", "Signals", 1, 5, 8)],
        thumbnail_url: None,
        thumbnail: None,
    });
    assert_eq!(queue.promote_track_to_release(1, p5), None);
    assert_eq!(queue.jobs[&1].dto.title, "Signals");
    assert_eq!(queue.jobs[&1].dto.subtitle, "Echo · Album · 1 of 8 tracks");

    let p2 = Arc::new(ResolvedResource {
        title: "Track 2".into(),
        edition: None,
        tracks: vec![release_track("402", "alb4", "Echo", "Signals", 1, 2, 8)],
        thumbnail_url: None,
        thumbnail: None,
    });
    assert_eq!(queue.promote_track_to_release(2, p2), Some(1));
    assert_eq!(queue.jobs.len(), 1);
    assert_eq!(queue.jobs[&1].dto.subtitle, "Echo · Album · 2 of 8 tracks");
    assert_eq!(
        queue.jobs[&1].dto.files.iter().map(|f| f.title.as_str()).collect::<Vec<_>>(),
        ["Track 2", "Track 5"]
    );

    // Active job retains its active RunTask Arc snapshot while queuing newly added track 1.
    let active_snapshot = queue.jobs[&1].plan.clone().unwrap();
    queue.jobs.get_mut(&1).unwrap().phase = Phase::Active;
    queue.enqueue_requests(
        vec![QueueEntryRequest {
            link: normalize_link("https://play.qobuz.com/track/401").unwrap(),
            release_id: Some("alb4".into()),
            album_title: Some("Signals".into()),
            album_artist: Some("Echo".into()),
            total_tracks: Some(8),
            search_origin: true,
        }],
        &settings,
        "Waiting",
        None,
    );
    assert_eq!(queue.jobs[&1].phase, Phase::Active);
    assert!(queue.jobs[&1].pending_additions.is_some());
    let p1 = ResolvedResource {
        title: "Signals".into(),
        edition: None,
        tracks: vec![release_track("401", "alb4", "Echo", "Signals", 1, 1, 8)],
        thumbnail_url: None,
        thumbnail: None,
    };
    assert!(queue.merge_resolved_into_job(1, &p1, true));
    assert_eq!(active_snapshot.tracks.len(), 2);
    assert_eq!(queue.jobs[&1].plan.as_ref().unwrap().tracks.len(), 3);
    assert_eq!(queue.jobs[&1].phase, Phase::Active);
    assert!(queue.jobs[&1].has_queued_primary_files());
    assert_eq!(
        queue.jobs[&1].dto.files.iter().map(|f| f.title.as_str()).collect::<Vec<_>>(),
        ["Track 1", "Track 2", "Track 5"]
    );
}
