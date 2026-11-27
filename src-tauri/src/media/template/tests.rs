use super::*;

#[test]
fn cover_layout_shares_identified_roots_and_uses_a_safe_disc_first_fallback() {
    use serde_json::json;
    use std::sync::Arc;
    let root = Path::new("C:/Music");
    let track = ResolvedTrack {
        id: "11".into(),
        title: "Song".into(),
        album_id: "001release".into(),
        disc: 2,
        number: 1,
        track: json!({"title":"Song"}),
        album: Arc::new(json!({"title":"Album","artist":{"name":"Artist"},"media_count":2})),
        unavailable: None,
    };
    for (template, audio, cover) in [
        (
            "%album artist%/%album name%",
            "Artist/Album/Disc 2",
            "Artist/Album",
        ),
        (
            "%album artist%/%album name%/Disc %disc number%",
            "Artist/Album/Disc 2",
            "Artist/Album",
        ),
        (
            "%album artist%/Disc %disc number%/%album name%",
            "Artist/Disc 2/Album",
            "Artist/Album [001release]",
        ),
        (
            "Disc %disc number%/%album name%",
            "Disc 2/Album",
            "Album [001release]",
        ),
        (
            "%album artist%",
            "Artist/Disc 2",
            "Artist/Album [001release]",
        ),
        (
            "[%album name% %year%]/Disc %disc number%/%album name%",
            "Disc 2/Album",
            "Album [001release]",
        ),
        (
            "%album id%/Disc %disc number%",
            "001release/Disc 2",
            "001release",
        ),
    ] {
        let options = OutputOptions {
            folder_template: template.into(),
            ..Default::default()
        };
        let (path, layout) = plan_output(
            root,
            &options,
            &track,
            &DeliveredQuality::requested(4),
            None,
            1,
        )
        .unwrap();
        assert_eq!(
            path.parent(),
            Some(root.join(audio).as_path()),
            "{template}"
        );
        assert_eq!(layout.cover_dir, Some(root.join(cover)), "{template}");
        let first = ResolvedTrack {
            disc: 1,
            ..track.clone()
        };
        let (_, pinned) = plan_output(
            Path::new("D:/Elsewhere"),
            &options,
            &first,
            &DeliveredQuality::requested(2),
            Some(&layout),
            1,
        )
        .unwrap();
        assert_eq!(pinned.cover_dir, layout.cover_dir);
    }

    let options = OutputOptions {
        folder_template: "Disc %disc number%/%album name%".into(),
        ..Default::default()
    };
    let fallback = |track: &ResolvedTrack| {
        plan_output(
            root,
            &options,
            track,
            &DeliveredQuality::requested(4),
            None,
            1,
        )
        .unwrap()
        .1
        .cover_dir
        .unwrap()
    };
    let other = ResolvedTrack {
        album_id: "002release".into(),
        ..track.clone()
    };
    assert_ne!(fallback(&track), fallback(&other));
    for title in [json!(null), json!("***")] {
        let missing = ResolvedTrack {
            album: Arc::new(json!({"title":title,"media_count":2})),
            ..track.clone()
        };
        assert_eq!(fallback(&missing), root.join("[001release]"));
    }
    let long = ResolvedTrack {
        album: Arc::new(json!({"title":"é".repeat(150),"media_count":2})),
        ..track.clone()
    };
    let long = fallback(&long);
    let name = long.file_name().unwrap().to_str().unwrap();
    assert!(name.len() <= 220 && name.ends_with(" [001release]"));
    let one_disc = ResolvedTrack {
        album: Arc::new(json!({"title":"Album","media_count":1})),
        disc: 1,
        ..track
    };
    assert_eq!(fallback(&one_disc), root.join("Disc 1/Album"));
}

#[test]
fn authoritative_naming_and_safe_mode_fixtures_match_download_paths() {
    use serde_json::{Value, json};

    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/implementation-fixtures.json"
    ))
    .unwrap();
    for sample in fixtures["safeComponents"].as_array().unwrap() {
        let input = sample["input"].as_str().unwrap();
        assert_eq!(
            sanitize(input, true),
            sample["expectedOn"].as_str().unwrap(),
            "{}",
            sample["id"]
        );
        assert_eq!(
            sanitize(input, false),
            sample["expectedOff"].as_str().unwrap(),
            "{}",
            sample["id"]
        );
    }
    for control in '\0'..='\u{1f}' {
        let input = format!("A{control}B");
        assert_eq!(sanitize(&input, true), "AB");
        assert_eq!(sanitize(&input, false), "A_B");
    }
    let defaults = OutputOptions::default();
    assert_eq!(
        defaults.folder_template,
        fixtures["latestDefaults"]["folderTemplate"]
            .as_str()
            .unwrap()
    );
    assert_eq!(
        defaults.filename_template,
        fixtures["latestDefaults"]["filenameTemplate"]
            .as_str()
            .unwrap()
    );
    for case in fixtures["naming"].as_array().unwrap() {
        let mut values = fixtures["sampleMetadata"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str().unwrap().to_owned()))
            .collect::<BTreeMap<_, _>>();
        if let Some(overrides) = case["metadataOverrides"].as_object() {
            for (key, value) in overrides {
                if value.is_null() {
                    values.remove(key.as_str());
                } else {
                    values.insert(key, value.as_str().unwrap().into());
                }
            }
        }
        if let Some(component) = case["componentTemplate"].as_str() {
            let template = parse_template(component, "%track title%").unwrap();
            assert_eq!(
                render_folders(&template.folders, &values, true).to_string_lossy(),
                case["expectedComponent"].as_str().unwrap(),
                "{}",
                case["id"]
            );
            continue;
        }
        let options = OutputOptions {
            quality: case["requestedTier"].as_u64().unwrap_or(4) as u8,
            folder_template: case["folderTemplate"]
                .as_str()
                .unwrap_or(&defaults.folder_template)
                .into(),
            filename_template: case["filenameTemplate"]
                .as_str()
                .unwrap_or(&defaults.filename_template)
                .into(),
            ..defaults.clone()
        };
        let mut delivered = DeliveredQuality::requested(options.quality);
        if case["delivered"].is_object() {
            delivered.bit_depth = case["delivered"]["bitDepth"]
                .as_u64()
                .map(|depth| depth as u32);
            delivered.sample_rate = case["delivered"]["samplingRateKHz"].as_f64();
        }
        let track = ResolvedTrack {
            id: "3".into(), title: values["track title"].clone(), album_id: values["album id"].clone(),
            disc: 1, number: 3, unavailable: None,
            track: json!({"title": values["track title"], "performer": {"name": values["artist"]}}),
            album: json!({"title": values["album name"], "artist": {"name": values["album artist"]}, "release_date_original": values.get("year").map(|year| format!("{year}-01-01")), "media_count": 1}).into(),
        };
        let (path, _) =
            plan_output(Path::new(""), &options, &track, &delivered, None, 3).unwrap();
        assert_eq!(
            path.to_string_lossy().replace('\\', "/"),
            case["expectedRelativePath"].as_str().unwrap(),
            "{}",
            case["id"]
        );
    }
}

#[test]
fn template_rejects_traversal_and_unknown_tokens() {
    for folder in [
        "../foo",
        "C:/foo",
        "//server/share",
        "x//y",
        "%unknown%",
        "[(no token)]",
    ] {
        assert!(parse_template(folder, "name").is_err(), "{folder}");
    }
    assert!(parse_template("folder", "one/two").is_err());
}

#[test]
fn preview_and_optional_groups() {
    let path = preview_output(
        "%album artist%/%album name%[ (%year%)]",
        "%track number% - %track title%",
        4,
        true,
    )
    .unwrap();
    assert_eq!(
        path,
        PathBuf::from("Aster Vale")
            .join("Music for the Moon (2026)")
            .join("03 - Quiet orbit.flac")
    );
    let template = parse_template("Records/[Year %year%]/Album[ (%year%)]", "(Title)").unwrap();
    assert_eq!(
        render_folders(&template.folders, &BTreeMap::new(), true),
        PathBuf::from("Records").join("Album")
    );
    assert_eq!(
        template.filename(&BTreeMap::new(), &DeliveredQuality::requested(1), 3, true),
        "(Title).mp3"
    );
}

#[test]
fn optional_year_spacing_stays_literal_in_both_forms() {
    for folder in ["%album name%[ (%year%)]", "%album name% [(%year%)]"] {
        assert_eq!(
            preview_output(folder, "%track title%", 4, true).unwrap(),
            PathBuf::from("Music for the Moon (2026)").join("Quiet orbit.flac")
        );
        let template = parse_template(folder, "%track title%").unwrap();
        let values = BTreeMap::from([("album name", "Music for the Moon".into())]);
        assert_eq!(
            render_folders(&template.folders, &values, true),
            PathBuf::from("Music for the Moon")
        );
    }

    let values = BTreeMap::from([("album name", "Music for the Moon".into())]);
    for (folder, expected) in [
        (
            "%album name%[ (%year%)] - Deluxe",
            "Music for the Moon - Deluxe",
        ),
        (
            "%album name% [(%year%)] - Deluxe",
            "Music for the Moon  - Deluxe",
        ),
    ] {
        let template = parse_template(folder, "%track title%").unwrap();
        assert_eq!(
            render_folders(&template.folders, &values, true),
            PathBuf::from(expected)
        );
    }
}

#[test]
fn folder_and_filename_preserve_interior_spaces_and_trim_edges() {
    assert_eq!(
        preview_output(
            "  %artist%     %album name%  ",
            "  %track number%     %track title%  ",
            4,
            true,
        )
        .unwrap(),
        PathBuf::from("Aster Vale     Music for the Moon").join("03     Quiet orbit.flac")
    );
}

#[test]
fn multiple_optional_groups_in_each_field_render_independently() {
    let path = preview_output(
        "%album artist%/%album name%[ (%year%)][ - %genre%]",
        "[%disc number% - ]%track number% - %track title%[ (%bit depth% bit)][ (%sampling rate% kHz)]",
        4,
        true,
    )
    .unwrap();
    assert_eq!(
        path,
        PathBuf::from("Aster Vale")
            .join("Music for the Moon (2026) - Electronic")
            .join("1 - 03 - Quiet orbit (24 bit) (192 kHz).flac")
    );

    let template = parse_template(
        "Album[ (%year%)][ - %genre%]",
        "%track title%[ (%bit depth% bit)][ (%sampling rate% Hz)]",
    )
    .unwrap();
    let values = BTreeMap::from([
        ("genre", "Electronic".into()),
        ("track title", "Quiet orbit".into()),
        ("sampling rate", "44100".into()),
    ]);
    assert_eq!(
        render_folders(&template.folders, &values, true),
        PathBuf::from("Album - Electronic")
    );
    assert_eq!(
        template.filename(&values, &DeliveredQuality::requested(2), 1, true),
        "Quiet orbit (44100 Hz).flac"
    );
}

#[test]
fn optional_groups_allow_literal_brackets_and_album_title_alias() {
    let folder = "[(%year%)] %album title% [[%bit depth%-Bits]]";
    assert_eq!(
        preview_output(folder, "%track title%", 4, true).unwrap(),
        PathBuf::from("(2026) Music for the Moon [24-Bits]").join("Quiet orbit.flac")
    );
    assert_eq!(
        preview_output(folder, "%track title% [[%bit depth%-Bits]]", 1, true).unwrap(),
        PathBuf::from("(2026) Music for the Moon").join("Quiet orbit.mp3")
    );
    for folder in ["[[%year%]", "[%year%]]", "[[text]]", "[%year%"] {
        assert!(parse_template(folder, "%track title%").is_err(), "{folder}");
    }
}

#[test]
fn missing_metadata_keeps_stable_serials_album_identity_and_discs() {
    use serde_json::json;

    let root = Path::new("Music");
    let options = OutputOptions {
        folder_template: "%album artist%/%album name% [(%year%)]".into(),
        filename_template: "%track number% - %track title%".into(),
        ..Default::default()
    };
    let track = ResolvedTrack {
        id: "33".into(),
        title: "Unavailable track".into(),
        album_id: "album42".into(),
        disc: 2,
        number: 0,
        track: json!({}),
        album: json!({"media_count": 2}).into(),
        unavailable: None,
    };
    let quality = DeliveredQuality::requested(2);
    let (path, layout) = plan_output(root, &options, &track, &quality, None, 3).unwrap();
    assert_eq!(path, root.join("album42").join("Disc 2").join("03.flac"));
    let optional_disc = OutputOptions {
        folder_template: "%album name%/[%genre% Disc %disc number%]".into(),
        ..options
    };
    let (path, _) = plan_output(root, &optional_disc, &track, &quality, None, 3).unwrap();
    assert_eq!(path, root.join("album42").join("Disc 2").join("03.flac"));
    let changed = OutputOptions {
        folder_template: "Changed".into(),
        quality: 1,
        ..optional_disc
    };
    let (path, _) = plan_output(
        Path::new("Other"),
        &changed,
        &track,
        &DeliveredQuality::requested(1),
        Some(&layout),
        3,
    )
    .unwrap();
    assert_eq!(path, root.join("album42").join("Disc 2").join("03.mp3"));
}

#[test]
fn album_version_populates_album_tokens_optional_groups_cover_fallback_and_safe_mode() {
    use serde_json::json;
    use std::sync::Arc;

    let root = Path::new("C:/Music");
    let track = ResolvedTrack {
        id: "11".into(),
        title: "Song".into(),
        album_id: "001release".into(),
        disc: 2,
        number: 1,
        track: json!({"title": "Song"}),
        album: Arc::new(json!({
            "title": "Album",
            "version": "Deluxe: Live?",
            "artist": {"name": "Artist"},
            "media_count": 2
        })),
        unavailable: None,
    };
    let options = OutputOptions {
        folder_template: "%album artist%/%album title%/Disc %disc number%".into(),
        filename_template: "%album name% - %track title%[ %album version%]".into(),
        safe_mode: true,
        ..Default::default()
    };
    let (path, layout) = plan_output(
        root,
        &options,
        &track,
        &DeliveredQuality::requested(4),
        None,
        1,
    )
    .unwrap();
    assert_eq!(
        path,
        root.join("Artist")
            .join("Album (Deluxe Live)")
            .join("Disc 2")
            .join("Album (Deluxe Live) - Song (Deluxe Live).flac")
    );
    assert_eq!(
        layout.cover_dir,
        Some(root.join("Artist").join("Album (Deluxe Live)"))
    );

    let fallback_options = OutputOptions {
        folder_template: "Disc %disc number%/%album name%".into(),
        filename_template: "%track title%[ %album version%]".into(),
        ..Default::default()
    };
    let (_, fallback_layout) = plan_output(
        root,
        &fallback_options,
        &track,
        &DeliveredQuality::requested(4),
        None,
        1,
    )
    .unwrap();
    assert_eq!(
        fallback_layout.cover_dir,
        Some(root.join("Album (Deluxe Live) [001release]"))
    );

    let unversioned = ResolvedTrack {
        album: Arc::new(json!({
            "title": "Album",
            "version": null,
            "artist": {"name": "Artist"},
            "media_count": 1
        })),
        disc: 1,
        ..track.clone()
    };
    let (plain_path, _) = plan_output(
        root,
        &fallback_options,
        &unversioned,
        &DeliveredQuality::requested(4),
        None,
        1,
    )
    .unwrap();
    assert_eq!(
        plain_path,
        root.join("Disc 1").join("Album").join("Song.flac")
    );

    let already_versioned = ResolvedTrack {
        album: Arc::new(json!({
            "title": "Album (Deluxe)",
            "version": "deluxe",
            "artist": {"name": "Artist"},
            "media_count": 1
        })),
        ..unversioned
    };
    let dedup_options = OutputOptions {
        folder_template: "%album artist%/%album title%".into(),
        filename_template: "%track title%".into(),
        ..Default::default()
    };
    let (dedup_path, _) = plan_output(
        root,
        &dedup_options,
        &already_versioned,
        &DeliveredQuality::requested(4),
        None,
        1,
    )
    .unwrap();
    assert_eq!(
        dedup_path,
        root.join("Artist").join("Album (Deluxe)").join("Song.flac")
    );

    assert_eq!(
        preview_output(
            "%album artist%/%album name%[ %album version%]",
            "%track number% - %track title%",
            4,
            true,
        )
        .unwrap(),
        PathBuf::from("Aster Vale")
            .join("Music for the Moon (Deluxe Edition)")
            .join("03 - Quiet orbit.flac")
    );
}
