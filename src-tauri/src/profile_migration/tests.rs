use super::*;
fn legacy(data: &Path) -> Connection {
    fs::create_dir_all(data).unwrap();
    let db = Connection::open(data.join(LEGACY)).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA user_version=2;CREATE TABLE settings(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO settings VALUES(1,'saved preferences');CREATE TABLE bookmarks(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO bookmarks VALUES(1,'saved bookmark');").unwrap();
    fs::write(data.join("credentials.vault"), b"opaque encrypted fixture").unwrap();
    db
}
#[test]
fn migrates_wal_preserves_records_and_vault_and_restarts() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let db = legacy(&data);
    // The migration owns the profile lock in production; fixture leaves a WAL connection open.
    drop(db);
    let current = prepare(&data).unwrap();
    assert_eq!(current, data.join(CURRENT));
    assert_eq!(
        open(&current)
            .unwrap()
            .query_row("SELECT count(*) FROM bookmarks", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        fs::read(data.join("credentials.vault")).unwrap(),
        b"opaque encrypted fixture"
    );
    assert!(!data.join(LEGACY).exists());
    assert!(data.join(JOURNAL).exists());
    assert_eq!(prepare(&data).unwrap(), current);
}
#[test]
fn conflicting_databases_are_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    drop(legacy(&data));
    fs::copy(data.join(LEGACY), data.join(CURRENT)).unwrap();
    let before = fs::read(data.join(CURRENT)).unwrap();
    assert!(prepare(&data).is_err());
    assert_eq!(fs::read(data.join(CURRENT)).unwrap(), before);
}
#[test]
fn interrupted_steps_resume_without_data_loss() {
    for stop in ["backup", "temporary", "published", "archived"] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        drop(legacy(&data));
        assert!(
            prepare_steps(&data, |step| if step == stop {
                Err(problem("injected interruption"))
            } else {
                Ok(())
            })
            .is_err()
        );
        let current = prepare(&data).unwrap();
        assert_eq!(
            open(&current)
                .unwrap()
                .query_row("SELECT value FROM settings", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "saved preferences"
        );
    }
}
#[test]
fn disk_full_error_before_publication_keeps_profile_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    drop(legacy(&data));
    let before = fingerprint(&open(&data.join(LEGACY)).unwrap()).unwrap();
    let error = prepare_steps(&data, |step| {
        if step == "temporary" {
            Err(io(std::io::Error::from(std::io::ErrorKind::StorageFull)))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(error.message.contains("recoverable"));
    assert!(!data.join(CURRENT).exists());
    assert_eq!(
        fingerprint(&open(&data.join(LEGACY)).unwrap()).unwrap(),
        before
    );
    assert_eq!(
        fs::read(data.join("credentials.vault")).unwrap(),
        b"opaque encrypted fixture"
    );
    let current = prepare(&data).unwrap();
    assert_eq!(fingerprint(&open(&current).unwrap()).unwrap(), before);
}

#[test]
fn source_change_after_interruption_is_a_recoverable_conflict() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    drop(legacy(&data));
    assert!(prepare_steps(&data, |_| Err(problem("stop"))).is_err());
    open(&data.join(LEGACY))
        .unwrap()
        .execute("INSERT INTO bookmarks VALUES(2,'newer')", [])
        .unwrap();
    assert!(prepare(&data).is_err());
    assert!(data.join(LEGACY).exists());
}
#[test]
fn corrupt_and_newer_profiles_are_preserved() {
    for corrupt in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        drop(legacy(&data));
        if corrupt {
            fs::write(data.join(LEGACY), b"not sqlite").unwrap();
        } else {
            open(&data.join(LEGACY))
                .unwrap()
                .pragma_update(None, "user_version", 99)
                .unwrap();
        }
        assert!(prepare(&data).is_err());
        assert!(data.join(LEGACY).exists());
    }
}
#[test]
fn fresh_profile_does_not_invent_a_migration() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    fs::create_dir(&data).unwrap();
    assert_eq!(prepare(&data).unwrap(), data.join(CURRENT));
    assert!(!data.join(JOURNAL).exists());
}
