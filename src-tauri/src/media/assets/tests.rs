use super::*;
use std::fs;

impl ArtworkCache {
    pub(in crate::media) async fn seed_test_png(
        &self,
        media: &Media,
        directory: &Path,
    ) -> PathBuf {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII="
        ).unwrap();
        let file = media.store.stage(directory, ".image").await.unwrap();
        fs::write(file.path(), &bytes).unwrap();
        let path = file.path().to_owned();
        assert!(
            self.staged
                .set(Ok(StagedArtwork {
                    file,
                    info: image_data_info(&bytes).unwrap(),
                    resolution: ArtworkResolution::Original,
                }))
                .is_ok()
        );
        path
    }
}

#[tokio::test]
async fn staged_cover_publishes_once_at_the_final_target_and_preserves_conflicts() {
    let fixture = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(fixture.path()).unwrap();
    let db = root.join("receipts.sqlite3");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(super::super::RECEIPT_SCHEMA)
        .unwrap();
    let media = Media::new(reqwest::Client::new(), db, root.clone());
    let cancel = CancellationToken::new();
    let recovery = std::sync::Arc::new(crate::recovery::Recovery::default());
    let transfer = recovery.transfer(1, 1, 1, |_| {});
    let track = ResolvedTrack {
        id: "12".into(),
        title: "Song".into(),
        album_id: "001release".into(),
        disc: 2,
        number: 1,
        track: serde_json::json!({}),
        album: serde_json::json!({}).into(),
        unavailable: None,
    };
    let artwork = ArtworkCache::default();
    let staged = artwork.seed_test_png(&media, &root).await;
    let previous_disc = root.join("Disc 2/Album");
    fs::create_dir_all(&previous_disc).unwrap();
    fs::write(previous_disc.join("Front.png"), b"old disc cover").unwrap();
    let provisional = root.join("Provisional/Album [001release]");
    let target = root.join("Delivered/Album [001release]");
    let missing = ArtworkCache::default();
    let error = media.prestage_artwork(
        &track, &provisional, &root, ArtworkResolution::Original, "Front", &transfer, &cancel, &missing, |_| {},
    ).await.unwrap_err();
    assert_eq!(error.code, "artwork_unavailable");
    assert!(!provisional.exists());
    let embed = media
        .artwork_for_embedding(
            &track,
            &provisional,
            &root,
            ArtworkResolution::Original,
            "Front",
            &transfer,
            &cancel,
            &artwork,
        )
        .await
        .unwrap();
    assert_eq!(embed, staged);
    assert!(!provisional.exists());

    let stopped = CancellationToken::new();
    stopped.cancel();
    let result = media
        .download_artwork(
            &track,
            &target,
            ArtworkResolution::Original,
            "Front",
            &transfer,
            &stopped,
            Some(&artwork),
            |_| {},
        )
        .await;
    assert_eq!(result.err().unwrap().code, "cancelled");
    assert!(!target.exists());
    let saved = media
        .download_artwork(
            &track,
            &target,
            ArtworkResolution::Original,
            "Front",
            &transfer,
            &cancel,
            Some(&artwork),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(
        saved.path,
        fs::canonicalize(target.join("Front.png")).unwrap()
    );
    assert!(!saved.reused);
    assert_eq!(fs::read(&saved.path).unwrap(), fs::read(&staged).unwrap());
    let verified = ArtworkCache::default();
    media.prestage_artwork(
        &track, &target, &root, ArtworkResolution::Original, "Front", &transfer, &cancel, &verified, |_| {},
    ).await.unwrap();
    assert!(matches!(verified.staged.get(), Some(Ok(_))));
    media.discard_artwork(verified).await;
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    assert_eq!(
        fs::read(previous_disc.join("Front.png")).unwrap(),
        b"old disc cover"
    );
    assert!(
        media
            .download_artwork(
                &track,
                &target,
                ArtworkResolution::Original,
                "Front",
                &transfer,
                &cancel,
                None,
                |_| {},
            )
            .await
            .unwrap()
            .reused
    );
    fs::write(&saved.path, b"foreign cover").unwrap();
    let retry = recovery.transfer(1, 2, 1, |_| {});
    assert_eq!(
        media
            .download_artwork(
                &track,
                &target,
                ArtworkResolution::Original,
                "Front",
                &retry,
                &cancel,
                Some(&artwork),
                |_| {},
            )
            .await
            .err()
            .unwrap()
            .code,
        "artwork_conflict"
    );
    assert_eq!(fs::read(&saved.path).unwrap(), b"foreign cover");
    let mut track = track;
    track.album = serde_json::json!({
        "title": "Album", "artist": {"name": "Artist"}, "media_count": 2,
    })
    .into();
    for (index, (template, expected)) in [
        ("%album artist%/%album name%", "Artist/Album"),
        (
            "%album artist%/%album name%/Disc %disc number%",
            "Artist/Album",
        ),
        (
            "%album artist%/Disc %disc number%/%album name%",
            "Artist/Album [001release]",
        ),
        ("Disc %disc number%/%album name%", "Album [001release]"),
    ]
    .into_iter()
    .enumerate()
    {
        let output = root.join(format!("Layout {index}"));
        fs::create_dir(&output).unwrap();
        let options = super::super::OutputOptions {
            folder_template: template.into(),
            safe_mode: true,
            embed_cover_art: false,
            ..Default::default()
        };
        let (_, layout) = super::super::plan_output(
            &output,
            &options,
            &track,
            &super::super::DeliveredQuality::requested(2),
            None,
            1,
        )
        .unwrap();
        let directory = layout.cover_dir.as_deref().unwrap();
        assert_eq!(directory, output.join(expected));
        let cover = media
            .download_artwork(
                &track,
                directory,
                options.artwork,
                "Front",
                &retry,
                &cancel,
                Some(&artwork),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            cover.path,
            fs::canonicalize(output.join(expected).join("Front.png")).unwrap()
        );
        for disc in layout.disc_dirs.values() {
            assert!(!disc.join("Front.png").exists());
        }
    }
    media.discard_artwork(artwork).await;
    assert!(!staged.exists());
}
use serde_json::json;

#[test]
fn cover_name_and_planned_booklet_names_follow_windows_rules() {
    for invalid in [
        "../cover",
        "cover.png",
        "NUL",
        "com¹",
        "ends.",
        "has?wildcard",
    ] {
        assert!(validate_cover_filename(invalid).is_err(), "{invalid}");
    }
    for valid in ["", "cover", "front sleeve", "Album.front"] {
        assert!(validate_cover_filename(valid).is_ok(), "{valid}");
    }
    let album = json!({"goodies": [
        {"file_format_id": 21, "title": "Liner Notes", "url": "https://static.qobuz.com/a.pdf"},
        {"file_format_id": 21, "title": "Liner Notes", "url": "https://static.qobuz.com/b.pdf"},
        {"file_format_id": 21, "url": "https://static.qobuz.com/c.pdf"},
        {"file_format_id": 21, "title": "booklet_01", "url": "https://static.qobuz.com/d.pdf"},
        {"file_format_id": 21, "url": "https://static.qobuz.com/e.pdf"}
    ]});
    let names = resolve_booklets(&album, true)
        .into_iter()
        .map(|source| source.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "liner_notes.pdf",
            "liner_notes_02.pdf",
            "booklet_01.pdf",
            "booklet_01_02.pdf",
            "booklet_02.pdf"
        ]
    );
    let unsafe_title = json!({"goodies": [{"file_format_id": 21, "title": "Liner: Notes", "url": "https://static.qobuz.com/notes.pdf"}]});
    assert_eq!(
        resolve_booklets(&unsafe_title, true)[0].name,
        "liner_notes.pdf"
    );
    assert_eq!(
        resolve_booklets(&unsafe_title, false)[0].name,
        "liner__notes.pdf"
    );
    assert!(
        !super::super::metadata::metadata_catalog()
            .iter()
            .find(|field| field.id == "replaygain")
            .unwrap()
            .default_enabled
    );
}
