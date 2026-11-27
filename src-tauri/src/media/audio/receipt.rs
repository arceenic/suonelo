use std::path::Path;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::super::{
    ArtworkResolution, DeliveredQuality, LyricsMode, Media, MediaError, metadata,
    output_store::Receipt,
};
#[cfg(target_os = "macos")]
use super::inspect::{inspect, verify_full_duration};
use super::{
    AudioRequest, Enrichment,
    pipeline::lyrics_unavailable,
};
use crate::qobuz::LyricsSource;
use crate::qobuz::Lyrics;

pub(super) fn audio_receipt(
    request: AudioRequest<'_>,
    quality: &DeliveredQuality,
    embedded: bool,
    cover_embedded: bool,
    lyrics_note: Option<(LyricsSource, bool)>,
) -> Result<Receipt, MediaError> {
    let options = request.options;
    #[derive(Serialize)]
    struct Variant<'a> {
        quality: &'a DeliveredQuality,
        tags: std::collections::BTreeMap<&'a str, bool>,
        reencode_flac: bool,
        embedded: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        lyrics_missing: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cover: Option<(ArtworkResolution, bool)>,
        #[serde(rename = "_lyrics", skip_serializing_if = "Option::is_none")]
        lyrics: Option<(&'static str, bool)>,
    }
    let mut tags: std::collections::BTreeMap<_, _> = options
        .tags
        .iter()
        .filter_map(|(key, &enabled)| {
            let default = metadata::is_default_enabled(key);
            (enabled != default).then_some((key.as_str(), enabled))
        })
        .collect();
    // Earlier receipts encoded ReplayGain as on by default. Keep that comparison
    // baseline so existing receipts can only match the same effective option.
    tags.remove("replaygain");
    if options.tags.get("replaygain") != Some(&true) {
        tags.insert("replaygain", false);
    }
    let variant = Variant {
        quality,
        tags,
        // Preserve the existing receipt fingerprint for the default processing path.
        reencode_flac: false,
        embedded,
        lyrics_missing: (options.lyrics_mode == LyricsMode::Embed && !embedded).then_some(true),
        cover: (options.download_cover_art && options.embed_cover_art)
            .then_some((options.artwork, cover_embedded)),
        lyrics: lyrics_note.map(|(source, timed)| {
            let provider = match source {
                LyricsSource::Qobuz => "qobuz",
                LyricsSource::Lrclib => "lrclib",
            };
            (provider, timed)
        }),
    };
    let variant = serde_json::to_string(&variant).map_err(|_| {
        MediaError::new(
            "file_index",
            "The output settings could not be indexed.",
            true,
        )
    })?;
    Ok(Receipt {
        kind: if request.credentials.access_mode.is_preview() { "audio_preview" } else { "audio" },
        identity: request.track.id.clone(),
        variant,
    })
}

impl Media {
    pub(super) async fn verify_audio(
        &self,
        request: AudioRequest<'_>,
        path: &Path,
        quality: &DeliveredQuality,
        cancel: &CancellationToken,
    ) -> Result<Option<Enrichment>, MediaError> {
        if let Some(approval) = request.replacement {
            self.store.verify_replacement(path, &request.track.id, approval, cancel).await?;
            return Ok(None);
        }
        let lyrics = if request.options.lyrics_mode == LyricsMode::Embed {
            &[true, false][..]
        } else {
            &[false][..]
        };
        let covers = if request.options.download_cover_art && request.options.embed_cover_art {
            &[true, false][..]
        } else {
            &[false][..]
        };
        let mut conflict = None;
        for &lyrics_embedded in lyrics {
            for &cover_embedded in covers {
                let receipt = audio_receipt(
                    request,
                    quality,
                    lyrics_embedded,
                    cover_embedded,
                    None,
                )?;
                match self.store.verify_variant(path, &receipt, cancel).await {
                    Ok(Some(matched_variant)) => {
                        #[cfg(target_os = "macos")]
                        if request.require_full_duration {
                            verify_full_duration(request.track, inspect(path, quality.format).await?)?;
                        }
                        let saved = if lyrics_embedded {
                            serde_json::from_str::<serde_json::Value>(&matched_variant)
                                .ok()
                                .and_then(|value| {
                                    let note = value.get("_lyrics")?.as_array()?;
                                    let source = match note.first()?.as_str()? {
                                        "qobuz" => LyricsSource::Qobuz,
                                        "lrclib" => LyricsSource::Lrclib,
                                        _ => return None,
                                    };
                                    Some(Lyrics {
                                        source,
                                        timed: note.get(1)?.as_bool()?,
                                        text: String::new(),
                                    })
                                })
                        } else {
                            None
                        };
                        return Ok(Some(Enrichment {
                            lyrics: saved,
                            lyrics_warning: (request.options.lyrics_mode == LyricsMode::Embed
                                && !lyrics_embedded)
                                .then(lyrics_unavailable),
                            cover_embedded,
                            ..Default::default()
                        }));
                    }
                    Ok(None) => return Ok(None),
                    Err(error) if error.code == "output_conflict" => conflict = Some(error),
                    Err(error) => return Err(error),
                }
            }
        }
        conflict.map_or(Ok(None), Err)
    }
}
