use super::{MAX_RECORD, SecretRecord, Vault, unix_time_ms, verified_identity};
use crate::{
    error::{AppError, Result},
    qobuz::{Credentials, TokenTransport, valid_app_id, valid_token},
};
use secret_service::{Collection, EncryptionType, Error, Item, SecretService};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, future::Future, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const SERVICE: &str = "com.suonelo.desktop.qobuz.v1";
const RECORD_MAGIC: &[u8] = b"SUONELO-SECRET-SERVICE-V1\0";
const CONTENT_TYPE: &str = "application/octet-stream";
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Update {
    profile: Option<String>,
    before: Option<SecretRecord>,
    after: Option<SecretRecord>,
}

impl Vault {
    pub fn linux(root: &Path) -> Self {
        // The same canonical directory identifies the filesystem lock, account
        // state, and keyring item, including when XDG_DATA_HOME is overridden.
        let profile = hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()));
        Self {
            linux_profile: Some(profile),
            linux_loading: true,
            record: None,
            load_error: None,
        }
    }

    #[cfg(test)]
    pub fn new(_: std::path::PathBuf) -> Self {
        Self {
            linux_profile: None,
            linux_loading: false,
            record: None,
            load_error: None,
        }
    }

    pub fn pending_linux_profile(&self) -> Option<String> {
        self.linux_profile.clone().filter(|_| self.linux_loading)
    }

    pub fn is_loading(&self) -> bool {
        self.linux_loading
    }

    pub fn ensure_loaded(&self) -> Result<()> {
        if self.linux_loading {
            return Err(loading_error());
        }
        self.load_error.clone().map_or(Ok(()), Err)
    }

    pub fn retry_linux_read(&mut self) -> bool {
        let retry = self.load_error.is_some() && !self.linux_loading;
        if retry {
            self.linux_loading = true;
            self.load_error = None;
        }
        retry
    }

    pub async fn read_linux(profile: String, cancel: &CancellationToken) -> Self {
        let loaded = interruptible(cancel, async {
            let service = connect().await?;
            let (_, item) = open_profile(&service, &profile).await?;
            read(item.as_ref()).await
        })
        .await;
        let (record, load_error) = match loaded {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            linux_profile: Some(profile),
            linux_loading: false,
            record,
            load_error,
        }
    }

    pub fn linux_save(&self, credentials: Credentials, id: String) -> Result<(String, Update)> {
        self.ensure_loaded()?;
        let (fingerprint, next) = self.prepare_save(credentials, id, unix_time_ms())?;
        Ok((
            fingerprint,
            self.update(next.or_else(|| self.record.clone())),
        ))
    }

    pub fn linux_reset(&self) -> Result<Update> {
        self.ensure_loaded()?;
        Ok(self.update(self.signed_out_record()))
    }

    fn update(&self, after: Option<SecretRecord>) -> Update {
        Update {
            profile: self.linux_profile.clone(),
            before: self.record.clone(),
            after,
        }
    }

    pub fn finish_linux_update(&mut self, update: Update) {
        self.record = update.after;
        self.load_error = None;
    }

    pub fn linux_update_failed(&mut self, error: &AppError) {
        if matches!(error.code, "vault_changed" | "vault_invalid") {
            self.load_error = Some(error.clone());
        }
    }
}

impl Update {
    pub async fn commit(
        self,
        cancel: &CancellationToken,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        #[cfg(test)]
        if self.profile.is_none() {
            if cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            authorize()?;
            return Ok(self);
        }
        let profile = self.profile.as_deref().ok_or_else(vault_io)?;
        let service = interruptible(cancel, connect()).await?;
        let (collection, item) = interruptible(cancel, async {
            let (collection, item) = open_profile(&service, profile).await?;
            if read(item.as_ref()).await? != self.before {
                return Err(changed_error());
            }
            Ok((collection, item))
        })
        .await?;
        let encoded = self.after.as_ref().map(encode).transpose()?;
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        authorize()?;
        if self.before != self.after {
            // Once authorized, let the provider finish the write. Cancelling
            // the future after dispatch cannot undo a D-Bus method call.
            let result = tokio::time::timeout(WRITE_TIMEOUT, async {
                match (item, encoded.as_deref()) {
                    (Some(item), Some(payload)) => item.set_secret(payload, CONTENT_TYPE).await,
                    (Some(item), None) => item.delete().await,
                    (None, Some(payload)) => collection
                        .create_item(
                            "suonelo Qobuz credentials",
                            attributes(profile),
                            payload,
                            false,
                            CONTENT_TYPE,
                        )
                        .await
                        .map(|_| ()),
                    (None, None) => Ok(()),
                }
            })
            .await;
            match result {
                Ok(Ok(())) => (),
                Ok(Err(error @ (Error::Locked | Error::Prompt))) => {
                    return Err(service_error(error));
                }
                // A lost reply may follow a committed operation. Require a
                // fresh read before permitting another credential change.
                _ => return Err(changed_error()),
            }
        }
        Ok(self)
    }
}

fn attributes(profile: &str) -> HashMap<&str, &str> {
    HashMap::from([("application", SERVICE), ("profile", profile)])
}

async fn connect() -> Result<SecretService<'static>> {
    SecretService::connect(EncryptionType::Dh)
        .await
        .map_err(service_error)
}

async fn open_profile<'a>(
    service: &'a SecretService<'_>,
    profile: &str,
) -> Result<(Collection<'a>, Option<Item<'a>>)> {
    let collection = service
        .get_default_collection()
        .await
        .map_err(service_error)?;
    match service.get_collection_by_alias("session").await {
        Ok(session) if session.collection_path == collection.collection_path => {
            return Err(access_error());
        }
        Ok(_) | Err(Error::NoResult) => (),
        Err(error) => return Err(service_error(error)),
    }
    if collection.is_locked().await.map_err(service_error)? {
        collection.unlock().await.map_err(service_error)?;
        collection.ensure_unlocked().await.map_err(service_error)?;
    }
    let matches = service
        .search_items(attributes(profile))
        .await
        .map_err(service_error)?;
    let mut items = matches.unlocked.into_iter().chain(matches.locked);
    let item = items.next();
    if items.next().is_some() {
        return Err(invalid_error());
    }
    if let Some(item) = &item {
        let in_collection = collection
            .search_items(attributes(profile))
            .await
            .map_err(service_error)?
            .iter()
            .any(|entry| entry.item_path == item.item_path);
        if !in_collection {
            return Err(changed_error());
        }
        if item.is_locked().await.map_err(service_error)? {
            item.unlock().await.map_err(service_error)?;
            item.ensure_unlocked().await.map_err(service_error)?;
        }
    }
    Ok((collection, item))
}

async fn read(item: Option<&Item<'_>>) -> Result<Option<SecretRecord>> {
    let Some(item) = item else {
        return Ok(None);
    };
    let payload = Zeroizing::new(item.get_secret().await.map_err(service_error)?);
    decode(&payload).map(Some).ok_or_else(invalid_error)
}

fn valid(record: &SecretRecord) -> bool {
    let credentials = &record.credentials;
    verified_identity(record)
        && record.fingerprint_key != [0; 32]
        && if credentials == &Credentials::default() {
            record.token_committed_at_ms.is_none()
        } else {
            valid_token(&credentials.token)
                && valid_app_id(&credentials.app_id)
                && !credentials.app_secret.trim().is_empty()
                && credentials.app_secret.len() as u64 <= MAX_RECORD
                && (credentials.transport == TokenTransport::Query
                    || (record.token_committed_at_ms.is_none()
                        && reqwest::header::HeaderValue::from_str(&credentials.token).is_ok()))
        }
}

fn decode(bytes: &[u8]) -> Option<SecretRecord> {
    if bytes.len() as u64 > MAX_RECORD {
        return None;
    }
    let record = serde_json::from_slice(bytes.strip_prefix(RECORD_MAGIC)?).ok()?;
    valid(&record).then_some(record)
}

fn encode(record: &SecretRecord) -> Result<Zeroizing<Vec<u8>>> {
    if !valid(record) {
        return Err(vault_io());
    }
    let mut bytes = Zeroizing::new(RECORD_MAGIC.to_vec());
    serde_json::to_writer(&mut *bytes, record).map_err(|_| vault_io())?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(vault_io());
    }
    Ok(bytes)
}

async fn interruptible<T>(
    cancel: &CancellationToken,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(cancelled_error()),
        outcome = tokio::time::timeout(READ_TIMEOUT, operation) => {
            outcome.unwrap_or_else(|_| Err(access_error()))
        }
    }
}

fn service_error(error: Error) -> AppError {
    match error {
        Error::Prompt => cancelled_error(),
        Error::Locked => AppError::new(
            "vault_locked",
            "account",
            "Unlock the Linux keyring, then retry credential access.",
            true,
        ),
        Error::Crypto(_) => invalid_error(),
        _ => access_error(),
    }
}

fn access_error() -> AppError {
    AppError::new(
        "vault_access",
        "account",
        "Linux Secret Service is unavailable. Check your session bus and persistent default keyring, then retry credential access.",
        true,
    )
}

fn cancelled_error() -> AppError {
    AppError::new(
        "vault_cancelled",
        "account",
        "The Linux keyring request was cancelled. Retry credential access to continue.",
        true,
    )
}

fn invalid_error() -> AppError {
    AppError::new(
        "vault_invalid",
        "account",
        "The Linux keyring contains an invalid or ambiguous suonelo credential record.",
        true,
    )
}

fn changed_error() -> AppError {
    AppError::new(
        "vault_changed",
        "account",
        "The Linux keyring record changed or could not be confirmed. Retry credential access before making changes.",
        true,
    )
}

pub(super) fn loading_error() -> AppError {
    AppError::new(
        "vault_loading",
        "account",
        "Waiting for the Linux keyring to open your saved credentials.",
        true,
    )
}

pub(super) fn vault_io() -> AppError {
    AppError::new(
        "vault_storage",
        "account",
        "The Linux keyring could not save your Qobuz credentials.",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(transport: TokenTransport) -> Credentials {
        Credentials {
            token: "synthetic-token".into(),
            app_id: "123456789".into(),
            app_secret: "synthetic-secret".into(),
            transport,
            ..Credentials::default()
        }
    }

    #[test]
    fn record_round_trips_identifiers_timestamps_and_both_transports() {
        let vault = Vault::new(Default::default());
        for transport in [TokenTransport::Query, TokenTransport::HeaderOnly] {
            let (_, record) = vault
                .prepare_save(credentials(transport), "42".into(), Some(1000))
                .unwrap();
            let record = record.unwrap();
            assert_eq!(
                record.token_committed_at_ms,
                (transport == TokenTransport::Query).then_some(1000)
            );
            let payload = encode(&record).unwrap();
            assert!(decode(&payload).is_some_and(|decoded| decoded == record));
            assert!(decode(&payload[1..]).is_none());
        }
    }

    #[test]
    fn malformed_oversized_and_ambiguous_records_are_rejected() {
        let vault = Vault::new(Default::default());
        let (_, record) = vault
            .prepare_save(credentials(TokenTransport::Query), "42".into(), Some(1000))
            .unwrap();
        let mut record = record.unwrap();
        let payload = encode(&record).unwrap();
        let mut ambiguous = payload[..payload.len() - 1].to_vec();
        ambiguous.extend_from_slice(br#", "account_id": "43" }"#);
        assert!(decode(&ambiguous).is_none());
        assert!(decode(&vec![0; MAX_RECORD as usize + 1]).is_none());
        record.account_id = Some("042".into());
        assert_eq!(encode(&record).unwrap_err().code, "vault_storage");
        record.account_id = Some("42".into());
        record.credentials.token = "two words".into();
        assert_eq!(encode(&record).unwrap_err().code, "vault_storage");
    }

    #[tokio::test]
    async fn sign_out_retains_verified_identity_and_cancelled_update_is_not_installed() {
        let mut vault = Vault::new(Default::default());
        let (fingerprint, update) = vault
            .linux_save(credentials(TokenTransport::Query), "42".into())
            .unwrap();
        let cancel = CancellationToken::new();
        vault.finish_linux_update(update.commit(&cancel, || Ok(())).await.unwrap());
        let key = vault.record.as_ref().unwrap().fingerprint_key;
        let (_, update) = vault
            .linux_save(credentials(TokenTransport::Query), "43".into())
            .unwrap();
        cancel.cancel();
        let error = update
            .commit(&cancel, || panic!("cancelled operation was authorized"))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "vault_cancelled");
        assert_eq!(vault.account_id(), Some("42"));

        let cancel = CancellationToken::new();
        let update = vault.linux_reset().unwrap();
        vault.finish_linux_update(update.commit(&cancel, || Ok(())).await.unwrap());
        assert_eq!(vault.account_id(), Some("42"));
        assert!(vault.credentials().is_none());
        assert_eq!(vault.record.as_ref().unwrap().fingerprint_key, key);
        let (restored, _) = vault
            .linux_save(credentials(TokenTransport::Query), "42".into())
            .unwrap();
        assert_eq!(restored, fingerprint);
    }

    #[tokio::test]
    #[ignore = "requires a private D-Bus session with an unlocked persistent test collection"]
    async fn isolated_provider_lifecycle() {
        assert_eq!(
            std::env::var("SUONELO_TEST_KEYRING").as_deref(),
            Ok("isolated")
        );
        let root = tempfile::tempdir().unwrap();
        let phase = std::env::var("SUONELO_TEST_KEYRING_PHASE").ok();
        let root = if phase.is_some() {
            std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap())
                .join("isolated-keyring-lifecycle-test")
        } else {
            root.path().to_owned()
        };
        let qa = root.join("qa");
        let profile = Vault::linux(&root).pending_linux_profile().unwrap();
        let qa_profile = Vault::linux(&qa).pending_linux_profile().unwrap();
        let cancel = CancellationToken::new();
        let mut vault = Vault::read_linux(profile.clone(), &cancel).await;
        vault.ensure_loaded().unwrap();
        if phase.as_deref() != Some("restore") {
            assert!(vault.record.is_none());
            let (_, initial) = vault
                .linux_save(credentials(TokenTransport::Query), "42".into())
                .unwrap();
            vault.finish_linux_update(initial.commit(&cancel, || Ok(())).await.unwrap());
        }
        if phase.as_deref() == Some("write") {
            return;
        }
        let mut restored = Vault::read_linux(profile.clone(), &cancel).await;
        assert_eq!(restored.account_id(), Some("42"));
        assert!(restored.credentials().is_some());
        let other = Vault::read_linux(qa_profile, &cancel).await;
        other.ensure_loaded().unwrap();
        assert!(other.record.is_none());

        let (_, stale) = restored
            .linux_save(credentials(TokenTransport::Query), "43".into())
            .unwrap();
        let signed_out = vault.linux_reset().unwrap();
        vault.finish_linux_update(signed_out.commit(&cancel, || Ok(())).await.unwrap());
        let failure = stale
            .commit(&cancel, || panic!("stale operation was authorized"))
            .await
            .err()
            .unwrap();
        assert_eq!(failure.code, "vault_changed");
        restored = Vault::read_linux(profile.clone(), &cancel).await;
        assert_eq!(restored.account_id(), Some("42"));
        assert!(restored.credentials().is_none());

        let service = connect().await.unwrap();
        let (collection, item) = open_profile(&service, &profile).await.unwrap();
        let payload = encode(restored.record.as_ref().unwrap()).unwrap();
        let duplicate = collection
            .create_item(
                "isolated suonelo test duplicate",
                attributes(&profile),
                &payload,
                false,
                CONTENT_TYPE,
            )
            .await
            .unwrap();
        let invalid = Vault::read_linux(profile.clone(), &cancel).await;
        assert_eq!(invalid.ensure_loaded().unwrap_err().code, "vault_invalid");
        duplicate.delete().await.unwrap();
        item.unwrap().delete().await.unwrap();
        let empty = Vault::read_linux(profile, &cancel).await;
        empty.ensure_loaded().unwrap();
        assert!(empty.record.is_none());
    }
}
