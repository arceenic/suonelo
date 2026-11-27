#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(test, not(target_os = "linux")))]
mod tests;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod windows;

use crate::error::{AppError, Result};
#[cfg(target_os = "macos")]
use crate::qobuz::AppPair;
use crate::qobuz::{AccessMode, Credentials, TokenTransport, valid_app_id};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

const REUSE_WINDOW_MS: u64 = 12 * 60 * 60 * 1000;
const MAX_RECORD: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[cfg_attr(target_os = "linux", derive(Clone, PartialEq, Eq))]
#[cfg_attr(target_os = "linux", serde(deny_unknown_fields))]
struct SecretRecord {
    fingerprint_key: [u8; 32],
    credentials: Credentials,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "read_token_timestamp"
    )]
    token_committed_at_ms: Option<u64>,
    #[cfg(target_os = "macos")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remembered_app_pair: Option<AppPair>,
}

fn read_token_timestamp<'de, D>(value: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[cfg(target_os = "linux")]
    return match Option::<u64>::deserialize(value)? {
        Some(0) => Err(serde::de::Error::custom(
            "invalid credential commit timestamp",
        )),
        timestamp => Ok(timestamp),
    };
    #[cfg(not(target_os = "linux"))]
    Ok(serde_json::Value::deserialize(value)?
        .as_u64()
        .filter(|time| *time > 0))
}

impl Drop for SecretRecord {
    fn drop(&mut self) {
        self.fingerprint_key.zeroize();
    }
}

pub struct Vault {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    path: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    linux_profile: Option<String>,
    #[cfg(target_os = "linux")]
    linux_loading: bool,
    #[cfg(target_os = "macos")]
    keychain_account: String,
    #[cfg(target_os = "macos")]
    keychain_loading: bool,
    record: Option<SecretRecord>,
    load_error: Option<AppError>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub state: &'static str,
    pub epoch: u64,
    pub has_token: bool,
    pub has_saved_credentials: bool,
    pub has_app_id: bool,
    pub has_secret: bool,
    #[cfg(target_os = "macos")]
    pub has_remembered_app_pair: bool,
    #[cfg(target_os = "macos")]
    pub can_remember_saved_app_pair: bool,
    pub access_mode: AccessMode,
    pub reason: Option<String>,
    pub reason_code: Option<&'static str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInput {
    pub token: String,
    pub app_id: Option<String>,
    #[serde(alias = "appSecret")]
    pub secret: Option<String>,
    #[serde(default)]
    pub force_discovery: bool,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub remember_saved_app_pair: bool,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct PreparedToken {
    pub credentials: Credentials,
    pub retry_reused_pair: bool,
    pub remembered_fallback: Option<AppPair>,
    pub remember_pair: bool,
}

impl CredentialInput {
    fn prepare(
        &self,
        saved: Option<&Credentials>,
        committed_at_ms: Option<u64>,
        now_ms: Option<u64>,
    ) -> Result<(Credentials, bool)> {
        let token = self.token.trim();
        let app_id = self.app_id.as_deref().unwrap_or_default().trim();
        let secret = self.secret.as_deref().unwrap_or_default().trim();
        let automatic = app_id.is_empty() && secret.is_empty();
        #[cfg(target_os = "macos")]
        let automatic = self.force_discovery || automatic;
        if !self.force_discovery && app_id.is_empty() != secret.is_empty() {
            return Err(AppError::new(
                "incomplete_app_pair",
                "account",
                "Enter both app fields, or clear both for automatic setup.",
                false,
            ));
        }
        if let Some(saved) = saved.filter(|saved| saved.transport == TokenTransport::HeaderOnly)
            && (token.is_empty() || token == stored_token(saved))
        {
            if automatic && !self.force_discovery {
                return Ok((saved.clone(), false));
            }
            return Err(AppError::invalid(
                "account",
                "Sign in with email again or enter a different token.",
            ));
        }
        let token = if token.is_empty() && automatic {
            saved.map(stored_token).unwrap_or_default()
        } else {
            token
        };
        if token.is_empty() {
            return Err(AppError::invalid("account", "Enter your Qobuz token."));
        }
        #[cfg(target_os = "macos")]
        if !crate::qobuz::valid_token(token) {
            return Err(AppError::new(
                "invalid_token",
                "account",
                "Enter a valid Qobuz account token.",
                false,
            ));
        }
        let mut input = Credentials::default();
        input.token = token.to_owned();
        if self.force_discovery {
            return Ok((input, false));
        }
        if !automatic {
            #[cfg(target_os = "macos")]
            if !valid_app_id(app_id) {
                return Err(AppError::new(
                    "invalid_app_id",
                    "account",
                    "Enter a nine-digit Qobuz App ID.",
                    false,
                ));
            }
            input.app_id = app_id.to_owned();
            input.app_secret = secret.to_owned();
            return Ok((input, false));
        }
        let complete_pair = |saved: &&Credentials| {
            #[cfg(target_os = "macos")]
            {
                saved.has_token_app_pair()
            }
            #[cfg(not(target_os = "macos"))]
            {
                saved.transport == TokenTransport::Query
                    && valid_app_id(&saved.app_id)
                    && !saved.app_secret.trim().is_empty()
            }
        };
        if let Some(saved) = saved.filter(complete_pair) {
            let replacement = stored_token(saved) != token;
            let age = now_ms
                .zip(committed_at_ms.filter(|committed| *committed > 0))
                .and_then(|(now, committed)| now.checked_sub(committed));
            if !replacement || age.is_some_and(|age| age <= REUSE_WINDOW_MS) {
                #[cfg(target_os = "macos")]
                {
                    input.app_id = saved.app_id.trim().to_owned();
                    input.app_secret = saved.app_secret.trim().to_owned();
                    input.app_pair_discovered = saved.app_pair_discovered;
                }
                #[cfg(not(target_os = "macos"))]
                {
                    input.app_id.clone_from(&saved.app_id);
                    input.app_secret.clone_from(&saved.app_secret);
                }
                return Ok((input, replacement));
            }
        }
        Ok((input, false))
    }
}

fn stored_token(credentials: &Credentials) -> &str {
    #[cfg(target_os = "macos")]
    {
        credentials.token.trim()
    }
    #[cfg(not(target_os = "macos"))]
    {
        &credentials.token
    }
}

fn unix_time_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

impl Drop for CredentialInput {
    fn drop(&mut self) {
        self.token.zeroize();
        self.app_id.zeroize();
        self.secret.zeroize();
    }
}

impl Vault {
    #[cfg(not(target_os = "macos"))]
    pub fn prepare_token(&self, input: CredentialInput) -> Result<(Credentials, bool)> {
        input.prepare(
            self.credentials(),
            self.record
                .as_ref()
                .and_then(|record| record.token_committed_at_ms),
            unix_time_ms(),
        )
    }

    pub fn load_error(&self) -> Option<&AppError> {
        self.load_error.as_ref()
    }

    pub fn credentials(&self) -> Option<&Credentials> {
        self.record
            .as_ref()
            .map(|record| &record.credentials)
            .filter(|credentials| !credentials.token.is_empty())
    }

    pub fn account_id(&self) -> Option<&str> {
        self.record.as_ref()?.account_id.as_deref()
    }

    pub fn has_saved_credentials(&self) -> bool {
        self.credentials().is_some() || self.load_error.is_some()
    }

    pub fn replacement_reason(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.keychain_loading {
            return Some(macos::keychain_loading_error().message);
        }
        #[cfg(target_os = "linux")]
        if self.linux_loading {
            return Some(linux::loading_error().message);
        }
        self.load_error.as_ref().map(|error| error.message.clone())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn reset(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        self.ensure_keychain_access()?;
        if let Some(signed_out) = self.signed_out_record() {
            self.persist(&signed_out)?;
            self.record = Some(signed_out);
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        windows::remove_record_file(&self.path)?;
        #[cfg(target_os = "macos")]
        macos::keychain_remove(&self.keychain_account)?;
        self.record = None;
        self.load_error = None;
        Ok(())
    }

    fn signed_out_record(&self) -> Option<SecretRecord> {
        let record = self.record.as_ref()?;
        let account_id = record.account_id.clone()?;
        // Retain the verified identity so signing out cannot bypass the
        // account switch warning for queued work.
        Some(SecretRecord {
            fingerprint_key: record.fingerprint_key,
            credentials: Credentials::default(),
            account_id: Some(account_id),
            token_committed_at_ms: None,
            #[cfg(target_os = "macos")]
            remembered_app_pair: None,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn save_with(
        &mut self,
        credentials: Credentials,
        account_id: String,
        #[cfg(target_os = "macos")] remember_pair: bool,
        now_ms: Option<u64>,
        persist: impl FnOnce(&Self, &SecretRecord) -> Result<()>,
    ) -> Result<String> {
        let (result, record) = self.prepare_save(
            credentials,
            account_id,
            #[cfg(target_os = "macos")]
            remember_pair,
            now_ms,
        )?;
        if let Some(record) = record {
            persist(self, &record)?;
            self.record = Some(record);
            self.load_error = None;
        }
        Ok(result)
    }

    fn prepare_save(
        &self,
        mut credentials: Credentials,
        account_id: String,
        #[cfg(target_os = "macos")] remember_pair: bool,
        now_ms: Option<u64>,
    ) -> Result<(String, Option<SecretRecord>)> {
        credentials.access_mode = AccessMode::Unknown;
        #[cfg(target_os = "macos")]
        self.ensure_keychain_access()?;
        #[cfg(target_os = "macos")]
        let remembered_app_pair = if remember_pair {
            Some(AppPair::from_token_credentials(&credentials).ok_or_else(macos::vault_io)?)
        } else {
            self.remembered_app_pair().cloned()
        };
        if let Some(record) = &self.record
            && record.credentials == credentials
            && record.account_id.as_deref() == Some(account_id.as_str())
        {
            #[cfg(not(target_os = "macos"))]
            return Ok((fingerprint(record), None));
            #[cfg(target_os = "macos")]
            if record.remembered_app_pair == remembered_app_pair {
                return Ok((fingerprint(record), None));
            }
        }
        let mut fingerprint_key = [0; 32];
        if let Some(record) = &self.record {
            fingerprint_key.copy_from_slice(&record.fingerprint_key);
        } else {
            OsRng
                .try_fill_bytes(&mut fingerprint_key)
                .map_err(|_| vault_io())?;
        }
        let token_committed_at_ms = if credentials.transport == TokenTransport::Query {
            match self.record.as_ref() {
                Some(record)
                    if record.credentials.transport == TokenTransport::Query
                        && stored_token(&record.credentials) == credentials.token =>
                {
                    record.token_committed_at_ms
                }
                _ => now_ms,
            }
        } else {
            None
        };
        let record = SecretRecord {
            fingerprint_key,
            credentials,
            account_id: Some(account_id),
            token_committed_at_ms,
            #[cfg(target_os = "macos")]
            remembered_app_pair,
        };
        Ok((fingerprint(&record), Some(record)))
    }

    pub fn clear_memory(&mut self) {
        self.record = None;
    }

    #[cfg(not(target_os = "linux"))]
    fn persist(&self, record: &SecretRecord) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        return windows::persist(&self.path, record);
        #[cfg(target_os = "macos")]
        return macos::keychain_write(&self.keychain_account, record);
    }
}

fn fingerprint(record: &SecretRecord) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&record.fingerprint_key).unwrap();
    for value in [
        &record.credentials.token,
        &record.credentials.app_id,
        &record.credentials.app_secret,
    ] {
        mac.update(&(value.len() as u64).to_be_bytes());
        mac.update(value.as_bytes());
    }
    hex::encode(mac.finalize().into_bytes())
}

fn verified_identity(record: &SecretRecord) -> bool {
    record.account_id.as_deref().is_some_and(|id| {
        id.parse::<u64>()
            .is_ok_and(|number| number > 0 && number.to_string() == id)
    })
}

fn vault_io() -> AppError {
    #[cfg(target_os = "macos")]
    return macos::vault_io();
    #[cfg(target_os = "linux")]
    return linux::vault_io();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return windows::vault_io();
}
