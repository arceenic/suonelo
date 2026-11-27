use super::{MAX_RECORD, PreparedToken, SecretRecord, Vault, unix_time_ms, verified_identity};
use crate::{
    error::{AppError, Result},
    qobuz::{AppPair, Credentials, TokenTransport, valid_app_id, valid_token},
};
use zeroize::Zeroizing;

const KEYCHAIN_SERVICE: &str = "com.suonelo.desktop.qobuz.v1";
pub(super) const KEYCHAIN_RECORD_MAGIC: &[u8] = b"SUONELO-KEYCHAIN-V1\0";
const KEYCHAIN_ITEM_NOT_FOUND: i32 = -25300;

impl Vault {
    pub fn keychain(qa_name: Option<&str>) -> Self {
        let keychain_account =
            qa_name.map_or_else(|| "default".to_owned(), |name| format!("qa:{name}"));
        Self {
            keychain_account,
            keychain_loading: true,
            record: None,
            load_error: None,
        }
    }

    pub fn pending_keychain_account(&self) -> Option<String> {
        self.keychain_loading.then(|| self.keychain_account.clone())
    }

    pub fn read_keychain(keychain_account: String) -> Self {
        let (record, load_error) = match keychain_read(&keychain_account) {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            keychain_account,
            keychain_loading: false,
            record,
            load_error,
        }
    }

    pub fn finish_keychain_load(
        &mut self,
        loaded: std::result::Result<Self, tokio::task::JoinError>,
    ) {
        match loaded {
            Ok(vault) => *self = vault,
            Err(_) => {
                self.keychain_loading = false;
                self.load_error = Some(vault_io());
            }
        }
    }

    pub fn is_loading(&self) -> bool {
        self.keychain_loading
    }

    pub fn ensure_loaded(&self) -> Result<()> {
        if self.keychain_loading {
            return Err(keychain_loading_error());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn new(path: std::path::PathBuf) -> Self {
        use sha2::Digest;

        // Portable fixtures start empty and never read a Windows credential file.
        Self {
            keychain_account: format!(
                "test:{}",
                hex::encode(sha2::Sha256::digest(path.as_os_str().as_encoded_bytes()))
            ),
            keychain_loading: false,
            record: None,
            load_error: None,
        }
    }

    pub fn prepare_token(&self, input: super::CredentialInput) -> Result<PreparedToken> {
        self.prepare_token_at(input, unix_time_ms())
    }

    pub(super) fn prepare_token_at(
        &self,
        input: super::CredentialInput,
        now_ms: Option<u64>,
    ) -> Result<PreparedToken> {
        let saved = self.credentials();
        let has_manual_input = [&input.app_id, &input.secret]
            .iter()
            .any(|value| value.as_deref().is_some_and(|text| !text.trim().is_empty()));
        if input.remember_saved_app_pair {
            if input.force_discovery || has_manual_input || !input.token.trim().is_empty() {
                return Err(AppError::invalid(
                    "account",
                    "Remember the saved pair separately from entering new credentials.",
                ));
            }
            let credentials = saved
                .filter(|credentials| credentials.has_token_app_pair())
                .cloned()
                .ok_or_else(|| {
                    AppError::invalid("account", "There is no saved token app pair to remember.")
                })?;
            return Ok(PreparedToken {
                credentials,
                remember_pair: true,
                ..PreparedToken::default()
            });
        }

        let (credentials, retry_reused_pair) = input.prepare(
            saved,
            self.record
                .as_ref()
                .and_then(|record| record.token_committed_at_ms),
            now_ms,
        )?;
        let remember_pair = has_manual_input && !input.force_discovery;
        let may_discover = credentials.transport == TokenTransport::Query
            && (credentials.app_id.is_empty() || retry_reused_pair);
        let remembered_fallback = if may_discover {
            self.remembered_app_pair().cloned()
        } else {
            None
        };
        Ok(PreparedToken {
            credentials,
            retry_reused_pair,
            remembered_fallback,
            remember_pair,
        })
    }

    pub(super) fn remembered_app_pair(&self) -> Option<&AppPair> {
        self.record
            .as_ref()?
            .remembered_app_pair
            .as_ref()
            .filter(|pair| pair.is_complete())
    }

    pub fn has_remembered_app_pair(&self) -> bool {
        self.remembered_app_pair().is_some()
    }

    pub fn can_remember_saved_app_pair(&self) -> bool {
        self.credentials().is_some_and(|credentials| {
            credentials.has_token_app_pair()
                && self
                    .remembered_app_pair()
                    .is_none_or(|pair| !pair.matches(credentials))
        })
    }

    pub fn save(
        &mut self,
        credentials: Credentials,
        account_id: String,
        remember_pair: bool,
    ) -> Result<String> {
        self.save_with(
            credentials,
            account_id,
            remember_pair,
            unix_time_ms(),
            Self::persist,
        )
    }

    pub(super) fn ensure_keychain_access(&self) -> Result<()> {
        self.ensure_loaded()?;
        // An unavailable item may still hold a verified identity. Read it on a
        // fresh launch before replacing it or clearing the account.
        match &self.load_error {
            Some(error) if error.code != "vault_invalid" => Err(error.clone()),
            _ => Ok(()),
        }
    }
}

pub(super) fn keychain_loading_error() -> AppError {
    AppError::new(
        "vault_loading",
        "account",
        "Waiting for macOS Keychain to open your saved credentials.",
        true,
    )
}

pub(super) fn vault_io() -> AppError {
    AppError::new(
        "vault_storage",
        "account",
        "macOS Keychain could not read or save your Qobuz credentials. Reopen suonelo after checking Keychain access.",
        true,
    )
}

fn keychain_read(account: &str) -> Result<Option<SecretRecord>> {
    use security_framework::passwords::{PasswordOptions, generic_password};

    match generic_password(PasswordOptions::new_generic_password(
        KEYCHAIN_SERVICE,
        account,
    )) {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            decode_keychain_record(&bytes).map(Some).ok_or_else(|| {
                AppError::new(
                    "vault_invalid",
                    "account",
                    "The saved Qobuz credentials in Keychain could not be read. Sign in to replace them.",
                    true,
                )
            })
        }
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error(error.code())),
    }
}

pub(super) fn decode_keychain_record(bytes: &[u8]) -> Option<SecretRecord> {
    if bytes.len() as u64 > MAX_RECORD {
        return None;
    }
    let payload = bytes.strip_prefix(KEYCHAIN_RECORD_MAGIC)?;
    let record: SecretRecord = serde_json::from_slice(payload).ok()?;
    valid_keychain_record(&record).then_some(record)
}

fn valid_keychain_record(record: &SecretRecord) -> bool {
    let credentials = &record.credentials;
    verified_identity(record)
        && (credentials == &Credentials::default()
            || (valid_token(&credentials.token)
                && (credentials.transport != TokenTransport::HeaderOnly
                    || reqwest::header::HeaderValue::from_str(&credentials.token).is_ok())
                && valid_app_id(&credentials.app_id)
                && !credentials.app_secret.is_empty()))
}

pub(super) fn encode_keychain_record(record: &SecretRecord) -> Result<Zeroizing<Vec<u8>>> {
    if !valid_keychain_record(record) {
        return Err(vault_io());
    }
    let mut bytes = Zeroizing::new(KEYCHAIN_RECORD_MAGIC.to_vec());
    serde_json::to_writer(&mut *bytes, record).map_err(|_| vault_io())?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(vault_io());
    }
    Ok(bytes)
}

pub(super) fn keychain_write(account: &str, record: &SecretRecord) -> Result<()> {
    let bytes = encode_keychain_record(record)?;
    // Security.framework adds the generic password or atomically updates its data
    // on a duplicate item. It never deletes the previous item before writing.
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, &bytes)
        .map_err(|error| match error.code() {
            KEYCHAIN_ITEM_NOT_FOUND => keychain_changed(),
            status => keychain_error(status),
        })
}

pub(super) fn keychain_remove(account: &str) -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(keychain_error(error.code())),
    }
}

pub(super) fn keychain_error(status: i32) -> AppError {
    // OSStatus values from Security/SecBase.h.
    match status {
        -128 | -25291 | -25293 | -25308 | -25315 => AppError::new(
            "vault_access",
            "account",
            "Keychain access is unavailable. Unlock Keychain, allow access, and reopen suonelo.",
            true,
        ),
        -25299 => keychain_changed(),
        _ => vault_io(),
    }
}

fn keychain_changed() -> AppError {
    AppError::new(
        "vault_changed",
        "account",
        "The Keychain item changed while saving. Reopen suonelo and sign in again.",
        true,
    )
}
