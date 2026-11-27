use super::*;
use crate::{
    app::Destination,
    search::activity::{Credit, event_time},
};
use std::{fs, os::windows::fs::OpenOptionsExt};

fn candidate(id: &str, token: &str) -> ValidatedAccount {
    ValidatedAccount {
        credentials: Credentials {
            token: token.into(),
            app_id: "123456789".into(),
            app_secret: "synthetic-key".into(),
            transport: crate::qobuz::TokenTransport::Query,
            access_mode: crate::qobuz::AccessMode::Unknown,
        },
        id: id.into(),
    }
}

#[tokio::test]
async fn account_commit_keeps_state_on_decline_or_vault_failure_and_clears_only_for_switch() {
    let directory = tempfile::tempdir().unwrap();
    let app = App::initialize_root(directory.path().to_owned()).unwrap();
    let vault = app.root.join("data/credentials.vault");
    {
        let mut state = app.state();
        state
            .vault
            .save(candidate("101", "original").credentials, "101".into())
            .unwrap();
        commit_candidate(&app, &mut state, candidate("101", "original"), None).unwrap();
    }
    app.submit(
        "https://open.qobuz.com/album/fixture-release".into(),
        Destination::Downloads,
    )
    .unwrap();
    let credit = Credit {
        artist: "9".into(),
        release: "fixture-release".into(),
    };
    app.storage
        .record_search_activity([&credit], 2, event_time())
        .unwrap();

    let mut state = app.state();
    let queue = state.queue.snapshot();
    assert_eq!(queue.jobs.len(), 1);
    assert!(confirmation(&state, &candidate("101", "new-token")).is_none());
    assert!(
        confirmation(&state, &candidate("202", "new-token")) == Some(AccountConfirmation::Switch)
    );

    let original = fs::read(&vault).unwrap();
    let cancellation = state.account_cancel.child_token();
    state.account_attempt = Some(AccountAttempt {
        id: "first".into(),
        cancel: cancellation.clone(),
        expires: Instant::now() + Duration::from_secs(1),
        pending: Some((candidate("202", "new-token"), AccountConfirmation::Switch)),
    });
    assert!(take_attempt(&mut state, "wrong").is_err());
    cancel_attempt(&mut state, "first");
    assert!(cancellation.is_cancelled());
    assert!(take_attempt(&mut state, "first").is_err());
    assert_eq!(state.queue.snapshot().revision, queue.revision);
    assert_eq!(fs::read(&vault).unwrap(), original);

    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&vault)
        .unwrap();
    let failure = commit_candidate(
        &app,
        &mut state,
        candidate("202", "new-token"),
        Some(AccountConfirmation::Switch),
    )
    .err()
    .expect("locked vault prevents replacement");
    assert_eq!(failure.code, "vault_storage");
    drop(locked);
    assert_eq!(fs::read(&vault).unwrap(), original);
    assert_eq!(state.account.as_ref().unwrap().token, "original");
    assert_eq!(state.queue.snapshot().revision, queue.revision);

    commit_candidate(&app, &mut state, candidate("101", "same-account"), None).unwrap();
    assert_eq!(state.queue.snapshot().jobs.len(), 1);
    let old_epoch = state.account_epoch;
    let old_cancel = state.account_cancel.clone();
    commit_candidate(
        &app,
        &mut state,
        candidate("202", "other-account"),
        Some(AccountConfirmation::Switch),
    )
    .unwrap();
    assert!(old_cancel.is_cancelled());
    assert!(state.account_epoch > old_epoch);
    assert!(state.queue.snapshot().jobs.is_empty());
    assert!(state.queue.snapshot().confirmations.is_empty());
    assert_eq!(crate::vault::Vault::new(vault).account_id(), Some("202"));
    assert_eq!(app.storage.search_scores().unwrap().0["9"].0, 2);
}
