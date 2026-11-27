#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(all(test, windows))]
mod tests;

use crate::{
    app::{App, AppState},
    error::{AppError, Result},
    qobuz::{Credentials, EmailSignInPhase},
    vault::{AccountDto, CredentialInput},
};
#[cfg(target_os = "macos")]
use crate::vault::PreparedToken;
#[cfg(target_os = "linux")]
use linux::{LinuxAccountChange, linux_account_change, linux_vault_busy};
use serde::Serialize;
use std::sync::Arc;
use tauri::{WebviewWindow, ipc::Channel};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[cfg(not(target_os = "macos"))]
type PreparedToken = (Credentials, bool);

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AccountConfirmation {
    Switch,
    Active,
}

pub(crate) struct ValidatedAccount {
    pub(crate) credentials: Credentials,
    pub(crate) id: String,
    #[cfg(target_os = "macos")]
    pub(crate) remember_pair: bool,
}

pub(crate) struct AccountAttempt {
    pub(crate) id: String,
    pub(crate) cancel: CancellationToken,
    pub(crate) expires: Instant,
    pub(crate) pending: Option<(ValidatedAccount, AccountConfirmation)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub(crate) account: Option<AccountDto>,
    pub(crate) confirmation: Option<AccountConfirmation>,
}

enum FinishedAttempt {
    Confirm(ValidationResult),
    Commit(ValidatedAccount, AccountAttempt),
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::task)?
}

pub(crate) async fn reset_vault(app: Arc<App>) -> Result<AccountDto> {
    #[cfg(target_os = "linux")]
    {
        return linux_account_change(app, |_| Ok(LinuxAccountChange::Reset))
            .await?
            .account
            .ok_or_else(account_changed);
    }
    #[cfg(not(target_os = "linux"))]
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        state.vault.reset()?;
        finish_vault_reset(&app, &mut state)
    })
    .await
}

pub(crate) fn finish_vault_reset(app: &App, state: &mut AppState) -> Result<AccountDto> {
    state.clear_account();
    account_waiting(state, true);
    for bookmark in state.bookmarks.values_mut() {
        bookmark.fingerprint = None;
    }
    let status = state.account_status();
    app.emit_account(status.clone());
    app.storage.save_bookmarks(state.bookmarks.values())?;
    Ok(status)
}

pub(crate) fn confirmation(
    state: &AppState,
    candidate: &ValidatedAccount,
) -> Option<AccountConfirmation> {
    let previous = state.vault.account_id();
    if previous.is_some_and(|id| id != candidate.id)
        || (previous.is_none()
            && state.vault.has_saved_credentials()
            && !state.queue.jobs.is_empty())
    {
        Some(AccountConfirmation::Switch)
    } else if state
        .account
        .as_ref()
        .is_some_and(|account| account != &candidate.credentials)
        && state.queue.active()
    {
        Some(AccountConfirmation::Active)
    } else {
        None
    }
}

pub(crate) async fn validate_credentials(
    app: Arc<App>,
    attempt: String,
    input: Option<CredentialInput>,
) -> Result<ValidationResult> {
    let (input, cancel) = {
        let mut state = app.state();
        state.ensure_open()?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        state.vault.ensure_loaded()?;
        let input = if let Some(input) = input {
            state.vault.prepare_token(input)?
        } else {
            saved_token(&state)?
        };
        let cancel = begin_attempt(&mut state, &attempt, Duration::from_secs(60))?;
        (input, cancel)
    };
    let result = validate_candidate(&app, &input, &cancel).await;
    finish_attempt(app, attempt, result).await
}

pub(crate) async fn start_email_sign_in(
    window: WebviewWindow,
    app: Arc<App>,
    attempt: String,
    phase: Channel<EmailSignInPhase>,
) -> Result<ValidationResult> {
    let cancel = begin_attempt(&mut app.state(), &attempt, Duration::from_secs(420))?;
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(account_changed()),
        result = app.qobuz.sign_in_with_email(window, phase) => result
            .map(|(credentials, id)| ValidatedAccount {
                credentials,
                id,
                #[cfg(target_os = "macos")]
                remember_pair: false,
            })
            .map_err(|error| AppError::qobuz(error, "account")),
    };
    finish_attempt(app, attempt, result).await
}

pub(crate) fn begin_attempt(
    state: &mut AppState,
    attempt: &str,
    lifetime: Duration,
) -> Result<CancellationToken> {
    state.ensure_open()?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    state.vault.ensure_loaded()?;
    #[cfg(target_os = "linux")]
    if state.vault_write.is_some() {
        return Err(linux_vault_busy());
    }
    if state.validating || state.account_attempt.is_some() {
        return Err(AppError::invalid(
            "account",
            "A Qobuz validation is already in progress.",
        ));
    }
    if attempt.is_empty() || attempt.len() > 64 {
        return Err(AppError::invalid(
            "account",
            "The Qobuz sign-in attempt is invalid.",
        ));
    }
    let cancel = state.account_cancel.child_token();
    state.account_attempt = Some(AccountAttempt {
        id: attempt.to_owned(),
        cancel: cancel.clone(),
        expires: Instant::now() + lifetime,
        pending: None,
    });
    Ok(cancel)
}

async fn finish_attempt(
    app: Arc<App>,
    attempt: String,
    result: Result<ValidatedAccount>,
) -> Result<ValidationResult> {
    #[cfg(target_os = "linux")]
    {
        let owner = app.clone();
        return linux_account_change(app, move |state| {
            Ok(
                match finish_candidate_attempt(&owner, state, &attempt, result)? {
                    FinishedAttempt::Confirm(result) => LinuxAccountChange::Confirmation(result),
                    FinishedAttempt::Commit(candidate, current) => LinuxAccountChange::Commit {
                        candidate,
                        confirmed: None,
                        attempt: Some(current),
                    },
                },
            )
        })
        .await;
    }
    #[cfg(not(target_os = "linux"))]
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        match finish_candidate_attempt(&app, &mut state, &attempt, result)? {
            FinishedAttempt::Confirm(result) => Ok(result),
            FinishedAttempt::Commit(candidate, current) => {
                drop(current);
                let account = commit_candidate(&app, &mut state, candidate, None)?;
                Ok(ValidationResult {
                    account: Some(account),
                    confirmation: None,
                })
            }
        }
    })
    .await
}

fn finish_candidate_attempt(
    app: &Arc<App>,
    state: &mut AppState,
    attempt: &str,
    result: Result<ValidatedAccount>,
) -> Result<FinishedAttempt> {
    let mut current = take_attempt(state, attempt)?;
    let candidate = result?;
    let Some(required) = confirmation(state, &candidate) else {
        return Ok(FinishedAttempt::Commit(candidate, current));
    };
    let expires = Instant::now() + Duration::from_secs(120);
    current.expires = expires;
    current.pending = Some((candidate, required));
    let cancel = current.cancel.clone();
    state.account_attempt = Some(current);
    let cleanup = app.clone();
    let attempt = attempt.to_owned();
    app.tasks.spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => (),
            _ = tokio::time::sleep_until(expires) => {
                let mut state = cleanup.state();
                if state.account_attempt.as_ref().is_some_and(|current| current.id == attempt) {
                    state.account_attempt = None;
                }
            }
        }
    });
    Ok(FinishedAttempt::Confirm(ValidationResult {
        account: None,
        confirmation: Some(required),
    }))
}

pub(crate) async fn confirm_validation(app: Arc<App>, attempt: String) -> Result<AccountDto> {
    #[cfg(target_os = "linux")]
    return linux_account_change(app, move |state| {
        let mut current = take_attempt(state, &attempt)?;
        let (candidate, approved) = current.pending.take().ok_or_else(account_changed)?;
        Ok(LinuxAccountChange::Commit {
            candidate,
            confirmed: Some(approved),
            attempt: Some(current),
        })
    })
    .await?
    .account
    .ok_or_else(account_changed);
    #[cfg(not(target_os = "linux"))]
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        let current = take_attempt(&mut state, &attempt)?;
        let (candidate, approved) = current.pending.ok_or_else(account_changed)?;
        current.cancel.cancel();
        commit_candidate(&app, &mut state, candidate, Some(approved))
    })
    .await
}

pub(crate) fn cancel_attempt(state: &mut AppState, id: &str) {
    #[cfg(target_os = "linux")]
    if let Some(write) = &state.vault_write
        && !write.authorized
        && write.attempt.as_deref() == Some(id)
    {
        write.cancel.cancel();
    }
    if state
        .account_attempt
        .as_ref()
        .is_some_and(|current| current.id == id)
        && let Some(current) = state.account_attempt.take()
    {
        current.cancel.cancel();
    }
}

pub(crate) fn take_attempt(state: &mut AppState, id: &str) -> Result<AccountAttempt> {
    if state
        .account_attempt
        .as_ref()
        .is_none_or(|current| current.id != id)
    {
        return Err(account_changed());
    }
    let attempt = state.account_attempt.take().ok_or_else(account_changed)?;
    if attempt.cancel.is_cancelled() || Instant::now() >= attempt.expires {
        attempt.cancel.cancel();
        return Err(account_changed());
    }
    Ok(attempt)
}

fn saved_token(state: &AppState) -> Result<PreparedToken> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    state.vault.ensure_loaded()?;
    let credentials = state.vault.credentials().cloned().ok_or_else(|| {
        state
            .vault
            .load_error()
            .cloned()
            .unwrap_or_else(|| AppError::invalid("account", "Enter your Qobuz token."))
    })?;
    #[cfg(not(target_os = "macos"))]
    let input = (credentials, false);
    #[cfg(target_os = "macos")]
    let input = PreparedToken {
        credentials,
        ..PreparedToken::default()
    };
    Ok(input)
}

async fn validate_candidate(
    app: &App,
    input: &PreparedToken,
    cancel: &CancellationToken,
) -> Result<ValidatedAccount> {
    #[cfg(not(target_os = "macos"))]
    let (credentials, retry_reused_pair) = input;
    #[cfg(target_os = "macos")]
    let credentials = &input.credentials;
    app.qobuz.retry_account_connection(credentials);
    #[cfg(not(target_os = "macos"))]
    let validation = app
        .qobuz
        .validate_account_with_reuse(credentials, *retry_reused_pair);
    #[cfg(target_os = "macos")]
    let validation = app.qobuz.validate_account(
        credentials,
        input.retry_reused_pair,
        input.remembered_fallback.as_ref(),
    );
    tokio::pin!(validation);
    let mut recovery = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(account_changed()),
            result = &mut validation => return result
                .map(|(credentials, id)| ValidatedAccount {
                    credentials,
                    id,
                    #[cfg(target_os = "macos")]
                    remember_pair: input.remember_pair,
                })
                .map_err(|error| AppError::qobuz(error, "account")),
            _ = recovery.tick() => {
                if app.qobuz.recovery_status(Some(credentials)).retry_connection {
                    return Err(AppError::new(
                        "connection_retry_required",
                        "account",
                        "Qobuz is still unavailable. Choose Retry validation to check the connection again.",
                        true,
                    ));
                }
            }
        }
    }
}

pub async fn validate_saved_account(app: Arc<App>) -> Result<AccountDto> {
    let (input, epoch, cancel) = {
        let mut state = app.state();
        state.ensure_open()?;
        if state.validating || state.account_attempt.is_some() || state.account.is_some() {
            return Ok(state.account_status());
        }
        #[cfg(target_os = "linux")]
        if state.vault_write.is_some() {
            return Ok(state.account_status());
        }
        let input = saved_token(&state)?;
        state.validating = true;
        state.account_error = None;
        account_waiting(&mut state, false);
        app.emit_account(state.account_status());
        (input, state.account_epoch, state.account_cancel.clone())
    };
    let result = validate_candidate(&app, &input, &cancel).await;
    #[cfg(target_os = "linux")]
    {
        let status = linux_account_change(app.clone(), |state| {
            if state.account_epoch != epoch || !state.validating || cancel.is_cancelled() {
                return Err(account_changed());
            }
            let candidate = check_saved_identity(state, result?)?;
            Ok(LinuxAccountChange::Commit {
                candidate,
                confirmed: None,
                attempt: None,
            })
        })
        .await;
        match status {
            Ok(result) => Ok(result.account.unwrap_or_else(|| app.state().account_status())),
            Err(error) => {
                let mut state = app.state();
                if state.account_epoch == epoch && !state.closing {
                    state.validating = false;
                    state.account_error = Some(error.clone());
                    account_waiting(&mut state, false);
                    app.emit_account(state.account_status());
                }
                Err(error)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if state.account_epoch != epoch {
            return Err(account_changed());
        }
        state.validating = false;
        // The saved token itself establishes continuity for legacy vaults. A
        // protected prior ID that disagrees requires explicit manual review.
        let result = result.and_then(|candidate| {
            let candidate = check_saved_identity(&state, candidate)?;
            commit_candidate(&app, &mut state, candidate, None)
        });
        if let Err(error) = &result {
            state.account_error = Some(error.clone());
            account_waiting(&mut state, false);
            app.emit_account(state.account_status());
        }
        result
    })
    .await
}

fn check_saved_identity(
    state: &AppState,
    candidate: ValidatedAccount,
) -> Result<ValidatedAccount> {
    if state
        .vault
        .account_id()
        .is_some_and(|id| id != candidate.id)
    {
        return Err(AppError::new(
            "account_confirmation",
            "account",
            "The saved credentials now identify a different Qobuz account. Retry validation to confirm the switch.",
            false,
        ));
    }
    Ok(candidate)
}

#[cfg(not(target_os = "linux"))]
fn commit_candidate(
    app: &Arc<App>,
    state: &mut AppState,
    candidate: ValidatedAccount,
    confirmed: Option<AccountConfirmation>,
) -> Result<AccountDto> {
    let fingerprint = state.vault.save(
        candidate.credentials.clone(),
        candidate.id,
        #[cfg(target_os = "macos")]
        candidate.remember_pair,
    )?;
    Ok(activate_candidate(
        app,
        state,
        candidate.credentials,
        fingerprint,
        confirmed,
    ))
}

pub(crate) fn activate_candidate(
    app: &Arc<App>,
    state: &mut AppState,
    credentials: Credentials,
    fingerprint: String,
    confirmed: Option<AccountConfirmation>,
) -> AccountDto {
    let replaces_live = state
        .account
        .as_ref()
        .is_some_and(|account| account != &credentials);
    if confirmed == Some(AccountConfirmation::Switch) {
        state.queue.shutdown();
        state.queue.reset_idle_pause(app.qobuz.recovery());
    }
    if replaces_live || confirmed == Some(AccountConfirmation::Switch) {
        state.clear_account();
    }
    state.account = Some(credentials);
    state.fingerprint = Some(fingerprint);
    state.account_error = None;
    if replaces_live || confirmed == Some(AccountConfirmation::Switch) || !state.queue.active() {
        account_waiting(state, replaces_live);
    }
    let mut pending = std::mem::take(&mut state.bookmark_refresh);
    pending.extend(
        state
            .bookmarks
            .values()
            .filter(|bookmark| {
                bookmark.fingerprint.is_none()
                    && bookmark.dto.album_name.is_none()
                    && bookmark.dto.reason.as_deref().is_some_and(|reason| {
                        [
                            "Set up a Qobuz account",
                            "Validate the Qobuz account",
                            "Unlock the vault",
                            "Validating saved Qobuz",
                            "Retry Qobuz account",
                            "Waiting to start",
                            "Waiting for the account change",
                        ]
                        .iter()
                        .any(|prefix| reason.starts_with(prefix))
                    })
            })
            .map(|bookmark| bookmark.dto.id),
    );
    for id in pending {
        app.resolve_bookmark(state, id);
    }
    let status = state.account_status();
    app.emit_account(status.clone());
    app.wake_scheduler();
    status
}

pub(crate) fn account_waiting(state: &mut AppState, cancel_active: bool) {
    let reason = state.waiting_reason();
    state.queue.account_changed(reason, cancel_active);
}

pub(crate) fn account_changed() -> AppError {
    AppError::new(
        "account_changed",
        "account",
        "The account changed during validation. Validate the current credentials.",
        true,
    )
}
