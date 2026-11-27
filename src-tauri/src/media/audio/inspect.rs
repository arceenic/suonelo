use std::{
    fs::File,
    io::Read,
    path::Path,
};

use lofty::{
    config::ParseOptions,
    file::AudioFile,
    mpeg::{Layer, MpegFile},
};

use super::{AudioProperties, super::{AudioFormat, DeliveredQuality, MediaError, blocking}};
#[cfg(target_os = "macos")]
use crate::qobuz::ResolvedTrack;
use crate::qobuz::StreamOffer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::media) struct FlacInfo {
    pub(super) sample_rate: u32,
    pub(super) channels: u8,
    pub(super) bit_depth: u8,
    pub(super) samples: u64,
    pub(super) checksum: [u8; 16],
}

impl FlacInfo {
    pub(super) fn known_checksum(self) -> bool {
        self.checksum != [0; 16]
    }

    pub(super) fn matches_pcm(self, new: Self) -> bool {
        self.sample_rate == new.sample_rate
            && self.channels == new.channels
            && self.bit_depth == new.bit_depth
            && (self.samples == 0 || self.samples == new.samples)
            && new.known_checksum()
            && (!self.known_checksum() || self.checksum == new.checksum)
    }
}

pub(super) fn bad_audio() -> MediaError {
    MediaError::new(
        "audio_format",
        "The transferred file did not match the expected audio container.",
        true,
    )
}

pub(super) fn read_flac(path: &Path) -> Result<FlacInfo, MediaError> {
    let mut input = File::open(path).map_err(MediaError::disk)?;
    let mut stream_info = [0_u8; 42];
    input
        .read_exact(&mut stream_info)
        .map_err(|_| bad_audio())?;
    if &stream_info[..4] != b"fLaC"
        || stream_info[4] & 0x7f != 0
        || stream_info[5..8] != [0, 0, 34]
        || input.metadata().map_err(MediaError::disk)?.len() <= stream_info.len() as u64
    {
        return Err(bad_audio());
    }
    let properties = u64::from_be_bytes(
        stream_info[18..26]
            .try_into()
            .expect("STREAMINFO properties"),
    );
    let sample_rate = (properties >> 44) as u32;
    let channels = ((properties >> 41) & 0x7) as u8 + 1;
    let bit_depth = ((properties >> 36) & 0x1f) as u8 + 1;
    let samples = properties & ((1_u64 << 36) - 1);
    let checksum = stream_info[26..42].try_into().expect("STREAMINFO checksum");
    if sample_rate == 0 || bit_depth < 4 {
        return Err(bad_audio());
    }
    Ok(FlacInfo {
        sample_rate,
        channels,
        bit_depth,
        samples,
        checksum,
    })
}

pub(super) async fn inspect(path: &Path, format: AudioFormat) -> Result<AudioProperties, MediaError> {
    let path = path.to_owned();
    blocking(move || match format {
        AudioFormat::Flac => read_flac(&path).map(AudioProperties::Flac),
        AudioFormat::Mp3 => {
            let mut file = File::open(path).map_err(MediaError::disk)?;
            let audio =
                MpegFile::read_from(&mut file, ParseOptions::new()).map_err(|_| bad_audio())?;
            let properties = audio.properties();
            if properties.duration().is_zero()
                || *properties.layer() != Layer::Layer3
                || properties.sample_rate() == 0
            {
                return Err(bad_audio());
            }
            Ok(AudioProperties::Mp3 {
                sample_rate: properties.sample_rate(),
                #[cfg(target_os = "macos")]
                duration_ms: properties.duration().as_millis().min(u128::from(u64::MAX)) as u64,
            })
        }
    })
    .await
}

#[cfg(target_os = "macos")]
pub(super) fn verify_full_duration(track: &ResolvedTrack, properties: AudioProperties) -> Result<(), MediaError> {
    let expected = track.track["duration"].as_f64().filter(|s| s.is_finite() && *s > 0.0);
    let actual = match properties {
        AudioProperties::Flac(info) => info.samples as f64 / f64::from(info.sample_rate),
        AudioProperties::Mp3 { duration_ms, .. } => duration_ms as f64 / 1_000.0,
    };
    if !expected.is_some_and(|seconds| actual > 0.0 && (actual - seconds).abs() <= (seconds * 0.02).max(2.0)) {
        return Err(MediaError::new("full_track_duration", "The audio duration could not be verified against the full track. No full-track file was saved.", false));
    }
    Ok(())
}

pub(super) fn actual_tier(depth: u32, hz: f64) -> Option<u8> {
    match (depth, hz) {
        (16, 44.1) => Some(2),
        (16..=24, 44.1..=96.0) => Some(3),
        (16..=24, 96.0..=192.0) => Some(4),
        _ => None,
    }
}

pub(super) fn offered_quality(offer: &StreamOffer) -> Option<DeliveredQuality> {
    if offer.tier == 1 {
        return Some(DeliveredQuality {
            tier: 1,
            format: AudioFormat::Mp3,
            bit_depth: None,
            sample_rate: Some(offer.sample_rate.unwrap_or(44.1)),
        });
    }
    let (depth, rate) = (offer.bit_depth?, offer.sample_rate?);
    let tier = actual_tier(depth, rate)?;
    (tier <= offer.tier).then_some(DeliveredQuality {
        tier,
        format: AudioFormat::Flac,
        bit_depth: Some(depth),
        sample_rate: Some(rate),
    })
}

pub(super) fn delivered_quality(
    properties: AudioProperties,
    offer_tier: u8,
) -> Result<DeliveredQuality, MediaError> {
    match properties {
        AudioProperties::Mp3 { sample_rate, .. } if offer_tier == 1 => Ok(DeliveredQuality {
            tier: 1,
            format: AudioFormat::Mp3,
            bit_depth: None,
            sample_rate: Some(f64::from(sample_rate) / 1_000.0),
        }),
        AudioProperties::Flac(info) => {
            let depth = u32::from(info.bit_depth);
            let rate = f64::from(info.sample_rate) / 1_000.0;
            let tier = actual_tier(depth, rate)
                .filter(|tier| *tier <= offer_tier)
                .ok_or_else(|| {
                    MediaError::new(
                        "audio_quality",
                        "The delivered FLAC properties were outside the offered quality range.",
                        true,
                    )
                })?;
            Ok(DeliveredQuality {
                tier,
                format: AudioFormat::Flac,
                bit_depth: Some(depth),
                sample_rate: Some(rate),
            })
        }
        _ => Err(bad_audio()),
    }
}
