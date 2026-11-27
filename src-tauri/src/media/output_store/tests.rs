use super::*;

#[tokio::test]
async fn mp3_preview_receipt_cannot_satisfy_or_replace_full_audio() {
    let directory = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(directory.path()).unwrap();
    let database = root.join("receipts.sqlite3");
    #[cfg(not(target_os = "macos"))]
    crate::storage::Storage::initialize(database.clone()).unwrap();
    #[cfg(target_os = "macos")]
    Connection::open(&database).unwrap().execute_batch(RECEIPT_SCHEMA).unwrap();
    let store = OutputStore::new(database.clone(), root.clone());
    let path = root.join("track - Preview.mp3");
    let cancel = CancellationToken::new();
    let preview = Receipt { kind: "audio_preview", identity: "42".into(), variant: "mp3".into() };
    let full = Receipt { kind: "audio", ..preview.clone() };
    #[cfg(not(target_os = "macos"))]
    let full_path = {
        let full_path = root.join("track.mp3");
        let full_stage = store.stage(&root, ".mp3").await.unwrap();
        fs::write(full_stage.path(), "full audio").unwrap();
        store.publish(full_stage, &full_path, &full, &cancel).await.unwrap();
        full_path
    };
    let stage = store.stage(&root, ".mp3").await.unwrap();
    fs::write(stage.path(), "preview audio").unwrap();
    store.publish(stage, &path, &preview, &cancel).await.unwrap();
    assert!(store.verify(&path, &preview, &cancel).await.unwrap());
    assert_eq!(store.verify(&path, &full, &cancel).await.unwrap_err().code, "output_conflict");
    assert!(store.offer_replacement(&path, "42", &cancel).await.is_err());
    #[cfg(not(target_os = "macos"))]
    assert!(store.verify(&full_path, &full, &cancel).await.unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "preview audio");
    #[cfg(not(target_os = "macos"))]
    {
        let db = Connection::open(&database).unwrap();
        assert_eq!(db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), 4);
        assert_eq!(db.pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0)).unwrap(), "ok");
        fs::write(&path, "tampered preview").unwrap();
        assert!(store.verify(&path, &preview, &cancel).await.is_err());
    }
}

#[tokio::test]
async fn confirmed_audio_replacement_revalidates_the_path_identity_and_old_digest() {
    let directory = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(directory.path()).unwrap();
    let database = root.join("receipts.sqlite3");
    Connection::open(&database).unwrap().execute_batch(RECEIPT_SCHEMA).unwrap();
    let store = OutputStore::new(database, root.clone());
    let path = root.join("track.flac");
    let cancel = CancellationToken::new();
    let old = Receipt { kind: "audio", identity: "42".into(), variant: "old tags".into() };
    let new = Receipt { variant: "new tags".into(), ..old.clone() };
    let first = store.stage(&root, ".flac").await.unwrap();
    fs::write(first.path(), "old audio").unwrap();
    store.publish(first, &path, &old, &cancel).await.unwrap();
    assert!(store.offer_replacement(&path, "43", &cancel).await.is_err());
    let approval = store.offer_replacement(&path, "42", &cancel).await.unwrap();
    let unapproved = store.stage(&root, ".flac").await.unwrap();
    fs::write(unapproved.path(), "new audio").unwrap();
    assert_eq!(store.publish(unapproved, &path, &new, &cancel).await.unwrap_err().code, "output_conflict");
    fs::write(&path, "outside edit").unwrap();
    let stale = store.stage(&root, ".flac").await.unwrap();
    fs::write(stale.path(), "new audio").unwrap();
    assert_eq!(store.publish_replacement(stale, &path, &new, &approval, &cancel).await.unwrap_err().code, "output_conflict");
    assert_eq!(fs::read_to_string(&path).unwrap(), "outside edit");
    fs::write(&path, "old audio").unwrap();
    let approved = store.stage(&root, ".flac").await.unwrap();
    fs::write(approved.path(), "new audio").unwrap();
    assert!(!store.publish_replacement(approved, &path, &new, &approval, &cancel).await.unwrap());
    assert!(store.verify(&path, &new, &cancel).await.unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "new audio");
    assert!(store.verify_replacement(&path, "42", &approval, &cancel).await.is_err());
}

#[tokio::test]
async fn published_file_requires_identity_options_and_digest() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("receipts.sqlite3");
    Connection::open(&database)
        .unwrap()
        .execute_batch(RECEIPT_SCHEMA)
        .unwrap();
    let store = OutputStore::new(database, root.path().to_owned());
    let root = fs::canonicalize(root.path()).unwrap();
    let stage = store.stage(&root, ".flac").await.unwrap();
    let dest = root.join("track.flac");
    fs::write(stage.path(), "first").unwrap();
    let cancel = CancellationToken::new();
    let mut receipt = Receipt {
        kind: "audio",
        identity: "42".into(),
        variant: "default".into(),
    };
    assert!(
        !store
            .publish(stage, &dest, &receipt, &cancel)
            .await
            .unwrap()
    );
    assert!(store.verify(&dest, &receipt, &cancel).await.unwrap());
    receipt.variant = "different tags".into();
    assert_eq!(
        store
            .verify(&dest, &receipt, &cancel)
            .await
            .unwrap_err()
            .code,
        "output_conflict"
    );
    receipt.variant = "default".into();
    fs::write(&dest, "user edit").unwrap();
    assert_eq!(
        store
            .verify(&dest, &receipt, &cancel)
            .await
            .unwrap_err()
            .code,
        "output_conflict"
    );
    assert_eq!(fs::read_to_string(&dest).unwrap(), "user edit");
}

#[tokio::test]
async fn interrupted_playlist_replacement_recovers_only_a_known_digest() {
    let root = tempfile::tempdir().unwrap();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let database = root_path.join("receipts.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch(RECEIPT_SCHEMA).unwrap();
    let store = OutputStore::new(database, root_path.clone());
    let path = root_path.join("saved.m3u");
    let stage = store.stage(&root_path, ".m3u").await.unwrap();
    fs::write(stage.path(), "old export\n").unwrap();
    let cancel = CancellationToken::new();
    let receipt = Receipt {
        kind: "playlist",
        identity: "42".into(),
        variant: "m3u".into(),
    };
    store
        .publish(stage, &path, &receipt, &cancel)
        .await
        .unwrap();
    let old = file_hash(&path, None).unwrap();
    let replacement = root_path.join("next.m3u");
    fs::write(&replacement, "new export\n").unwrap();
    let new = file_hash(&replacement, None).unwrap();
    let key = store.receipt_location(&path);
    let insert = || {
        connection.execute("INSERT INTO media_pending_updates(location, old_sha256, new_sha256) VALUES (?1, ?2, ?3)", params![key, old, new]).unwrap()
    };
    insert();
    assert!(store.verify(&path, &receipt, &cancel).await.unwrap());
    insert();
    fs::write(&path, "new export\n").unwrap();
    assert!(store.verify(&path, &receipt, &cancel).await.unwrap());
    let recorded: String = connection
        .query_row(
            "SELECT sha256 FROM media_receipts WHERE location = ?1",
            [&key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, new);
    connection.execute("INSERT INTO media_pending_updates(location, old_sha256, new_sha256) VALUES (?1, ?2, ?3)", params![key, new, old]).unwrap();
    fs::write(&path, "a user changed the export\n").unwrap();
    assert_eq!(
        store
            .verify(&path, &receipt, &cancel)
            .await
            .unwrap_err()
            .code,
        "output_conflict"
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "a user changed the export\n"
    );
}
