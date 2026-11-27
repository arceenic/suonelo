use super::*;
use serde_json::json;

#[test]
fn advanced_catalog_keeps_raw_identity_exports_and_sensitive_filters() {
    let track = ResolvedTrack {
        id: "3".into(), title: "Song".into(), album_id: "album42".into(), disc: 1, number: 3, unavailable: None,
        track: json!({"title": "Song", "user_token": "secret", "signed_url": "https://example.invalid/signed", "audio_info": {"replaygain_track_gain": -5.0}}),
        album: json!({"title": "Album", "genre": {"name": "Electronic"}, "release_date_original": "2026-01-01"}).into(),
    };
    let catalog = catalog_track(&track);
    let raw = catalog
        .iter()
        .find(|field| field.id == "album/genre/name")
        .unwrap();
    assert_eq!(raw.label, "Genre / Name");
    assert_eq!(raw.group, Some("album"));
    assert_eq!(raw.raw_path.as_deref(), Some("album/genre/name"));
    assert_eq!(raw.export_tag.as_deref(), Some("QOBUZ:album/genre/name"));
    assert!(!raw.default_enabled);
    assert!(
        catalog
            .iter()
            .filter(|field| field.id.contains('/'))
            .all(|field| !field.id.contains("token") && !field.id.contains("url"))
    );
    let mut options = OutputOptions::default();
    for id in ["qobuzAlbumId", "qobuzTrackId"] {
        let field = STANDARD.iter().find(|field| field.id == id).unwrap();
        assert!(
            catalog
                .iter()
                .find(|field| field.id == id)
                .unwrap()
                .default_enabled
        );
        assert!(enabled(&options, field));
        options.tags.insert(id.into(), false);
        assert!(!enabled(&options, field));
    }
    assert!(!custom_tags(&track, &options).contains_key("QOBUZ:album/genre/name"));
    options.tags.insert("album/genre/name".into(), true);
    assert_eq!(
        custom_tags(&track, &options)["QOBUZ:album/genre/name"],
        ["Electronic"]
    );
    options.tags.insert("album/genre/name".into(), false);
    assert!(!custom_tags(&track, &options).contains_key("QOBUZ:album/genre/name"));
    assert!(
        catalog
            .iter()
            .find(|field| field.id == "genre")
            .unwrap()
            .raw_path
            .is_none()
    );
}

#[test]
fn standard_values_and_write_tags_include_album_version_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let options = OutputOptions::default();

    let versioned_track = ResolvedTrack {
        id: "10".into(),
        title: "Song".into(),
        album_id: "album10".into(),
        disc: 1,
        number: 1,
        track: json!({"title": "Song"}),
        album: json!({
            "title": "Moonlight",
            "version": "Deluxe Edition",
            "artist": {"name": "Aster Vale"},
            "media_count": 1
        })
        .into(),
        unavailable: None,
    };
    let plain_track = ResolvedTrack {
        album: json!({
            "title": "Moonlight",
            "version": null,
            "artist": {"name": "Aster Vale"},
            "media_count": 1
        })
        .into(),
        ..versioned_track.clone()
    };

    assert_eq!(
        standard_values(&versioned_track)["album"],
        ["Moonlight (Deluxe Edition)"]
    );
    assert_eq!(standard_values(&plain_track)["album"], ["Moonlight"]);

    let flac_fixture = include_bytes!("../../../tests/fixtures/pipeline-pcm16-mono.flac");
    let flac_versioned = dir.path().join("versioned.flac");
    let flac_plain = dir.path().join("plain.flac");
    std::fs::write(&flac_versioned, flac_fixture).unwrap();
    std::fs::write(&flac_plain, flac_fixture).unwrap();

    write_tags(
        &flac_versioned,
        AudioFormat::Flac,
        &versioned_track,
        &options,
        None,
        None,
    )
    .unwrap();
    write_tags(
        &flac_plain,
        AudioFormat::Flac,
        &plain_track,
        &options,
        None,
        None,
    )
    .unwrap();

    let flac_v = FlacFile::read_from(
        &mut File::open(&flac_versioned).unwrap(),
        ParseOptions::new(),
    )
    .unwrap();
    assert_eq!(
        flac_v.vorbis_comments().unwrap().get("ALBUM"),
        Some("Moonlight (Deluxe Edition)")
    );
    let flac_p =
        FlacFile::read_from(&mut File::open(&flac_plain).unwrap(), ParseOptions::new())
            .unwrap();
    assert_eq!(
        flac_p.vorbis_comments().unwrap().get("ALBUM"),
        Some("Moonlight")
    );

    let mut mp3_frame = vec![0_u8; 417];
    mp3_frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    let mp3_bytes = mp3_frame.repeat(10);
    let mp3_versioned = dir.path().join("versioned.mp3");
    let mp3_plain = dir.path().join("plain.mp3");
    std::fs::write(&mp3_versioned, &mp3_bytes).unwrap();
    std::fs::write(&mp3_plain, &mp3_bytes).unwrap();

    write_tags(
        &mp3_versioned,
        AudioFormat::Mp3,
        &versioned_track,
        &options,
        None,
        None,
    )
    .unwrap();
    write_tags(
        &mp3_plain,
        AudioFormat::Mp3,
        &plain_track,
        &options,
        None,
        None,
    )
    .unwrap();

    let mp3_v = MpegFile::read_from(
        &mut File::open(&mp3_versioned).unwrap(),
        ParseOptions::new(),
    )
    .unwrap();
    assert_eq!(
        mp3_v
            .id3v2()
            .unwrap()
            .get_text(&FrameId::Valid(Cow::Borrowed("TALB"))),
        Some("Moonlight (Deluxe Edition)")
    );
    let mp3_p =
        MpegFile::read_from(&mut File::open(&mp3_plain).unwrap(), ParseOptions::new())
            .unwrap();
    assert_eq!(
        mp3_p
            .id3v2()
            .unwrap()
            .get_text(&FrameId::Valid(Cow::Borrowed("TALB"))),
        Some("Moonlight")
    );
}
