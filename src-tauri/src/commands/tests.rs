use super::settings::save_settings_patch;
use crate::{
    app::App,
    storage::{MaxConnections, SettingsPatch},
};

#[test]
fn failed_settings_write_preserves_the_saved_and_live_limit() {
    let root = tempfile::tempdir().unwrap();
    let app = App::initialize_root(root.path().to_owned()).unwrap();
    let mut state = app.state();
    app.storage.save_settings(&state.settings).unwrap();
    let db = app.storage.open().unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_settings BEFORE UPDATE ON settings
         BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;",
    )
    .unwrap();
    let before = state.queue.snapshot();
    let expected_revision = state.settings.revision;
    let mut patch = SettingsPatch::default();
    patch.max_connections = Some(MaxConnections::Count(8));
    assert!(save_settings_patch(&app, &mut state, expected_revision, patch, true).is_err());
    assert_eq!(state.settings.revision, expected_revision);
    assert_eq!(state.settings.max_connections, MaxConnections::Count(1));
    let after = state.queue.snapshot();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.live_limit, before.live_limit);
    let saved: String = db
        .query_row("SELECT value FROM settings WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&saved).unwrap()["maxConnections"],
        1
    );
}
