use super::Managed;
use crate::{
    account::{self, ValidationResult},
    error::{AppError, Result},
    qobuz::EmailSignInPhase,
    vault::{AccountDto, CredentialInput},
};
use tauri::ipc::Channel;

#[tauri::command]
pub async fn reset_vault(app: Managed<'_>, confirmation: bool) -> Result<AccountDto> {
    if !confirmation {
        return Err(AppError::invalid(
            "vault",
            "Confirm removing the saved Qobuz credentials.",
        ));
    }
    account::reset_vault(app.inner().clone()).await
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub async fn retry_vault_access(
    window: tauri::WebviewWindow,
    app: Managed<'_>,
) -> Result<AccountDto> {
    if window.label() != "main" {
        return Err(AppError::invalid(
            "account",
            "Retry keyring access from the main window.",
        ));
    }
    app.restore_linux_vault(true).await?;
    Ok(app.state().account_status())
}

#[tauri::command]
pub async fn validate_credentials(
    app: Managed<'_>,
    attempt: String,
    input: Option<CredentialInput>,
) -> Result<ValidationResult> {
    account::validate_credentials(app.inner().clone(), attempt, input).await
}

#[tauri::command]
pub async fn start_email_sign_in(
    window: tauri::WebviewWindow,
    app: Managed<'_>,
    attempt: String,
    phase: Channel<EmailSignInPhase>,
) -> Result<ValidationResult> {
    if window.label() != "main" {
        return Err(AppError::invalid(
            "account",
            "Start Qobuz sign-in from the main window.",
        ));
    }
    account::start_email_sign_in(window, app.inner().clone(), attempt, phase).await
}

#[tauri::command]
pub async fn confirm_account_validation(app: Managed<'_>, attempt: String) -> Result<AccountDto> {
    account::confirm_validation(app.inner().clone(), attempt).await
}

#[tauri::command]
pub fn cancel_account_validation(app: Managed<'_>, attempt: String) {
    account::cancel_attempt(&mut app.state(), &attempt);
}

#[tauri::command]
pub fn retry_connection(app: Managed<'_>) -> Result<()> {
    app.state().ensure_open()?;
    app.qobuz.retry_connection();
    Ok(())
}
