use super::{MAX_RECORD, SecretRecord, Vault, unix_time_ms, verified_identity};
use crate::{
    error::{AppError, Result},
    qobuz::{Credentials, TokenTransport, valid_app_id},
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

pub(super) const EMAIL_RECORD_MAGIC: &[u8] = b"SUONELO-EMAIL-V1\0";

impl Vault {
    pub fn new(path: PathBuf) -> Self {
        let result = match fs::File::open(&path) {
            Ok(file) => read_record(file).map(Some).ok_or_else(vault_load_error),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(vault_load_error()),
        };
        let (record, load_error) = match result {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            path,
            record,
            load_error,
        }
    }

    pub fn save(&mut self, credentials: Credentials, account_id: String) -> Result<String> {
        self.save_at(credentials, account_id, unix_time_ms())
    }

    pub(super) fn save_at(
        &mut self,
        credentials: Credentials,
        account_id: String,
        now_ms: Option<u64>,
    ) -> Result<String> {
        self.save_with(credentials, account_id, now_ms, Self::persist)
    }
}

pub(super) fn remove_record_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(vault_io()),
    }
}

fn read_record(file: fs::File) -> Option<SecretRecord> {
    let mut encrypted = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut encrypted).ok()?;
    if encrypted.len() as u64 > MAX_RECORD || encrypted.starts_with(b"QBDLXVT1") {
        return None;
    }
    let bytes = protect(&mut encrypted, false).ok()?;
    let (transport, payload) = if let Some(payload) = bytes.strip_prefix(EMAIL_RECORD_MAGIC) {
        (TokenTransport::HeaderOnly, payload)
    } else {
        (TokenTransport::Query, bytes.as_slice())
    };
    let record: SecretRecord = serde_json::from_slice(payload).ok()?;
    if record.credentials.transport != transport {
        return None;
    }
    let verified_identity = verified_identity(&record);
    if transport == TokenTransport::HeaderOnly {
        let credentials = &record.credentials;
        return (verified_identity
            && !credentials.token.is_empty()
            && reqwest::header::HeaderValue::from_str(&credentials.token).is_ok()
            && valid_app_id(&credentials.app_id)
            && !credentials.app_secret.is_empty())
        .then_some(record);
    }
    (!record.credentials.token.is_empty() || verified_identity).then_some(record)
}

pub(super) fn persist(path: &Path, record: &SecretRecord) -> Result<()> {
    let parent = path.parent().ok_or_else(vault_io)?;
    let mut bytes = Zeroizing::new(Vec::new());
    if record.credentials.transport == TokenTransport::HeaderOnly {
        // Older versions deserialize a plain JSON record and reject this envelope.
        bytes.extend_from_slice(EMAIL_RECORD_MAGIC);
    }
    serde_json::to_writer(&mut *bytes, record).map_err(|_| vault_io())?;
    let protected = protect(&mut bytes, true)?;
    let mut bundle = NamedTempFile::new_in(parent).map_err(|_| vault_io())?;
    bundle.write_all(&protected).map_err(|_| vault_io())?;
    bundle.as_file().sync_all().map_err(|_| vault_io())?;
    bundle.persist(path).map_err(|_| vault_io())?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn protect(bytes: &mut [u8], encrypt: bool) -> Result<Zeroizing<Vec<u8>>> {
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
            },
        },
        core::w,
    };
    use zeroize::Zeroize;
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| vault_io())?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI allocates the output. Copy it out, then clear and free the native buffer.
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                w!("suonelo credentials"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|_| vault_io())?;
        if output.pbData.is_null() {
            return Err(vault_io());
        }
        let buffer = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = Zeroizing::new(buffer.to_vec());
        buffer.zeroize();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

#[cfg(not(windows))]
pub(super) fn protect(_: &mut [u8], _: bool) -> Result<Zeroizing<Vec<u8>>> {
    Err(vault_io())
}

fn vault_load_error() -> AppError {
    AppError::new(
        "vault_unreadable",
        "account",
        "Windows could not read the saved credentials. Sign in again to replace them.",
        true,
    )
}

pub(super) fn vault_io() -> AppError {
    AppError::new(
        "vault_storage",
        "account",
        "Windows could not read or save your credentials.",
        true,
    )
}
