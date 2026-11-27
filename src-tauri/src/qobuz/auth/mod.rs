mod discovery;
#[cfg(test)]
mod tests;

pub(super) use discovery::{exchange_key, parse_player_bundle};
use discovery::discovery_unavailable;

use super::{
    ApiError, QobuzClient, QobuzError, TokenTransport,
    parse_positive_id,
    stream::{MP3_FORMAT, read_mp3_preview, read_offer},
    zeroize_replace,
};
use crate::recovery::RetryBudget;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, time::Duration};
use zeroize::Zeroize;
#[cfg(target_os = "macos")]
use zeroize::ZeroizeOnDrop;

const PROBE_TRACK: &str = "5966783";
const SIGNING_PROBE_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    #[default]
    Unknown,
    Full,
    Preview,
}

impl AccessMode {
    pub fn is_preview(self) -> bool {
        self == Self::Preview
    }

    pub(crate) fn allows_preview_selection(self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self != Self::Full
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.is_preview()
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(target_os = "linux", serde(deny_unknown_fields))]
pub struct Credentials {
    pub token: String,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    #[serde(default, skip_serializing_if = "TokenTransport::is_query")]
    pub transport: TokenTransport,
    #[serde(skip)]
    pub access_mode: AccessMode,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub app_pair_discovered: bool,
}

#[cfg(target_os = "macos")]
impl Credentials {
    pub(crate) fn has_token_app_pair(&self) -> bool {
        self.transport == TokenTransport::Query
            && valid_app_id(self.app_id.trim())
            && !self.app_secret.trim().is_empty()
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ZeroizeOnDrop)]
pub(crate) struct AppPair {
    app_id: String,
    secret: String,
}

#[cfg(target_os = "macos")]
impl AppPair {
    pub(crate) fn from_token_credentials(credentials: &Credentials) -> Option<Self> {
        credentials.has_token_app_pair().then(|| Self {
            app_id: credentials.app_id.trim().to_owned(),
            secret: credentials.app_secret.trim().to_owned(),
        })
    }

    pub(crate) fn matches(&self, credentials: &Credentials) -> bool {
        self.app_id == credentials.app_id.trim() && self.secret == credentials.app_secret.trim()
    }

    pub(crate) fn is_complete(&self) -> bool {
        valid_app_id(&self.app_id) && !self.secret.trim().is_empty()
    }

    fn apply_to(&self, credentials: &mut Credentials) {
        zeroize_replace(&mut credentials.app_id, &self.app_id);
        zeroize_replace(&mut credentials.app_secret, &self.secret);
        credentials.app_pair_discovered = false;
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.token.zeroize();
        self.app_id.zeroize();
        self.app_secret.zeroize();
    }
}

pub(super) struct Candidates {
    pub(super) app_id: String,
    pub(super) secrets: Vec<String>,
}

impl Drop for Candidates {
    fn drop(&mut self) {
        self.secrets.zeroize();
    }
}

impl QobuzClient {
    #[cfg(all(test, not(target_os = "macos")))]
    pub async fn validate_account(
        &self,
        input: &Credentials,
    ) -> Result<(Credentials, String), QobuzError> {
        self.validate_account_with_reuse(input, false).await
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) async fn validate_account_with_reuse(
        &self,
        input: &Credentials,
        retry_reused_pair: bool,
    ) -> Result<(Credentials, String), QobuzError> {
        if input.transport == TokenTransport::HeaderOnly {
            return self.validate_with_candidates(input, None).await;
        }
        validation_deadline(validate_token(
            input.clone(),
            retry_reused_pair,
            || self.discover_app_credentials(),
            |credentials| async move { self.account_identity(&credentials).await },
        ))
        .await
    }
    #[cfg(target_os = "macos")]
    pub(crate) async fn validate_account(
        &self,
        input: &Credentials,
        retry_reused_pair: bool,
        remembered_pair: Option<&AppPair>,
    ) -> Result<(Credentials, String), QobuzError> {
        if input.transport == TokenTransport::HeaderOnly {
            return self.validate_with_candidates(input, None).await;
        }
        validation_deadline(validate_token(
            input.clone(),
            retry_reused_pair,
            remembered_pair,
            || self.discover_app_credentials(),
            |credentials| async move { self.account_identity(&credentials).await },
        ))
        .await
    }

    pub(super) async fn validate_with_candidates(
        &self,
        input: &Credentials,
        discovered: Option<Candidates>,
    ) -> Result<(Credentials, String), QobuzError> {
        validation_deadline(self.validate_hosted(input, discovered)).await
    }

    async fn account_identity(&self, credentials: &Credentials) -> Result<String, QobuzError> {
        let body = self
            .request_with_budget(
                "user/get",
                Vec::new,
                credentials,
                &RetryBudget::validation(),
            )
            .await
            .map_err(|failure| validation_error(failure.error))?;
        verified_account_id(&body)
    }

    async fn validate_hosted(
        &self,
        input: &Credentials,
        discovered: Option<Candidates>,
    ) -> Result<(Credentials, String), QobuzError> {
        let mut effective = normalized_credentials(input.clone())?;
        if effective.transport != TokenTransport::HeaderOnly {
            return Err(app_pair_rejected());
        }
        let candidates = if let Some(candidates) = discovered {
            candidates
        } else if valid_app_id(&effective.app_id) && !effective.app_secret.is_empty() {
            Candidates {
                app_id: effective.app_id.clone(),
                secrets: vec![std::mem::take(&mut effective.app_secret)],
            }
        } else {
            return Err(app_pair_rejected());
        };
        zeroize_replace(&mut effective.app_id, &candidates.app_id);
        let account_id = self.account_identity(&effective).await?;
        let budget = RetryBudget::validation();
        let mut probes = SigningProbes::new();
        let mut inconclusive = false;
        let mut preview_secret = None;
        for secret in &candidates.secrets {
            if probes.remaining == 0 {
                break;
            }
            zeroize_replace(&mut effective.app_secret, secret);
            let credentials = &effective;
            let budget = &budget;
            let outcome = probe_candidate(
                &mut probes,
                |track_id: String, format_id| async move {
                    let offer = self
                        .signed_stream_request_with_budget(
                            &track_id,
                            format_id,
                            credentials,
                            budget,
                        )
                        .await;
                    verify_discovered_offer(offer, format_id)
                },
                || self.alternate_signing_tracks(credentials, budget),
            )
            .await?;
            match outcome {
                SigningProbe::Verified => {
                    effective.access_mode = AccessMode::Full;
                    return Ok((effective, account_id));
                }
                SigningProbe::Preview => {
                    preview_secret.get_or_insert(secret);
                }
                SigningProbe::Unavailable => inconclusive = true,
                SigningProbe::Rejected => (),
            }
        }
        if let Some(secret) = preview_secret {
            zeroize_replace(&mut effective.app_secret, secret);
            effective.access_mode = AccessMode::Preview;
            return Ok((effective, account_id));
        }
        if inconclusive {
            Err(signing_inconclusive())
        } else {
            Err(app_pair_rejected())
        }
    }

    async fn alternate_signing_tracks(
        &self,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<Vec<String>, QobuzError> {
        match self
            .request_with_budget(
                "track/search",
                || vec![("query", "love".into()), ("limit", "20".into())],
                credentials,
                budget,
            )
            .await
        {
            Ok(body) => Ok(alternate_track_ids(&body)),
            Err(failure) if validation_interrupted(failure.error.code) => {
                Err(validation_error(failure.error))
            }
            Err(_) => Ok(Vec::new()),
        }
    }
}

async fn validation_deadline<T>(
    validate: impl Future<Output = Result<T, QobuzError>>,
) -> Result<T, QobuzError> {
    tokio::time::timeout(Duration::from_secs(45), validate)
        .await
        .map_err(|_| {
            QobuzError::new(
                "validation_timeout",
                "Qobuz validation timed out. Try again shortly.",
                true,
            )
        })?
}

fn normalized_credentials(mut credentials: Credentials) -> Result<Credentials, QobuzError> {
    credentials.access_mode = AccessMode::Unknown;
    for value in [
        &mut credentials.token,
        &mut credentials.app_id,
        &mut credentials.app_secret,
    ] {
        let trimmed = value.trim();
        if trimmed.len() != value.len() {
            let trimmed = trimmed.to_owned();
            value.zeroize();
            *value = trimmed;
        }
    }
    if !valid_token(&credentials.token) {
        return Err(QobuzError::new(
            "invalid_token",
            "Enter a valid Qobuz account token.",
            false,
        ));
    }
    Ok(credentials)
}

async fn validate_token<Discover, Verify>(
    input: Credentials,
    retry_reused_pair: bool,
    #[cfg(target_os = "macos")]
    remembered_pair: Option<&AppPair>,
    discover: impl FnOnce() -> Discover,
    verify: impl Fn(Credentials) -> Verify,
) -> Result<(Credentials, String), QobuzError>
where
    Discover: Future<Output = Result<Candidates, QobuzError>>,
    Verify: Future<Output = Result<String, QobuzError>>,
{
    let mut effective = normalized_credentials(input)?;
    // Repeating an App ID with another secret cannot change identity verification.
    #[cfg(target_os = "macos")]
    let remembered_pair = remembered_pair.filter(|pair| pair.app_id != effective.app_id);
    match (effective.app_id.is_empty(), effective.app_secret.is_empty()) {
        (true, true) => (),
        (false, false) if valid_app_id(&effective.app_id) => {
            match verify(effective.clone()).await {
                Ok(id) => return Ok((effective, id)),
                Err(error) if retry_reused_pair && app_id_may_be_rejected(&error) => (),
                Err(error) => return Err(error),
            }
        }
        (false, false) => {
            return Err(QobuzError::new(
                "invalid_app_id",
                "Enter a nine-digit Qobuz App ID.",
                false,
            ));
        }
        _ => {
            return Err(QobuzError::new(
                "incomplete_app_pair",
                "Enter both app fields, or clear both for automatic setup.",
                false,
            ));
        }
    }
    let mut candidates = discover().await?;
    #[cfg(target_os = "macos")]
    {
        effective.app_pair_discovered = true;
    }
    zeroize_replace(&mut effective.app_id, &candidates.app_id);
    effective.app_secret.zeroize();
    // The first provider-bundle candidate is provisional signing material.
    // Identity validation has no signed operations and does not prove this value.
    effective.app_secret = std::mem::take(
        candidates
            .secrets
            .first_mut()
            .ok_or_else(discovery_unavailable)?,
    );
    #[cfg(target_os = "macos")]
    let id = match verify(effective.clone()).await {
        Err(error) if app_id_may_be_rejected(&error) => {
            let Some(pair) = remembered_pair.filter(|pair| pair.app_id != effective.app_id) else {
                return Err(error);
            };
            pair.apply_to(&mut effective);
            verify(effective.clone()).await?
        }
        result => result?,
    };
    #[cfg(not(target_os = "macos"))]
    let id = verify(effective.clone()).await?;
    Ok((effective, id))
}

fn app_id_may_be_rejected(error: &QobuzError) -> bool {
    matches!(error.code, "invalid_app_id" | "account_rejected")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SigningProbe {
    Verified,
    Rejected,
    Unavailable,
    Preview,
}

struct SigningProbes {
    remaining: usize,
    alternatives: Option<Vec<String>>,
}

impl SigningProbes {
    fn new() -> Self {
        Self {
            remaining: SIGNING_PROBE_LIMIT,
            alternatives: None,
        }
    }
}

async fn probe_candidate<Offer, Tracks>(
    probes: &mut SigningProbes,
    probe: impl Fn(String, u32) -> Offer,
    load_tracks: impl FnOnce() -> Tracks,
) -> Result<SigningProbe, QobuzError>
where
    Offer: std::future::Future<Output = Result<SigningProbe, QobuzError>>,
    Tracks: std::future::Future<Output = Result<Vec<String>, QobuzError>>,
{
    let probe_formats = async |track: &str, remaining: &mut usize, saw_preview: &mut bool| {
        for format_id in [MP3_FORMAT, 6] {
            let Some(next) = remaining.checked_sub(1) else {
                break;
            };
            *remaining = next;
            match probe(track.to_owned(), format_id).await? {
                SigningProbe::Preview => *saw_preview = true,
                SigningProbe::Rejected if *saw_preview => return Ok(SigningProbe::Preview),
                SigningProbe::Unavailable => (),
                outcome => return Ok(outcome),
            }
        }
        Ok(SigningProbe::Unavailable)
    };
    let mut saw_preview = false;
    let initial = probe_formats(PROBE_TRACK, &mut probes.remaining, &mut saw_preview).await?;
    if initial != SigningProbe::Unavailable {
        return Ok(initial);
    }
    if probes.alternatives.is_none() && probes.remaining > 0 {
        probes.alternatives = Some(load_tracks().await?);
    }
    for track in probes.alternatives.as_deref().unwrap_or_default() {
        let outcome = probe_formats(track, &mut probes.remaining, &mut saw_preview).await?;
        if outcome != SigningProbe::Unavailable {
            return Ok(outcome);
        }
    }
    Ok(if saw_preview {
        SigningProbe::Preview
    } else {
        SigningProbe::Unavailable
    })
}

fn alternate_track_ids(body: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let tracks = body["tracks"]["items"].as_array().into_iter().flatten();
    let preferred = |track: &&Value| track["streamable"].as_bool() == Some(true);
    for track in tracks
        .clone()
        .filter(preferred)
        .chain(tracks.filter(|track| !preferred(track)))
    {
        if let Some(id) = parse_positive_id(&track["id"]) {
            if id != PROBE_TRACK && !ids.contains(&id) {
                ids.push(id);
                if ids.len() == 2 {
                    break;
                }
            }
        }
    }
    ids
}

fn verify_discovered_offer(
    offer: Result<Value, ApiError>,
    format_id: u32,
) -> Result<SigningProbe, QobuzError> {
    match offer {
        Ok(body)
            if body["sample"].as_bool() == Some(false)
                && read_offer(&body, 1, 2).is_ok_and(|offer| offer.format_id == format_id) =>
        {
            Ok(SigningProbe::Verified)
        }
        Ok(body) if format_id == MP3_FORMAT && read_mp3_preview(&body).is_some() => {
            Ok(SigningProbe::Preview)
        }
        Ok(_) => Ok(SigningProbe::Unavailable),
        Err(failure) if failure.error.code == "invalid_app_pair" => Ok(SigningProbe::Rejected),
        Err(failure) if validation_interrupted(failure.error.code) => {
            Err(validation_error(failure.error))
        }
        Err(failure)
            if matches!(
                failure.error.code,
                "resource_unavailable" | "quality_unavailable" | "invalid_response"
            ) =>
        {
            Ok(SigningProbe::Unavailable)
        }
        Err(_) => Err(signing_inconclusive()),
    }
}

fn validation_interrupted(code: &str) -> bool {
    matches!(
        code,
        "rate_limit" | "api_timeout" | "network" | "invalid_token" | "missing_entitlement"
    )
}

pub(super) fn signing_inconclusive() -> QobuzError {
    QobuzError::new(
        "signing_inconclusive",
        "Your account was recognized, but Suonelo could not verify streaming access. Try signing in again shortly.",
        true,
    )
}

fn app_pair_rejected() -> QobuzError {
    #[cfg(target_os = "macos")]
    let (message, retryable) = (
        "Qobuz did not accept the discovered signing material. Try signing in again.",
        false,
    );
    #[cfg(not(target_os = "macos"))]
    let (message, retryable) = (
        "Qobuz rejected automatic app verification. Try signing in again shortly.",
        true,
    );
    QobuzError::new("invalid_app_pair", message, retryable)
}

pub(super) fn validation_error(error: QobuzError) -> QobuzError {
    let message = match error.code {
        "invalid_app_id" => {
            "Qobuz rejected this App ID. Retry automatic setup or enter app credentials manually."
        }
        "account_rejected" => {
            "Qobuz rejected the token or App ID without identifying which. Retry automatic setup or enter app credentials manually."
        }
        "provider_blocked" => {
            "Qobuz blocked account verification. Check your access on Qobuz and try again."
        }
        "invalid_token" => "Qobuz rejected the account token. Enter a replacement token.",
        "invalid_app_pair" => {
            "Qobuz rejected the app pair. Enter both app fields, or submit the token with both blank to discover a new pair."
        }
        "missing_entitlement" => "This account does not have Qobuz streaming access.",
        "rate_limit" => "Qobuz is limiting requests. Try again after the cooldown.",
        "api_timeout" => "The Qobuz validation request timed out.",
        "network" => "Could not complete the Qobuz validation request. Try again shortly.",
        _ => {
            "Qobuz could not verify these credentials. Check the token and app pair and try again."
        }
    };
    QobuzError::new(error.code, message, error.retryable)
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_whitespace)
}

pub(crate) fn valid_app_id(value: &str) -> bool {
    value.len() == 9 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn verified_account_id(account: &Value) -> Result<String, QobuzError> {
    account
        .pointer("/user/id")
        .or_else(|| account.get("id"))
        .and_then(parse_positive_id)
        .ok_or_else(|| QobuzError::invalid_response("Qobuz did not return a verified account ID."))
}


