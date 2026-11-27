use super::*;
use crate::qobuz::normalize_link;

#[test]
fn upgrades_the_original_database_without_losing_receipts() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("qbdlx.sqlite3");
    let (storage, _, _, _) = Storage::initialize(path.clone()).unwrap();
    let db = storage.open().unwrap();
    db.execute("INSERT INTO media_receipts VALUES ('portable:track.flac','audio','42','options','digest')", []).unwrap();
    db.execute_batch("DROP TABLE media_pending_updates; PRAGMA user_version=1;")
        .unwrap();
    drop(db);
    let (storage, _, _, _) = Storage::initialize(path).unwrap();
    let db = storage.open().unwrap();
    let digest: String = db
        .query_row("SELECT sha256 FROM media_receipts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(digest, "digest");
    let count: i64 = db
        .query_row("SELECT count(*) FROM media_pending_updates", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    let version: u32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn legacy_preferences_pick_up_new_defaults_and_keep_explicit_tags() {
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "revision": 7, "quality": 3, "tags": {"replaygain": true}, "intervalSeconds": 2.0
    }))
    .unwrap();
    assert_eq!(settings.revision, 7);
    assert_eq!(settings.output.quality, 3);
    assert_eq!(settings.output.tags.get("replaygain"), Some(&true));
    assert_eq!(settings.max_connections.limit(), 1);
    assert_eq!(settings.download_mode, DownloadMode::Sequential);
    assert_eq!(settings.output.lyrics_mode, LyricsMode::External);
    assert!(settings.output.download_cover_art);
    assert!(settings.output.allow_fallback);
    assert!(!settings.output.booklet_only);
    let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
        "maxConnections": "unlimited", "intervalSeconds": 10000.0, "coverFilename": "   "
    }))
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let updated = settings.apply(directory.path(), patch).unwrap();
    assert_eq!(updated.max_connections.limit(), usize::MAX);
    assert_eq!(updated.interval_seconds, 10000.0);
    assert_eq!(updated.output.cover_filename, "cover");
}

#[test]
fn legacy_flac_preferences_are_ignored_without_losing_other_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("qbdlx.sqlite3");
    let (storage, _, _, _) = Storage::initialize(path.clone()).unwrap();
    let mut expected = serde_json::to_value(Settings::default()).unwrap();
    expected["revision"] = serde_json::json!(12);
    expected["quality"] = serde_json::json!(4);
    expected["outputLocation"] = serde_json::json!({ "Portable": "Library" });
    expected["maxConnections"] = serde_json::json!("unlimited");
    expected["theme"] = serde_json::json!("Violet");
    expected["tags"] = serde_json::json!({ "replaygain": true });
    let mut legacy = expected.clone();
    legacy["flacLocation"] = serde_json::json!({ "Portable": "tools/flac.exe" });
    legacy["reencodeFlac"] = serde_json::json!(true);
    storage
        .open()
        .unwrap()
        .execute(
            "INSERT INTO settings (id, value) VALUES (1, ?1)",
            [legacy.to_string()],
        )
        .unwrap();

    let (storage, settings, _, _) = Storage::initialize(path.clone()).unwrap();
    assert_eq!(serde_json::to_value(&settings).unwrap(), expected);
    assert_eq!(
        settings.destination(directory.path()),
        Some(directory.path().join("Library"))
    );
    let dto = serde_json::to_value(settings.dto(directory.path())).unwrap();
    for removed in ["flacLocation", "flacPath", "reencodeFlac"] {
        assert!(dto.get(removed).is_none());
    }

    let patch = serde_json::from_value(serde_json::json!({
        "intervalSeconds": 9.0,
        "flacPath": "missing/flac.exe",
        "reencodeFlac": true,
    }))
    .unwrap();
    let updated = settings.apply(directory.path(), patch).unwrap();
    storage.save_settings(&updated).unwrap();
    expected["intervalSeconds"] = serde_json::json!(9.0);
    expected["revision"] = serde_json::json!(13);
    let (_, reloaded, _, _) = Storage::initialize(path).unwrap();
    assert_eq!(serde_json::to_value(reloaded).unwrap(), expected);
}

#[test]
fn naming_defaults_and_saved_templates_keep_their_spacing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let database = root.join("qbdlx.sqlite3");
    let (storage, settings, _, _) = Storage::initialize(database.clone()).unwrap();
    assert_eq!(
        settings.output.folder_template,
        "%album artist%/[(%year%)] %album name% [[Q%target quality%]][[%bit depth%-Bit %sampling rate%kHz]]"
    );
    assert_eq!(
        settings.output.filename_template,
        "%track number%. %artist% - %track title%"
    );
    assert!(settings.output.safe_mode);

    // An existing template retains its separator placement on load and unrelated edits.
    let existing_folder = "%album artist%/%album name% [(%year%)]";
    let existing = Settings {
        output: OutputOptions {
            folder_template: existing_folder.into(),
            ..settings.output
        },
        ..settings
    };
    storage.save_settings(&existing).unwrap();
    let (_, loaded, _, _) = Storage::initialize(database.clone()).unwrap();
    let updated = loaded
        .apply(
            root,
            SettingsPatch {
                quality: Some(2),
                safe_mode: Some(false),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
    assert_eq!(updated.output.folder_template, existing_folder);

    let folder = "%artist%     %album name%";
    let filename = "%track number%     %track title%";
    let spaced = updated
        .apply(
            root,
            SettingsPatch {
                folder_template: Some(folder.into()),
                filename_template: Some(filename.into()),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
    storage.save_settings(&spaced).unwrap();
    let (_, restored, _, _) = Storage::initialize(database).unwrap();
    assert_eq!(restored.output.folder_template, folder);
    assert_eq!(restored.output.filename_template, filename);
    assert!(!restored.output.safe_mode);
    assert_eq!(
        media::preview_output(
            &restored.output.folder_template,
            &restored.output.filename_template,
            2,
            restored.output.safe_mode,
        )
        .unwrap(),
        PathBuf::from("Aster Vale     Music for the Moon").join("03     Quiet orbit.flac")
    );
}

#[test]
fn restores_settings_and_cached_bookmarks_from_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let database = root.join("qbdlx.sqlite3");
    let (storage, settings, _, _) = Storage::initialize(database.clone()).unwrap();
    let next = settings
        .apply(
            root,
            SettingsPatch {
                quality: Some(1),
                interval_seconds: Some(4.0),
                output_path: Some(root.to_string_lossy().into_owned()),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
    storage.save_settings(&next).unwrap();
    let mut bookmark = Bookmark::new(
        1,
        normalize_link("https://open.qobuz.com/album/0886443927087").unwrap(),
        "Pending".into(),
    );
    bookmark.update(
        BookmarkSummary {
            activity: None,
            title: "Cached release".into(),
            album_artist: Some("Cached artist".into()),
            album_name: Some("Cached release".into()),
            edition: Some("(Deluxe)".into()),
            bit_depth: Some(24),
            sample_rate: Some(96.0),
            thumbnail_url: None,
            thumbnail: None,
        },
        "account-fingerprint".into(),
    );
    storage.save_bookmarks([&bookmark]).unwrap();

    let (_, settings, bookmarks, next_id) = Storage::initialize(database.clone()).unwrap();
    assert_eq!(settings.output.quality, 1);
    assert_eq!(settings.interval_seconds, 4.0);
    assert!(
        matches!(settings.output_location, Some(SavedLocation::Portable(ref path)) if path.as_os_str().is_empty())
    );
    assert_eq!(bookmarks[&1].dto.title, "Cached release");
    assert_eq!(bookmarks[&1].dto.edition.as_deref(), Some("(Deluxe)"));
    assert_eq!(bookmarks[&1].dto.sampling_rate, Some(96.0));
    assert_eq!(
        bookmarks[&1].fingerprint.as_deref(),
        Some("account-fingerprint")
    );
    assert_eq!(next_id, 2);

    // Legacy bookmark rows without an `edition` JSON field still deserialize cleanly.
    let legacy_key = serde_json::json!({"kind": "album", "id": "legacy01"});
    let legacy = serde_json::json!({
        "key": legacy_key,
        "dto": {
            "id": 2,
            "url": "https://open.qobuz.com/album/legacy01",
            "title": "Legacy release",
            "albumArtist": "Legacy artist",
            "albumName": "Legacy release",
            "bitDepth": 16,
            "samplingRate": 44.1,
            "reason": null,
            "thumbnail": null,
            "resourceKind": "album"
        },
        "fingerprint": "account-fingerprint"
    });
    storage
        .open()
        .unwrap()
        .execute(
            "INSERT INTO bookmarks(id, resource, value) VALUES(2, ?1, ?2)",
            [legacy_key.to_string(), legacy.to_string()],
        )
        .unwrap();
    let (_, _, restored_bookmarks, restored_next_id) = Storage::initialize(database).unwrap();
    assert_eq!(restored_bookmarks[&2].dto.title, "Legacy release");
    assert_eq!(restored_bookmarks[&2].dto.edition, None);
    let serialized_legacy = serde_json::to_value(&restored_bookmarks[&2].dto).unwrap();
    assert!(serialized_legacy.get("edition").is_none());
    let serialized_edition = serde_json::to_value(&restored_bookmarks[&1].dto).unwrap();
    assert_eq!(serialized_edition["edition"], "(Deluxe)");
    assert_eq!(restored_next_id, 3);
}

#[cfg(test)]
mod suonelo_theme_tests {
    use super::super::*;

    fn save_fixture(store: &Storage, fields: serde_json::Value) {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let settings = value.as_object_mut().unwrap();
        settings.remove("theme");
        settings.extend(fields.as_object().unwrap().clone());
        store.open().unwrap().execute(
            "INSERT INTO settings(id,value) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            [value.to_string()],
        ).unwrap();
    }

    #[test]
    fn connection_repairs_persist_once_and_keep_explicit_preferences() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, fresh, _, _) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(fresh.max_connections, MaxConnections::Count(1));
        assert_eq!(fresh.download_mode, DownloadMode::Sequential);
        assert_eq!(fresh.output.lyrics_mode, LyricsMode::External);
        for (input, expected) in [(6, 4), (14, 8), (30, 16), (-3, 1), (1000, 1), (32, 32)] {
            save_fixture(
                &store,
                serde_json::json!({
                    "revision": 9, "maxConnections": input, "downloadMode": "concurrent", "lyricsMode": "off",
                    "tags": {"qobuzAlbumId": false, "qobuzTrackId": true}, "quality": 2,
                }),
            );
            for _ in 0..2 {
                let (_, saved, _, _) = Storage::initialize(path.clone()).unwrap();
                assert_eq!(
                    saved.revision,
                    if input == i32::from(expected) { 9 } else { 10 }
                );
                assert_eq!(saved.max_connections, MaxConnections::Count(expected));
                assert_eq!(saved.download_mode, DownloadMode::Concurrent);
                assert_eq!(saved.output.lyrics_mode, LyricsMode::Off);
                assert_eq!(saved.output.tags["qobuzAlbumId"], false);
                assert_eq!(saved.output.tags["qobuzTrackId"], true);
                assert_eq!(saved.output.quality, 2);
            }
        }
        let patch = serde_json::from_value(serde_json::json!({"maxConnections": 6})).unwrap();
        assert!(fresh.apply(folder.path(), patch).is_err());
    }
    #[test]
    fn shared_theme_wins_and_invalid_shared_values_repair_once() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, fresh, _, _) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(fresh.theme.as_deref(), Some("Suonelo"));
        assert!(matches!(fresh.appearance, Appearance::System));
        for (theme, expected, revision) in [
            (serde_json::json!("Vercel"), "Vercel", 9),
            (serde_json::json!("unknown"), "Suonelo", 10),
            (serde_json::Value::Null, "Suonelo", 10),
            (serde_json::json!(42), "Suonelo", 10),
        ] {
            save_fixture(
                &store,
                serde_json::json!({
                    "theme":theme,"lightTheme":"Spotify","darkTheme":"Violet","revision":9,"maxConnections":1,
                }),
            );
            let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(loaded.theme.as_deref(), Some(expected));
            assert_eq!(loaded.revision, revision);
            assert_eq!(loaded.max_connections.limit(), 1);
            assert!(loaded.migrated_theme(8, true).unwrap().is_none());
            let (_, again, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(again.revision, revision);
        }
    }

    #[test]
    fn legacy_theme_conversion_is_revision_checked_and_preserves_other_settings() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, ..) = Storage::initialize(path.clone()).unwrap();
        for (appearance, dark, light, legacy_dark, expected) in [
            (
                "light",
                false,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Spotify",
            ),
            (
                "dark",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Violet",
            ),
            (
                "system",
                false,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Spotify",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Violet",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Spotify"),
                "Spotify",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!(42),
                "Suonelo",
            ),
        ] {
            save_fixture(
                &store,
                serde_json::json!({
                    "appearance":appearance,"lightTheme":light,"darkTheme":legacy_dark,"revision":9,
                    "folderTemplate":"%artist%     %album name%","maxConnections":1,
                }),
            );
            let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
            assert!(loaded.theme.is_none());
            assert_eq!(
                loaded.migrated_theme(8, dark).err().unwrap().code,
                "settings_conflict"
            );
            let next = loaded.migrated_theme(9, dark).unwrap().unwrap();
            assert_eq!(next.theme.as_deref(), Some(expected));
            assert_eq!(serde_json::to_value(next.appearance).unwrap(), appearance);
            assert_eq!(next.output.folder_template, "%artist%     %album name%");
            assert_eq!(next.max_connections.limit(), 1);
            store.save_settings(&next).unwrap();
            let (_, restarted, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(restarted.theme.as_deref(), Some(expected));
            assert_eq!(restarted.revision, 10);
            assert!(restarted.migrated_theme(9, !dark).unwrap().is_none());
        }

        save_fixture(
            &store,
            serde_json::json!({"appearance":"system","revision":9,"lightTheme":"Violet"}),
        );
        let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
        let next = loaded.migrated_theme(9, true).unwrap().unwrap();
        assert_eq!(next.theme.as_deref(), Some("Suonelo"));
        store.open().unwrap().execute_batch(
            "CREATE TRIGGER reject_theme BEFORE UPDATE ON settings BEGIN SELECT raise(ABORT, 'test failure'); END;"
        ).unwrap();
        assert!(store.save_settings(&next).is_err());
        let (_, after_failure, _, _) = Storage::initialize(path.clone()).unwrap();
        assert!(after_failure.theme.is_none());
        store
            .open()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_theme")
            .unwrap();
        let selected = after_failure
            .apply(
                folder.path(),
                SettingsPatch {
                    theme: Some("Spotify".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.save_settings(&selected).unwrap();
        assert!(selected.migrated_theme(9, true).unwrap().is_none());
        let (_, saved, _, _) = Storage::initialize(path).unwrap();
        assert_eq!(saved.theme.as_deref(), Some("Spotify"));
    }
}
