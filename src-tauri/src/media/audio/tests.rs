use super::*;
#[cfg(target_os = "macos")]
use super::inspect::verify_full_duration;
use super::inspect::{inspect, read_flac};
#[cfg(target_os = "macos")]
use super::pipeline::ensure_same_offer;
use super::receipt::audio_receipt;
use super::super::{ArtworkCache, AudioFormat, DeliveredQuality, Media, output_store::Receipt};
use crate::qobuz::StreamOffer;
use lofty::{config::ParseOptions, file::AudioFile};
use serde_json::json;
use std::fs::File;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn token_preview_uses_distinct_path_receipt_and_reuse_without_changing_preferences() {
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let database = root_path.join("index.sqlite3");
    rusqlite::Connection::open(&database).unwrap().execute_batch(super::super::RECEIPT_SCHEMA).unwrap();
    let mut client = QobuzClient::new().unwrap();
    client.fixture = Some(std::sync::Arc::new(crate::qobuz::ProviderFixture {
        api: Box::new(|endpoint, _, _| {
            assert_eq!(endpoint,"track/getFileUrl");
            Ok(json!({"sample":true,"format_id":5,"sampling_rate":44.1,"url":"https://example.test/preview"}))
        }),
        player_text: Box::new(|_| panic!("manual pair must not discover")),
    }));
    let media = Media::new(client.http_client().clone(), database, root_path.clone());
    let track = ResolvedTrack {
        id: "11".into(), title: "Fixture".into(), album_id:"22".into(), disc:1, number:1,
        track: json!({"title":"Fixture"}), album: json!({"title":"Album","artist":{"name":"Artist"}}).into(), unavailable:None,
    };
    let options = OutputOptions { quality:4, download_cover_art:false, folder_template:"%album name%".into(), filename_template:"%track title%".into(), ..Default::default() };
    let credentials = Credentials::default();
    let artwork_cache = ArtworkCache::default();
    let transfer = std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1,1,1, |_| {});
    let request = AudioRequest { client:&client, credentials:&credentials, track:&track, options:&options, root:&root_path, pinned:None, sequence:1, transfer:&transfer, replacement:None, require_full_duration:false, artwork_cache:&artwork_cache, cover_basename:"cover" };
    let cancel = CancellationToken::new();
    let (full_path, _) = media.output_path(request,&DeliveredQuality::requested(1),&cancel).await.unwrap();
    std::fs::write(&full_path,b"existing full audio").unwrap();
    // Seed a transferred source to exercise publication without a live CDN.
    let file = media.store.stage(&root_path,".mp3").await.unwrap();
    let mut frame = vec![0;417]; frame[..4].copy_from_slice(&[0xff,0xfb,0x90,0x64]);
    std::fs::write(file.path(),frame.repeat(10)).unwrap();
    let properties = inspect(file.path(),AudioFormat::Mp3).await.unwrap();
    media.retained.lock().unwrap().insert(StagedKey { track:track.id.clone(), root:root_path.clone(),tier:1,preview_only:true }, StagedAudio {file,owner:1,properties,tier:1});
    let output = media.download_audio(request,&cancel, |_| {}).await.unwrap();
    assert!(output.is_preview && !output.reused);
    assert_eq!(output.quality.tier,1);
    assert!(output.path.ends_with("Fixture - Preview.mp3"));
    assert_eq!(std::fs::read(&full_path).unwrap(),b"existing full audio");
    assert_eq!(options.quality,4);
    assert_eq!(credentials.access_mode,crate::qobuz::AccessMode::Unknown);
    let reused = media.download_audio(request,&cancel, |_| {}).await.unwrap();
    assert!(reused.is_preview && reused.reused);
    assert_eq!(reused.path,output.path);
    assert!(media.offer_audio_replacement(&output.path,&track.id,&cancel).await.is_err());
    std::fs::write(&output.path,b"modified by operator").unwrap();
    let error=media.download_audio(request,&cancel, |_| {}).await.err().unwrap();
    assert_eq!(error.code,"preview_output_conflict");
    assert_eq!(std::fs::read(&output.path).unwrap(),b"modified by operator");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn legacy_full_mp3_is_published_and_reused_only_with_full_duration() {
    let root=tempfile::tempdir().unwrap();
    let root_path=std::fs::canonicalize(root.path()).unwrap();
    let db=root_path.join("index.sqlite3");
    rusqlite::Connection::open(&db).unwrap().execute_batch(super::super::RECEIPT_SCHEMA).unwrap();
    let mut client=QobuzClient::new().unwrap();
    client.fixture=Some(std::sync::Arc::new(crate::qobuz::ProviderFixture {
        api:Box::new(|_,_,_| Ok(json!({"format_id":5,"sampling_rate":44.1,"url":"https://example.test/full"}))),
        player_text:Box::new(|_| panic!("manual pair")),
    }));
    let media=Media::new(client.http_client().clone(),db,root_path.clone());
    let track=ResolvedTrack {id:"12".into(),title:"Full".into(),album_id:"22".into(),disc:1,number:1,track:json!({"title":"Full","duration":0.261}),album:json!({"title":"Album","artist":{"name":"Artist"}}).into(),unavailable:None};
    let options=OutputOptions {quality:1,download_cover_art:false,folder_template:"%album name%".into(),filename_template:"%track title%".into(),..Default::default()};
    let credentials=Credentials::default();
    let artwork=ArtworkCache::default();
    let transfer=std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1,1,1, |_| {});
    let request=AudioRequest {client:&client,credentials:&credentials,track:&track,options:&options,root:&root_path,pinned:None,sequence:1,transfer:&transfer,replacement:None,require_full_duration:false,artwork_cache:&artwork,cover_basename:"cover"};
    let cancel=CancellationToken::new();
    let mut frame=vec![0;417];frame[..4].copy_from_slice(&[0xff,0xfb,0x90,0x64]);
    for short in [false,true] {
        let current=if short {ResolvedTrack {id:"13".into(),title:"Short".into(),track:json!({"title":"Short","duration":180}),..track.clone()}} else {track.clone()};
        let file=media.store.stage(&root_path,".mp3").await.unwrap();
        std::fs::write(file.path(),frame.repeat(10)).unwrap();
        let properties=inspect(file.path(),AudioFormat::Mp3).await.unwrap();
        media.retained.lock().unwrap().insert(StagedKey {track:current.id.clone(),root:root_path.clone(),tier:1,preview_only:false},StagedAudio {file,owner:1,properties,tier:1});
        let current_request=AudioRequest {track:&current,..request};
        let result=media.download_audio(current_request,&cancel, |_| {}).await;
        if short {
            assert_eq!(result.err().unwrap().code,"full_track_duration");
            assert!(!root_path.join("Album/Short.mp3").exists());
        } else {
            let output=result.unwrap();
            assert!(!output.is_preview && !output.reused);
            assert!(output.path.ends_with("Full.mp3"));
            assert!(media.download_audio(current_request,&cancel, |_| {}).await.unwrap().reused);
            assert!(media.offer_audio_replacement(&output.path,&current.id,&cancel).await.is_ok());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn full_duration_guard_rejects_short_or_unknown_flac_and_mp3() {
    let track=ResolvedTrack {id:"42".into(),title:"Full".into(),album_id:"22".into(),disc:1,number:1,track:json!({"duration":180}),album:json!({}).into(),unavailable:None};
    for duration_ms in [0,30_000,90_000,170_000,190_000] {
        assert!(verify_full_duration(&track,AudioProperties::Mp3 {sample_rate:44_100,duration_ms}).is_err());
    }
    assert!(verify_full_duration(&track,AudioProperties::Mp3 {sample_rate:44_100,duration_ms:180_000}).is_ok());
    let info=FlacInfo {sample_rate:44_100,channels:2,bit_depth:16,samples:180*44_100,checksum:[1;16]};
    assert!(verify_full_duration(&track,AudioProperties::Flac(info)).is_ok());
    assert!(verify_full_duration(&track,AudioProperties::Flac(FlacInfo {samples:30*44_100,..info})).is_err());
    let missing=ResolvedTrack {track:json!({}),..track};
    assert!(verify_full_duration(&missing,AudioProperties::Flac(info)).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn refreshed_offer_cannot_change_reserved_output_kind_or_tier() {
    let offer = |is_preview,tier| StreamOffer { url:url::Url::parse("https://example.test/audio").unwrap(),tier,format_id:5,bit_depth:None,sample_rate:None,restrictions:vec![],is_preview,requires_full_duration:false };
    assert!(ensure_same_offer(&offer(true,1),&offer(true,1)).is_ok());
    assert!(ensure_same_offer(&offer(false,1),&offer(true,1)).is_err());
    assert!(ensure_same_offer(&offer(false,4),&offer(false,3)).is_err());
}

#[test]
fn reads_pcm_checksum_from_native_streaminfo() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pcm.flac");
    let mut bytes = vec![0_u8; 43];
    bytes[..4].copy_from_slice(b"fLaC");
    bytes[7] = 34;
    let properties = (44_100_u64 << 44) | (1 << 41) | (15 << 36) | 88_200;
    bytes[18..26].copy_from_slice(&properties.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let unknown = read_flac(&path).unwrap();
    assert!(!unknown.known_checksum());
    bytes[26] = 1;
    std::fs::write(&path, &bytes).unwrap();
    let known = read_flac(&path).unwrap();
    assert!(known.known_checksum());
    assert!(unknown.matches_pcm(known));
    assert!(!known.matches_pcm(unknown));
    assert_eq!(known.sample_rate, 44_100);
    assert_eq!(known.channels, 2);
    assert_eq!(known.bit_depth, 16);
}

#[tokio::test]
async fn selective_clear_discards_only_removed_owners_retained_stages() {
    let root = tempfile::tempdir().unwrap();
    let client = QobuzClient::new().unwrap();
    rusqlite::Connection::open(root.path().join("index.sqlite3"))
        .unwrap()
        .execute_batch(super::super::RECEIPT_SCHEMA)
        .unwrap();
    let media = Media::new(
        client.http_client().clone(),
        root.path().join("index.sqlite3"),
        root.path().to_path_buf(),
    );
    let mut paths = Vec::new();
    for owner in [11, 22] {
        let file = media.store.stage(root.path(), ".mp3").await.unwrap();
        std::fs::write(file.path(), b"retained source").unwrap();
        paths.push(file.path().to_path_buf());
        media.retained.lock().unwrap().insert(
            StagedKey {
                track: owner.to_string(),
                root: root.path().to_path_buf(),
                tier: 1,
                preview_only: false,
            },
            StagedAudio {
                file,
                owner,
                properties: AudioProperties::Mp3 {
                    sample_rate: 44_100,
                    #[cfg(target_os = "macos")]
                    duration_ms: 1000,
                },
                tier: 1,
            },
        );
    }
    let finalized = root.path().join("saved.mp3");
    std::fs::write(&finalized, b"finalized").unwrap();
    media.discard_retained_jobs(&[11]).await;
    assert!(!paths[0].exists());
    assert!(paths[1].exists());
    assert_eq!(media.retained.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(finalized).unwrap(), b"finalized");
    media.discard_retained_jobs(&[22]).await;
    assert!(!paths[1].exists());
}

#[tokio::test]
async fn repairs_unknown_checksum_and_preserves_pcm_and_disabled_tags() {
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let database = root_path.join("index.sqlite3");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch(super::super::RECEIPT_SCHEMA)
        .unwrap();
    let qobuz = QobuzClient::new().unwrap();
    let media = Media::new(qobuz.http_client().clone(), database.clone(), root_path.clone());
    let original = root_path.join("original.flac");
    // 128 mono PCM samples, sample[n] = n * 91, and a source TITLE comment.
    let fixture = include_bytes!("../../../tests/fixtures/pipeline-pcm16-mono.flac");
    std::fs::write(&original, fixture).unwrap();
    let cancel = CancellationToken::new();
    let original_info = read_flac(&original).unwrap();
    let mut data = fixture.to_vec();
    data[26..42].fill(0);
    let file = media.store.stage(&root_path, ".flac").await.unwrap();
    std::fs::write(file.path(), &data).unwrap();
    let source = StagedAudio {
        owner: 1,
        properties: inspect(file.path(), AudioFormat::Flac).await.unwrap(),
        file,
        tier: 2,
    };
    let track = ResolvedTrack {
        id: "11".into(),
        title: "Qobuz title".into(),
        album_id: "22".into(),
        disc: 2,
        number: 3,
        track: json!({"title": "Qobuz title"}),
        album:
            json!({"title": "Test album", "artist": {"name": "Test artist"}, "media_count": 2})
                .into(),
        unavailable: None,
    };
    let mut options = OutputOptions {
        quality: 2,
        download_cover_art: false,
        folder_template: "%album artist%/%album name% [(%year%)]".into(),
        filename_template: "%track number% - %track title%".into(),
        ..Default::default()
    };
    options.tags.insert("title".into(), false);
    let credentials = Credentials::default();
    let artwork_cache = ArtworkCache::default();
    let transfer =
        std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1, 1, 1, |_| {});
    let request = AudioRequest {
        replacement: None,
        #[cfg(target_os = "macos")]
        require_full_duration: false,
        artwork_cache: &artwork_cache,
        cover_basename: &options.cover_filename,
        client: &qobuz,
        credentials: &credentials,
        track: &track,
        options: &options,
        root: &root_path,
        pinned: None,
        sequence: 1,
        transfer: &transfer,
    };
    let (path, _) = media
        .output_path(request, &DeliveredQuality::requested(2), &cancel)
        .await
        .unwrap();
    let stopped = CancellationToken::new();
    stopped.cancel();
    let error = media
        .process_audio(request, &source, None, &stopped)
        .await
        .err()
        .expect("a cancelled job must not be published");
    assert_eq!(error.code, "cancelled");
    assert!(!path.exists());

    let known_stage = media.store.stage(&root_path, ".flac").await.unwrap();
    std::fs::write(known_stage.path(), fixture).unwrap();
    let known_source = StagedAudio {
        owner: 1,
        properties: AudioProperties::Flac(original_info),
        file: known_stage,
        tier: 2,
    };
    assert!(
        media
            .repair_flac(&known_source, &root_path, &cancel)
            .await
            .unwrap()
            .is_none()
    );
    media.store.discard(known_source.file).await;

    // Retain a valid STREAMINFO header but truncate the audio frame.
    std::fs::write(source.file.path(), &data[..data.len() - 4]).unwrap();
    let error = media
        .process_audio(request, &source, None, &cancel)
        .await
        .err()
        .expect("a truncated FLAC must not be published");
    assert_eq!(error.code, "flac_source");
    assert!(!error.retryable);
    assert!(!path.exists());

    // A bad nonzero digest survives metadata editing and fails final verification.
    let mut corrupt = fixture.to_vec();
    corrupt[26] ^= 1;
    std::fs::write(&original, corrupt).unwrap();
    let error = media
        .tag_and_publish(
            request,
            &path,
            &original,
            DeliveredQuality::requested(2),
            true,
            &mut Enrichment::default(),
            &cancel,
        )
        .await
        .expect_err("a corrupt final stage must not be published");
    assert_eq!(error.code, "flac_verification");
    assert!(error.retryable);
    assert!(!path.exists());
    let connection = rusqlite::Connection::open(&database).unwrap();
    for (query, expected) in [
        ("SELECT COUNT(*) FROM media_receipts", 0),
        ("SELECT COUNT(*) FROM media_stages", 1),
    ] {
        assert_eq!(
            connection.query_row(query, [], |row| row.get::<_, i64>(0)).unwrap(),
            expected
        );
    }
    drop(connection);

    std::fs::write(source.file.path(), &data).unwrap();
    let output = media
        .process_audio(request, &source, None, &cancel)
        .await
        .unwrap();
    assert_eq!(std::fs::read(source.file.path()).unwrap(), data);
    assert!(
        output.path.ends_with(
            Path::new("Test artist")
                .join("Test album")
                .join("Disc 2")
                .join("03 - Qobuz title.flac")
        )
    );
    assert_eq!(read_flac(&output.path).unwrap(), original_info);
    let mut file = File::open(&output.path).unwrap();
    let audio = lofty::flac::FlacFile::read_from(&mut file, ParseOptions::new()).unwrap();
    let tag = audio.vorbis_comments().unwrap();
    assert_eq!(tag.get("TITLE"), Some("Source title"));
    assert_eq!(tag.get("ALBUM"), Some("Test album"));
    let receipt = audio_receipt(request, &output.quality, false, false, None).unwrap();
    let mut variant: serde_json::Value = serde_json::from_str(&receipt.variant).unwrap();
    assert_eq!(variant["reencode_flac"], false);
    // Reuse a receipt from a profile that previously enabled recompression.
    variant["reencode_flac"] = json!(true);
    let connection = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE media_receipts SET variant = ?1 WHERE kind = 'audio'",
                [variant.to_string()],
            )
            .unwrap(),
        1
    );
    drop(connection);
    assert!(
        media
            .process_audio(request, &source, None, &cancel)
            .await
            .unwrap()
            .reused
    );
    let mut with_cover = options.clone();
    with_cover.download_cover_art = true;
    with_cover.filename_template = "%track number% - %track title% - artwork".into();
    let cover_request = AudioRequest {
        replacement: None,
        #[cfg(target_os = "macos")]
        require_full_duration: false,
        options: &with_cover,
        ..request
    };
    let (_, cover_layout) = media
        .output_path(cover_request, &DeliveredQuality::requested(2), &cancel)
        .await
        .unwrap();
    let directory = cover_layout.cover_dir.as_deref().unwrap();
    std::fs::create_dir_all(directory).unwrap();
    let cover = directory.join("cover.png");
    let cover_stage = media.store.stage(directory, ".png").await.unwrap();
    use base64::Engine;
    let image = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII=").unwrap();
    std::fs::write(cover_stage.path(), image).unwrap();
    let cover_receipt = Receipt {
        kind: "artwork",
        identity: track.album_id.clone(),
        variant: "Original".into(),
    };
    media
        .store
        .publish(cover_stage, &cover, &cover_receipt, &cancel)
        .await
        .unwrap();
    let output = media
        .process_audio(cover_request, &source, None, &cancel)
        .await
        .unwrap();
    assert!(output.cover_embedded);
    assert_eq!(output.cover_path, Some(cover));
    assert!(output.cover_warning.is_none());
    assert_eq!(read_flac(&output.path).unwrap(), original_info);
    let mut output_file = File::open(&output.path).unwrap();
    let output_audio =
        lofty::flac::FlacFile::read_from(&mut output_file, ParseOptions::new()).unwrap();
    use lofty::ogg::OggPictureStorage;
    assert_eq!(output_audio.pictures().len(), 1);
    assert!(
        media
            .process_audio(cover_request, &source, None, &cancel)
            .await
            .unwrap()
            .cover_embedded
    );
    let mut fallback = with_cover.clone();
    fallback.quality = 4;
    fallback.cover_filename = "Front".into();
    fallback.folder_template = "[%bit depth% Bit]/Disc %disc number%/%album name%".into();
    let staged_cache = ArtworkCache::default();
    let staged = staged_cache.seed_test_png(&media, &root_path).await;
    let disc_one = ResolvedTrack {
        id: "12".into(),
        disc: 1,
        ..track.clone()
    };
    let fallback_request = AudioRequest {
        artwork_cache: &staged_cache,
        cover_basename: &fallback.cover_filename,
        options: &fallback,
        track: &disc_one,
        ..cover_request
    };
    let first = media
        .process_audio(fallback_request, &source, None, &cancel)
        .await
        .unwrap();
    let second = media
        .process_audio(
            AudioRequest {
                track: &track,
                pinned: Some(&first.layout),
                ..fallback_request
            },
            &source,
            None,
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        first
            .path
            .parent()
            .unwrap()
            .ends_with("16 Bit/Disc 1/Test album")
    );
    assert!(
        second
            .path
            .parent()
            .unwrap()
            .ends_with("16 Bit/Disc 2/Test album")
    );
    let target = first.layout.cover_dir.as_deref().unwrap();
    assert!(target.ends_with("16 Bit/Test album [22]"));
    assert!(!root_path.join("24 Bit").exists());
    assert!(!target.exists());
    for output in [&first, &second] {
        assert!(output.cover_embedded && output.cover_warning.is_none());
        let mut audio = File::open(&output.path).unwrap();
        let audio = lofty::flac::FlacFile::read_from(&mut audio, ParseOptions::new()).unwrap();
        assert_eq!(audio.pictures().len(), 1);
        assert!(!output.path.parent().unwrap().join("Front.png").exists());
    }
    let saved = media
        .download_artwork(
            &disc_one,
            target,
            fallback.artwork,
            &fallback.cover_filename,
            &transfer,
            &cancel,
            Some(&staged_cache),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(
        saved.path,
        std::fs::canonicalize(target.join("Front.png")).unwrap()
    );
    assert_eq!(
        std::fs::read(saved.path).unwrap(),
        std::fs::read(staged).unwrap()
    );
    assert_eq!(std::fs::read_dir(target).unwrap().count(), 1);

    let sidecar_options = OutputOptions {
        embed_cover_art: false,
        filename_template: "%track title% sidecar only".into(),
        ..fallback.clone()
    };
    let sidecar_only = media
        .process_audio(
            AudioRequest {
                options: &sidecar_options,
                pinned: Some(&first.layout),
                ..fallback_request
            },
            &source,
            None,
            &cancel,
        )
        .await
        .unwrap();
    assert!(!sidecar_only.cover_embedded && sidecar_only.cover_warning.is_none());
    let mut audio = File::open(&sidecar_only.path).unwrap();
    let audio = lofty::flac::FlacFile::read_from(&mut audio, ParseOptions::new()).unwrap();
    assert!(audio.pictures().is_empty());
    assert!(
        media
            .download_artwork(
                &track,
                target,
                fallback.artwork,
                &fallback.cover_filename,
                &transfer,
                &cancel,
                None,
                |_| {},
            )
            .await
            .unwrap()
            .reused
    );
    media.discard_artwork(staged_cache).await;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        let retry_options = OutputOptions {
            filename_template: "%track title% retry".into(),
            ..options.clone()
        };
        let retry_request = AudioRequest {
            options: &retry_options,
            ..request
        };
        let offer = || StreamOffer {
            url: url::Url::parse("file:///unexpected-refetch.flac").unwrap(),
            tier: 2,
            format_id: 6,
            bit_depth: Some(16),
            sample_rate: Some(44.1),
            restrictions: Vec::new(),
        };
        let key = || StagedKey {
            track: track.id.clone(),
            root: root_path.clone(),
            tier: 2,
            preview_only: false,
        };
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(source.file.path())
            .unwrap();
        let source_path = source.file.path().to_owned();
        media.retained.lock().unwrap().insert(key(), source);
        let budget = transfer.budget();
        let error = media
            .download_offered_audio(retry_request, offer(), key(), &budget, &cancel, &|_| {})
            .await
            .err()
            .expect("the locked repair source should cause a retryable disk error");
        assert_eq!(error.code, "disk");
        assert!(error.retryable);
        assert_eq!(media.retained.lock().unwrap()[&key()].file.path(), source_path);
        drop(locked);
        let output = media
            .download_offered_audio(retry_request, offer(), key(), &budget, &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(read_flac(&output.path).unwrap(), original_info);
        assert!(!source_path.exists());
    }
    #[cfg(not(windows))]
    media.store.discard(source.file).await;
    media.cleanup_stages().await.unwrap();
}

#[tokio::test]
async fn mid_download_tagging_writes_album_version_to_published_flac() {
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let database = root_path.join("index.sqlite3");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch(super::super::RECEIPT_SCHEMA)
        .unwrap();
    let qobuz = QobuzClient::new().unwrap();
    let media = Media::new(qobuz.http_client().clone(), database, root_path.clone());
    let fixture = include_bytes!("../../../tests/fixtures/pipeline-pcm16-mono.flac");
    let stage = media.store.stage(&root_path, ".flac").await.unwrap();
    std::fs::write(stage.path(), fixture).unwrap();
    let source = StagedAudio {
        owner: 1,
        properties: inspect(stage.path(), AudioFormat::Flac).await.unwrap(),
        file: stage,
        tier: 2,
    };
    let track = ResolvedTrack {
        id: "11".into(),
        title: "Qobuz title".into(),
        album_id: "22".into(),
        disc: 1,
        number: 1,
        track: json!({"title": "Qobuz title"}),
        album: json!({
            "title": "Test album",
            "version": "Remastered",
            "artist": {"name": "Test artist"},
            "media_count": 1
        })
        .into(),
        unavailable: None,
    };
    let options = OutputOptions {
        quality: 2,
        download_cover_art: false,
        folder_template: "%album artist%/%album title%".into(),
        filename_template: "%track number% - %track title%[ %album version%]".into(),
        ..Default::default()
    };
    let credentials = Credentials::default();
    let artwork_cache = ArtworkCache::default();
    let transfer =
        std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1, 1, 1, |_| {});
    let request = AudioRequest {
        replacement: None,
        #[cfg(target_os = "macos")]
        require_full_duration: false,
        artwork_cache: &artwork_cache,
        cover_basename: &options.cover_filename,
        client: &qobuz,
        credentials: &credentials,
        track: &track,
        options: &options,
        root: &root_path,
        pinned: None,
        sequence: 1,
        transfer: &transfer,
    };
    let cancel = CancellationToken::new();
    let output = media
        .process_audio(request, &source, None, &cancel)
        .await
        .unwrap();
    assert!(output.path.ends_with(
        Path::new("Test artist")
            .join("Test album (Remastered)")
            .join("01 - Qobuz title (Remastered).flac")
    ));
    let mut file = File::open(&output.path).unwrap();
    let audio = lofty::flac::FlacFile::read_from(&mut file, ParseOptions::new()).unwrap();
    assert_eq!(
        audio.vorbis_comments().unwrap().get("ALBUM"),
        Some("Test album (Remastered)")
    );

    let mut mp3_frame = vec![0_u8; 417];
    mp3_frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    let mp3_stage = media.store.stage(&root_path, ".mp3").await.unwrap();
    std::fs::write(mp3_stage.path(), mp3_frame.repeat(10)).unwrap();
    let mp3_options = OutputOptions {
        quality: 1,
        download_cover_art: false,
        folder_template: "%album artist%/%album name%".into(),
        filename_template: "%track number% - %track title%".into(),
        ..Default::default()
    };
    let mp3_request = AudioRequest {
        options: &mp3_options,
        ..request
    };
    let (mp3_path, _) = media
        .output_path(mp3_request, &DeliveredQuality::requested(1), &cancel)
        .await
        .unwrap();
    assert!(
        mp3_path.ends_with(
            Path::new("Test artist")
                .join("Test album (Remastered)")
                .join("01 - Qobuz title.mp3")
        )
    );
    media
        .tag_and_publish(
            mp3_request,
            &mp3_path,
            mp3_stage.path(),
            DeliveredQuality::requested(1),
            false,
            &mut Enrichment::default(),
            &cancel,
        )
        .await
        .unwrap();
    let mut mp3_file = File::open(&mp3_path).unwrap();
    let mp3_audio =
        lofty::mpeg::MpegFile::read_from(&mut mp3_file, ParseOptions::new()).unwrap();
    assert_eq!(
        mp3_audio
            .id3v2()
            .unwrap()
            .get_text(&lofty::id3::v2::FrameId::Valid(std::borrow::Cow::Borrowed("TALB"))),
        Some("Test album (Remastered)")
    );
    media.store.discard(mp3_stage).await;

    media.store.discard(source.file).await;
    media.cleanup_stages().await.unwrap();
}
