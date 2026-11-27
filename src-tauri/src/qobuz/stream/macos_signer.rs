#[cfg(target_os = "macos")]
use super::super::{AccessMode, ApiError, Credentials, QobuzClient, QobuzError, TokenTransport, zeroize_replace};
#[cfg(target_os = "macos")]
use super::{FORMATS, MP3_FORMAT, StreamOffer, no_stream_offer_error, read_mp3_preview, read_offer, validated_stream_floor};
#[cfg(target_os = "macos")]
use crate::recovery::RetryBudget;
#[cfg(target_os = "macos")]
use serde_json::Value;

// Session-local signer cache holds no token and is never persisted as an
// identity validation result. Key changes isolate token, App ID and source pair.
#[cfg(target_os = "macos")]
pub(in crate::qobuz) struct MediaSigner {
    key: [u8; 32],
    secret: zeroize::Zeroizing<String>,
}

#[cfg(target_os = "macos")]
fn signer_key(credentials: &Credentials) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for field in [
        &credentials.token,
        &credentials.app_id,
        &credentials.app_secret,
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().into()
}

#[cfg(target_os = "macos")]
impl QobuzClient {
    #[cfg(target_os = "macos")]
    pub async fn negotiate_stream(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<StreamOffer, QobuzError> {
        let operation = async {
            let mut remaining = 16;
            if credentials.transport != TokenTransport::Query
                || !credentials.app_pair_discovered
            {
                return self
                    .negotiate_track(
                        track_id,
                        requested_tier,
                        allow_fallback,
                        credentials,
                        budget,
                        &mut remaining,
                    )
                    .await;
            }
            // Serialize discovery for an automatic pair. Transfers proceed after
            // negotiation releases this lock, and later tracks reuse its result.
            let mut cache = self.media_signer.lock().await;
            let key = signer_key(credentials);
            let mut effective = credentials.clone();
            if let Some(signer) = cache.as_ref().filter(|signer| signer.key == key) {
                zeroize_replace(&mut effective.app_secret, &signer.secret);
            }
            let first = self
                .negotiate_track(
                    track_id,
                    requested_tier,
                    allow_fallback,
                    &effective,
                    budget,
                    &mut remaining,
                )
                .await;
            if !first
                .as_ref()
                .is_err_and(|error| error.code == "invalid_app_pair")
            {
                return first;
            }
            let candidates = self.discover_app_credentials().await?;
            // The identity validator accepted this App ID. A signing failure
            // cannot authorize changing it during a media request.
            if candidates.app_id != credentials.app_id {
                return first;
            }
            let previous_secret = zeroize::Zeroizing::new(effective.app_secret.clone());
            for secret in &candidates.secrets {
                if secret == &credentials.app_secret || secret == &*previous_secret {
                    continue;
                }
                zeroize_replace(&mut effective.app_secret, secret);
                match self
                    .negotiate_track(
                        track_id,
                        requested_tier,
                        allow_fallback,
                        &effective,
                        budget,
                        &mut remaining,
                    )
                    .await
                {
                    Ok(offer) => {
                        *cache = Some(MediaSigner {
                            key,
                            secret: zeroize::Zeroizing::new(effective.app_secret.clone()),
                        });
                        return Ok(offer);
                    }
                    Err(error) if error.code == "invalid_app_pair" => (),
                    Err(error) => return Err(error),
                }
            }
            first
        };
        if credentials.transport == TokenTransport::Query {
            tokio::time::timeout(std::time::Duration::from_secs(45), operation)
                .await
                .map_err(|_| {
                    QobuzError::new(
                        "media_negotiation_timeout",
                        "Qobuz media negotiation timed out. Retry the track.",
                        true,
                    )
                })?
        } else {
            operation.await
        }
    }

    #[cfg(target_os = "macos")]
    async fn bounded_stream_request(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<Value, ApiError> {
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            QobuzError::new(
                "media_negotiation_limit",
                "Qobuz could not provide an offer within the signing request limit.",
                false,
            )
        })?;
        self.signed_stream_request_with_budget(track_id, format_id, credentials, budget)
            .await
    }

    #[cfg(target_os = "macos")]
    async fn negotiate_track(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<StreamOffer, QobuzError> {
        let floor = validated_stream_floor(requested_tier, allow_fallback)?;
        if credentials.access_mode.is_preview() {
            return self
                .preview_offer(track_id, credentials, budget, remaining)
                .await;
        }
        let token_form = credentials.transport == TokenTransport::Query
            && credentials.access_mode == AccessMode::Unknown;
        let mut last_quality_error = None;
        let mut full_offer_seen = false;
        for attempted in (floor..=requested_tier).rev() {
            let format_id = FORMATS[usize::from(attempted - 1)];
            let response = self
                .bounded_stream_request(track_id, format_id, credentials, budget, remaining)
                .await;
            let result = match response {
                Ok(body) => {
                    full_offer_seen |=
                        body.get("sample").is_none() || body["sample"].as_bool() == Some(false);
                    if token_form
                        && format_id == MP3_FORMAT
                        && let Some(preview) = read_mp3_preview(&body)
                    {
                        return Ok(preview);
                    }
                    read_offer(&body, floor, attempted)
                }
                Err(failure) => Err(failure.error),
            };
            match result {
                Ok(offer) => return Ok(offer),
                Err(error) if error.code == "quality_unavailable" => {
                    last_quality_error = Some(error)
                }
                Err(error)
                    if token_form
                        && matches!(error.code, "resource_unavailable" | "missing_entitlement") =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if token_form && !full_offer_seen {
            return self
                .preview_offer(track_id, credentials, budget, remaining)
                .await;
        }
        Err(no_stream_offer_error(allow_fallback, requested_tier, floor, last_quality_error))
    }

    #[cfg(target_os = "macos")]
    async fn preview_offer(
        &self,
        track_id: &str,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<StreamOffer, QobuzError> {
        let unavailable = || {
            QobuzError::new(
                "preview_unavailable",
                "Qobuz did not offer a usable MP3 preview for this track.",
                false,
            )
        };
        match self
            .bounded_stream_request(track_id, MP3_FORMAT, credentials, budget, remaining)
            .await
        {
            Ok(body) => read_mp3_preview(&body).ok_or_else(unavailable),
            Err(failure)
                if matches!(
                    failure.error.code,
                    "resource_unavailable" | "quality_unavailable" | "missing_entitlement"
                ) =>
            {
                Err(unavailable())
            }
            Err(failure) => Err(failure.error),
        }
    }
}
