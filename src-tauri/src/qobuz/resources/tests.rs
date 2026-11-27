use super::*;
use serde_json::json;

#[test]
fn deduplication_keys_ignore_storefront_and_tracking() {
    let storefront =
        normalize_link("https://www.qobuz.com/us-en/album/title/0093624949091?foo=bar#b")
            .unwrap();
    let direct = normalize_link("https://open.qobuz.com/album/0093624949091").unwrap();
    assert_eq!(storefront.key, direct.key);
    assert_ne!(storefront.original_url, direct.original_url);
    assert_eq!(
        normalize_link("https://qobuz.com/fr-fr/interpreter/name/12")
            .unwrap()
            .key
            .kind,
        ResourceKind::Artist
    );
    assert_ne!(
        normalize_link("https://qobuz.com/track/0093624949091")
            .unwrap()
            .key,
        direct.key
    );
    assert_eq!(
        normalize_link("https://www.qobuz.com/gb-en/playlists/top-uk/5624881")
            .unwrap()
            .key,
        normalize_link("https://play.qobuz.com/playlist/5624881")
            .unwrap()
            .key,
    );
}

#[test]
fn rejects_invalid_or_unsupported_urls() {
    for url in [
        "http://qobuz.com/track/5",
        "https://qobuz.com.attacker.test/track/5",
        "https://qobuz.com/search/5",
        "https://qobuz.com/track/",
        "https://qobuz.com:444/track/5",
        "https://person@qobuz.com/track/5",
        "https://qobuz.com/track/folder/5/6",
    ] {
        assert!(normalize_link(url).is_err(), "{url}");
    }
}

#[test]
fn playlist_occurrences_share_the_album_without_changing_track_identity() {
    let album = Arc::new(json!({"id": "album-id", "media_count": 2}));
    let track = json!({"id": 55, "title": "Track", "version": "Live", "media_number": 2, "track_number": 3});
    let first = resolved_track(track.clone(), album.clone(), None, false);
    let second = resolved_track(track, album, None, false);
    assert_eq!(
        (&first.id, first.disc, first.number),
        (&second.id, second.disc, second.number)
    );
    assert_eq!(first.title, "Track (Live)");
    assert!(Arc::ptr_eq(&first.album, &second.album));
}

#[test]
fn album_version_and_format_album_title_normalize_parentheses_and_avoid_duplicates() {
    assert_eq!(album_version(&json!({"version": "Deluxe Edition"})).as_deref(), Some("(Deluxe Edition)"));
    assert_eq!(album_version(&json!({"version": " ( Remastered ) "})).as_deref(), Some("(Remastered)"));
    assert_eq!(album_version(&json!({"version": "(Live at Montreux)"})).as_deref(), Some("(Live at Montreux)"));
    assert_eq!(album_version(&json!({"version": ""})), None);
    assert_eq!(album_version(&json!({"version": "   "})), None);
    assert_eq!(album_version(&json!({"version": "()"})), None);
    assert_eq!(album_version(&json!({"version": "(  )"})), None);
    assert_eq!(album_version(&json!({"version": null})), None);
    assert_eq!(album_version(&json!({})), None);

    assert_eq!(
        format_album_title(&json!({"title": "  Moonlight ", "version": "Deluxe Edition"})).as_deref(),
        Some("Moonlight (Deluxe Edition)")
    );
    assert_eq!(
        format_album_title(&json!({"title": "Moonlight", "version": "(Remastered)"})).as_deref(),
        Some("Moonlight (Remastered)")
    );
    assert_eq!(
        format_album_title(&json!({"title": "Moonlight", "version": null})).as_deref(),
        Some("Moonlight")
    );
    assert_eq!(
        format_album_title(&json!({"title": "Moonlight", "version": "  "})).as_deref(),
        Some("Moonlight")
    );
    assert_eq!(
        format_album_title(&json!({"title": "Moonlight (Deluxe)", "version": "deluxe"})).as_deref(),
        Some("Moonlight (Deluxe)")
    );
    assert_eq!(
        format_album_title(&json!({"title": "Moonlight (Deluxe Edition)", "version": "(DELUXE EDITION)"})).as_deref(),
        Some("Moonlight (Deluxe Edition)")
    );
    assert_eq!(format_album_title(&json!({"title": "  ", "version": "Deluxe"})), None);
    assert_eq!(format_album_title(&json!({"version": "Deluxe"})), None);
}

#[test]
fn artist_selection_prioritizes_quality_then_standard_edition() {
    let edition = |id, title, depth, rate, date| {
        json!({
            "id": id, "title": title, "maximum_bit_depth": depth, "maximum_sampling_rate": rate,
            "release_date_original": date, "artist": {"name": "Test Artist"},
        })
    };
    let entries = vec![
        edition("01", "A Long Album Title", 16, 44.1, "2023-01-01"),
        edition("02", "A Long Album Title (Deluxe)", 24, 96.0, "2024-01-01"),
        edition("03", "A Long Album Titles", 24, 96.0, "2022-01-01"),
        edition(
            "04",
            "A Long Album Title (Remastered)",
            24,
            48.0,
            "2025-01-01",
        ),
        edition("05", "Other recording", 16, 44.1, "2025-01-01"),
    ];
    let selected = best_artist_editions(&json!({"name": "Test Artist"}), entries);
    assert_eq!(
        selected
            .iter()
            .map(|album| album["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["03", "05"]
    );
    assert!(is_album_release(&json!({"release_type": "album"})));
    assert!(!is_album_release(
        &json!({"release_type": "album", "artist": {"name": "Various Artists"}})
    ));
}
