mod macos_signer;
#[cfg(test)]
mod tests;

#[cfg(target_os = "macos")]
pub(super) use macos_signer::MediaSigner;

use super::{
    ApiError, Credentials, QobuzClient, QobuzError, f64_value, media_url, signed_params, u32_value,
};
use crate::recovery::RetryBudget;
use serde_json::Value;
use url::Url;

// The URL is short-lived and stays on the Rust side of the process boundary.
pub struct StreamOffer {
    pub url: Url,
    pub tier: u8,
    pub format_id: u32,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    pub restrictions: Vec<String>,
    #[cfg(target_os = "macos")]
    pub is_preview: bool,
    #[cfg(target_os = "macos")]
    pub requires_full_duration: bool,
}

pub(super) const FORMATS: [u32; 4] = [5, 6, 7, 27];
pub(super) const MP3_FORMAT: u32 = FORMATS[0];

pub(super) fn validated_stream_floor(
    requested_tier: u8,
    allow_fallback: bool,
) -> Result<u8, QobuzError> {
    if !(1..=4).contains(&requested_tier) {
        return Err(QobuzError::new(
            "invalid_quality",
            "Select a quality tier from 1 through 4.",
            false,
        ));
    }
    Ok(if allow_fallback {
        requested_tier.min(2)
    } else {
        requested_tier
    })
}

pub(super) fn no_stream_offer_error(
    allow_fallback: bool,
    requested_tier: u8,
    floor: u8,
    last_quality_error: Option<QobuzError>,
) -> QobuzError {
    let message = if !allow_fallback && requested_tier > 1 {
        "No offer is available at the selected FLAC tier."
    } else if floor == 2 {
        "No eligible FLAC offer is available for this track."
    } else {
        "No eligible MP3 offer is available for this track."
    };
    let detail = last_quality_error.map(|error| format!("{message} {}", error.message));
    QobuzError::new(
        "quality_unavailable",
        detail.as_deref().unwrap_or(message),
        false,
    )
}

impl QobuzClient {
    #[cfg(test)]
    pub(super) async fn signed_stream_request(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
    ) -> Result<Value, ApiError> {
        self.signed_stream_request_with_budget(
            track_id,
            format_id,
            credentials,
            &RetryBudget::default(),
        )
        .await
    }

    pub(super) async fn signed_stream_request_with_budget(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<Value, ApiError> {
        let signature_base =
            format!("trackgetFileUrlformat_id{format_id}intentstreamtrack_id{track_id}");
        self.request_with_budget(
            "track/getFileUrl",
            || {
                let mut params = signed_params(&signature_base, &credentials.app_secret);
                params.extend([
                    ("track_id", track_id.to_owned()),
                    ("format_id", format_id.to_string()),
                    ("intent", "stream".to_owned()),
                ]);
                params
            },
            credentials,
            budget,
        )
        .await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn negotiate_stream(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<StreamOffer, QobuzError> {
        if credentials.access_mode.is_preview() {
            let unavailable = || {
                QobuzError::new(
                    "preview_unavailable",
                    "Qobuz did not offer a usable MP3 preview for this track.",
                    false,
                )
            };
            let response = self
                .signed_stream_request_with_budget(track_id, MP3_FORMAT, credentials, budget)
                .await;
            return match response {
                Ok(body) => read_mp3_preview(&body).ok_or_else(unavailable),
                Err(failure) if matches!(failure.error.code, "resource_unavailable" | "quality_unavailable") => Err(unavailable()),
                Err(failure) => Err(failure.error),
            };
        }
        let floor = validated_stream_floor(requested_tier, allow_fallback)?;
        let mut last_quality_error = None;
        for attempted in (floor..=requested_tier).rev() {
            let format_id = FORMATS[usize::from(attempted - 1)];
            let response = self
                .signed_stream_request_with_budget(track_id, format_id, credentials, budget)
                .await;
            let result = match response {
                Ok(body) if credentials.access_mode == super::AccessMode::Unknown => {
                    read_token_offer(&body, format_id, floor, attempted)
                }
                Ok(body) => read_offer(&body, floor, attempted),
                Err(failure) => Err(failure.error),
            };
            match result {
                Ok(offer) => return Ok(offer),
                Err(error) if error.code == "quality_unavailable" => {
                    last_quality_error = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(no_stream_offer_error(allow_fallback, requested_tier, floor, last_quality_error))
    }
}

#[cfg(not(target_os = "macos"))]
fn read_token_offer(
    body: &Value,
    requested_format: u32,
    floor: u8,
    ceiling: u8,
) -> Result<StreamOffer, QobuzError> {
    if read_mp3_preview(body).is_some() {
        return Err(QobuzError::new(
            "quality_unavailable",
            "Qobuz offered an MP3 preview for this track.",
            false,
        ));
    }
    // Signed full-track offers can omit the sample marker.
    if !matches!(body.get("sample"), None | Some(Value::Bool(false)))
        || u32_value(&body["format_id"]) != Some(requested_format)
    {
        return Err(QobuzError::new(
            "quality_unavailable",
            "Qobuz did not confirm a full stream in the requested format.",
            false,
        ));
    }
    read_offer(body, floor, ceiling)
}

pub(super) fn read_mp3_preview(body: &Value) -> Option<StreamOffer> {
    if body["sample"].as_bool() != Some(true) || u32_value(&body["format_id"]) != Some(MP3_FORMAT) {
        return None;
    }
    Some(StreamOffer {
        url: media_url(&body["url"])?,
        tier: 1,
        format_id: MP3_FORMAT,
        bit_depth: None,
        sample_rate: f64_value(&body["sampling_rate"]),
        restrictions: restriction_codes(&body["restrictions"]),
        #[cfg(target_os = "macos")]
        is_preview: true,
        #[cfg(target_os = "macos")]
        requires_full_duration: false,
    })
}

pub(super) fn read_offer(body: &Value, floor: u8, ceiling: u8) -> Result<StreamOffer, QobuzError> {
    let restrictions = restriction_codes(&body["restrictions"]);
    let quality_error = |reason: &str| QobuzError::new("quality_unavailable", reason, false);
    #[cfg(not(target_os = "macos"))]
    if body["sample"].as_bool() == Some(true) {
        return Err(quality_error("Qobuz offered a preview of the track."));
    }
    #[cfg(target_os = "macos")]
    let requires_full_duration = match body.get("sample") {
        Some(Value::Bool(false)) => false,
        None => true, // Legacy full responses omit sample; verify transferred duration.
        _ => {
            return Err(quality_error(
                "Qobuz did not provide a usable full-track offer.",
            ));
        }
    };
    let format_id = u32_value(&body["format_id"])
        .ok_or_else(|| quality_error("Qobuz did not identify the offered audio format."))?;
    let tier = FORMATS
        .iter()
        .position(|format| *format == format_id)
        .map(|index| index as u8 + 1)
        .filter(|tier| (floor..=ceiling).contains(tier))
        .ok_or_else(|| {
            quality_error("Qobuz offered a format outside the allowed quality range.")
        })?;
    let url = media_url(&body["url"]).ok_or_else(|| {
        if restrictions.iter().any(|code| {
            let code = code.to_ascii_lowercase();
            code.contains("format") || code.contains("quality")
        }) {
            quality_error("Qobuz restricted access to this format.")
        } else {
            QobuzError::new(
                "resource_unavailable",
                "Qobuz did not provide a usable full-track stream URL.",
                false,
            )
        }
    })?;
    Ok(StreamOffer {
        url,
        tier,
        format_id,
        bit_depth: u32_value(&body["bit_depth"]).filter(|depth| *depth > 0),
        sample_rate: f64_value(&body["sampling_rate"]),
        restrictions,
        #[cfg(target_os = "macos")]
        is_preview: false,
        #[cfg(target_os = "macos")]
        requires_full_duration,
    })
}

fn restriction_codes(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|restriction| {
            let code = restriction
                .as_str()
                .or_else(|| restriction["code"].as_str())?;
            (code.len() <= 80 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then(|| code.to_owned())
        })
        .collect()
}
