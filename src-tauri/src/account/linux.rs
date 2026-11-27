use super::{
    AccountAttempt, AccountConfirmation, ValidatedAccount, ValidationResult, account_changed,
    account_waiting, activate_candidate, confirmation, finish_vault_reset,
};
use crate::{
    app::{App, AppState},
    error::{AppError, Result},
};
use std::sync::Arc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) struct LinuxVaultWrite {
    pub(super) attempt: Option<String>,
    pub(super) cancel: CancellationToken,
    pub(crate) authorized: bool,
}

pub(super) enum LinuxAccountChange {
    Confirmation(ValidationResult),
    Reset,
    Commit {
        candidate: ValidatedAccount,
        confirmed: Option<AccountConfirmation>,
        attempt: Option<AccountAttempt>,
    },
}

pub(super) fn linux_vault_busy() -> AppError {
    AppError::new(
        "vault_loading",
        "account",
        "Wait for the current Linux keyring operation to finish.",
        true,
    )
}

pub(super) async fn linux_account_change(
    app: Arc<App>,
    prepare: impl FnOnce(&mut AppState) -> Result<LinuxAccountChange>,
) -> Result<ValidationResult> {
    let _operation = app.vault_operations.lock().await;
    let (change, update, fingerprint, cancel, epoch) = {
        let mut state = app.state();
        state.ensure_open()?;
        let change = prepare(&mut state)?;
        if let LinuxAccountChange::Confirmation(result) = change {
            return Ok(result);
        }
        let (update, fingerprint, attempt, cancel) = match &change {
            LinuxAccountChange::Reset => (
                state.vault.linux_reset()?,
                None,
                None,
                app.stop.child_token(),
            ),
            LinuxAccountChange::Commit {
                candidate, attempt, ..
            } => {
                let (fingerprint, update) = state
                    .vault
                    .linux_save(candidate.credentials.clone(), candidate.id.clone())?;
                let (id, cancel) = attempt.as_ref().map_or_else(
                    || (None, state.account_cancel.child_token()),
                    |attempt| (Some(attempt.id.clone()), attempt.cancel.clone()),
                );
                (update, Some(fingerprint), id, cancel)
            }
            LinuxAccountChange::Confirmation(_) => unreachable!(),
        };
        state.vault_write = Some(LinuxVaultWrite {
            attempt,
            cancel: cancel.clone(),
            authorized: false,
        });
        (change, update, fingerprint, cancel, state.account_epoch)
    };
    let persisted = update
        .commit(&cancel, || {
            let mut state = app.state();
            state.ensure_open()?;
            if state.account_epoch != epoch || cancel.is_cancelled() {
                return Err(account_changed());
            }
            if let LinuxAccountChange::Commit {
                candidate,
                confirmed,
                attempt,
            } = &change
            {
                if attempt
                    .as_ref()
                    .is_some_and(|attempt| Instant::now() >= attempt.expires)
                {
                    return Err(account_changed());
                }
                let required = confirmation(&state, candidate);
                if required.is_some() && required != *confirmed {
                    return Err(account_changed());
                }
            }
            state
                .vault_write
                .as_mut()
                .ok_or_else(account_changed)?
                .authorized = true;
            Ok(())
        })
        .await;
    let mut state = app.state();
    state.vault_write = None;
    cancel.cancel();
    state.ensure_open()?;
    match persisted {
        Ok(update) => state.vault.finish_linux_update(update),
        Err(error) => {
            if matches!(error.code, "vault_changed" | "vault_invalid") {
                state.clear_account();
                state.vault.linux_update_failed(&error);
                account_waiting(&mut state, true);
                app.emit_account(state.account_status());
            }
            return Err(error);
        }
    }
    let account = match change {
        LinuxAccountChange::Reset => finish_vault_reset(&app, &mut state)?,
        LinuxAccountChange::Commit {
            candidate,
            confirmed,
            attempt,
        } => {
            if attempt.is_none() {
                state.validating = false;
            }
            activate_candidate(
                &app,
                &mut state,
                candidate.credentials,
                fingerprint.ok_or_else(account_changed)?,
                confirmed,
            )
        }
        LinuxAccountChange::Confirmation(_) => unreachable!(),
    };
    Ok(ValidationResult {
        account: Some(account),
        confirmation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::{begin_attempt, cancel_attempt, finish_attempt, take_attempt},
        qobuz::Credentials,
    };
    use tokio::time::Duration;

    fn candidate(id: &str) -> ValidatedAccount {
        ValidatedAccount {
            credentials: Credentials {
                token: format!("synthetic-{id}"),
                app_id: "123456789".into(),
                app_secret: "synthetic-secret".into(),
                ..Default::default()
            },
            id: id.to_owned(),
        }
    }

    #[tokio::test]
    async fn signed_out_identity_requires_confirmation_and_cancellation_keeps_it() {
        let profile = tempfile::tempdir().unwrap();
        let app = App::initialize_root(profile.path().into()).unwrap();
        let begin = |id| begin_attempt(&mut app.state(), id, Duration::from_secs(60)).unwrap();

        begin("initial");
        let ready = finish_attempt(app.clone(), "initial".into(), Ok(candidate("42")))
            .await
            .unwrap();
        assert_eq!(ready.account.unwrap().state, "ready");

        let signed_out = linux_account_change(app.clone(), |_| Ok(LinuxAccountChange::Reset))
            .await
            .unwrap();
        assert_eq!(signed_out.account.unwrap().state, "missing");
        assert_eq!(app.state().vault.account_id(), Some("42"));

        begin("cancelled");
        let pending = finish_attempt(app.clone(), "cancelled".into(), Ok(candidate("43")))
            .await
            .unwrap();
        assert!(pending.confirmation == Some(AccountConfirmation::Switch));
        cancel_attempt(&mut app.state(), "cancelled");
        assert_eq!(app.state().vault.account_id(), Some("42"));

        begin("approved");
        let pending = finish_attempt(app.clone(), "approved".into(), Ok(candidate("43")))
            .await
            .unwrap();
        assert!(pending.confirmation == Some(AccountConfirmation::Switch));
        linux_account_change(app.clone(), |state| {
            let mut attempt = take_attempt(state, "approved")?;
            let (candidate, confirmation) = attempt.pending.take().unwrap();
            Ok(LinuxAccountChange::Commit {
                candidate,
                confirmed: Some(confirmation),
                attempt: Some(attempt),
            })
        })
        .await
        .unwrap();
        assert_eq!(app.state().vault.account_id(), Some("43"));
        assert_eq!(app.state().account_status().state, "ready");
    }
}
