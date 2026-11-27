mod inspect;
mod pipeline;
mod receipt;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use super::{
    ArtworkCache, AudioReplacement, DeliveredQuality, MediaError, OutputOptions,
    ReleaseLayout, output_store::Stage,
};
use crate::qobuz::{Credentials, Lyrics, LyricsSource, QobuzClient, ResolvedTrack};
use crate::recovery::TransferContext;
use inspect::FlacInfo;

pub(super) const AUDIO_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct AudioRequest<'a> {
    pub client: &'a QobuzClient,
    pub credentials: &'a Credentials,
    pub track: &'a ResolvedTrack,
    pub options: &'a OutputOptions,
    pub root: &'a Path,
    pub pinned: Option<&'a ReleaseLayout>,
    pub sequence: usize,
    pub transfer: &'a TransferContext,
    pub replacement: Option<&'a AudioReplacement>,
    #[cfg(target_os = "macos")]
    pub require_full_duration: bool,
    pub artwork_cache: &'a ArtworkCache,
    pub cover_basename: &'a str,
}

pub struct AudioOutput {
    #[cfg(target_os = "macos")]
    pub is_preview: bool,
    pub finalized_at: Option<i64>,
    pub path: PathBuf,
    pub layout: ReleaseLayout,
    pub quality: DeliveredQuality,
    pub warning: Option<MediaError>,
    pub lyrics_timed: Option<bool>,
    pub lyrics_source: Option<LyricsSource>,
    pub reused: bool,
    pub cover_path: Option<PathBuf>,
    pub cover_warning: Option<MediaError>,
    pub cover_embedded: bool,
}

#[derive(Default)]
pub(super) struct Enrichment {
    pub(super) lyrics: Option<Lyrics>,
    pub(super) lyrics_warning: Option<MediaError>,
    pub(super) cover_path: Option<PathBuf>,
    pub(super) cover_warning: Option<MediaError>,
    pub(super) cover_embedded: bool,
}

pub struct LyricsOutput {
    pub path: PathBuf,
    pub timed: bool,
    pub source: LyricsSource,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct StagedKey {
    pub(super) track: String,
    pub(super) root: PathBuf,
    pub(super) tier: u8,
    pub(super) preview_only: bool,
}

pub(super) struct StagedAudio {
    pub file: Stage,
    pub owner: u64,
    pub(super) properties: AudioProperties,
    pub(super) tier: u8,
}

#[derive(Clone, Copy)]
pub(super) enum AudioProperties {
    Flac(FlacInfo),
    Mp3 {
        sample_rate: u32,
        #[cfg(target_os = "macos")]
        duration_ms: u64,
    },
}

