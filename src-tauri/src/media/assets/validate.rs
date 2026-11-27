use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::super::{
    MediaError, blocking, is_reparse_or_symlink,
    output_store::safe_directory,
    template::sanitize,
};
use super::{ART_LIMIT, ArtworkOutput, ArtworkResolution, artwork_error};

pub(super) const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

pub fn validate_cover_filename(input: &str) -> Result<(), MediaError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(());
    }
    let lower = input.to_lowercase();
    let stem = lower.split('.').next().unwrap_or_default().trim_end();
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
    let image_extension = lower.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension,
            "avif"
                | "bmp"
                | "gif"
                | "heic"
                | "heif"
                | "ico"
                | "jpg"
                | "jpeg"
                | "png"
                | "svg"
                | "tif"
                | "tiff"
                | "webp"
        )
    });
    if reserved
        || image_extension
        || input != sanitize(input, false)
        || input.chars().any(char::is_control)
    {
        return Err(MediaError::new(
            "cover_filename",
            "Use a valid filename without an image extension.",
            false,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(in crate::media) struct ImageInfo {
    pub(in crate::media) extension: &'static str,
    pub(in crate::media) dimensions: Option<(u32, u32)>,
}

impl ImageInfo {
    pub(super) fn output(self, path: PathBuf, resolution: ArtworkResolution, reused: bool) -> ArtworkOutput {
        ArtworkOutput {
            path,
            resolution,
            width: self.dimensions.map(|size| size.0),
            height: self.dimensions.map(|size| size.1),
            reused,
        }
    }
}

pub(super) fn image_info(path: &Path) -> Result<ImageInfo, MediaError> {
    let mut file = fs::File::open(path).map_err(MediaError::disk)?;
    let len = file.metadata().map_err(MediaError::disk)?.len();
    if len > ART_LIMIT || len < 12 {
        return Err(artwork_error());
    }
    let mut header = [0_u8; 32];
    let n = file.read(&mut header).map_err(MediaError::disk)?;
    let head = &header[..n];
    let (extension, dimensions) = if head.starts_with(&[0xff, 0xd8, 0xff]) {
        let mut tail = [0_u8; 2];
        file.seek(SeekFrom::End(-2)).map_err(MediaError::disk)?;
        file.read_exact(&mut tail).map_err(MediaError::disk)?;
        if tail != [0xff, 0xd9] {
            return Err(artwork_error());
        }
        ("jpg", jpeg_file_dimensions(&mut file, len))
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR") && len > 33 {
        let mut tail = [0_u8; 4];
        file.seek(SeekFrom::End(-8)).map_err(MediaError::disk)?;
        file.read_exact(&mut tail).map_err(MediaError::disk)?;
        if tail != *b"IEND" {
            return Err(artwork_error());
        }
        let width = u32::from_be_bytes(head[16..20].try_into().expect("PNG width"));
        let height = u32::from_be_bytes(head[20..24].try_into().expect("PNG height"));
        ("png", Some((width, height)))
    } else if (head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) && len > 13 {
        let mut end = [0_u8; 1];
        file.seek(SeekFrom::End(-1)).map_err(MediaError::disk)?;
        file.read_exact(&mut end).map_err(MediaError::disk)?;
        if end != [0x3b] {
            return Err(artwork_error());
        }
        let width = u16::from_le_bytes([head[6], head[7]]).into();
        let height = u16::from_le_bytes([head[8], head[9]]).into();
        ("gif", Some((width, height)))
    } else if head.starts_with(b"RIFF")
        && head.get(8..12) == Some(b"WEBP")
        && len >= 25
        && u64::from(u32::from_le_bytes(
            head[4..8].try_into().expect("WebP RIFF size"),
        )) + 8
            == len
    {
        ("webp", webp_dimensions(head))
    } else {
        return Err(artwork_error());
    };
    if dimensions.is_none_or(|(width, height)| width == 0 || height == 0) {
        return Err(artwork_error());
    }
    Ok(ImageInfo {
        extension,
        dimensions,
    })
}

fn jpeg_file_dimensions(file: &mut fs::File, len: u64) -> Option<(u32, u32)> {
    let mut index = 2_u64;
    file.seek(SeekFrom::Start(index)).ok()?;
    let mut buf = [0_u8; 1];
    while index + 4 < len {
        file.read_exact(&mut buf).ok()?;
        if buf[0] != 0xff {
            return None;
        }
        index += 1;
        while {
            file.read_exact(&mut buf).ok()?;
            index += 1;
            buf[0] == 0xff
        } {
            if index + 3 >= len {
                return None;
            }
        }
        let marker = buf[0];
        let mut seg = [0_u8; 2];
        file.read_exact(&mut seg).ok()?;
        let length = u64::from(u16::from_be_bytes(seg));
        if length < 2 || index + length > len {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && length >= 7 {
            let mut sof = [0_u8; 5];
            file.read_exact(&mut sof).ok()?;
            let height = u16::from_be_bytes([sof[1], sof[2]]).into();
            let width = u16::from_be_bytes([sof[3], sof[4]]).into();
            return Some((width, height));
        }
        if marker == 0xda {
            return None;
        }
        index += length;
        file.seek(SeekFrom::Start(index)).ok()?;
    }
    None
}

pub(in crate::media) fn image_data_info(data: &[u8]) -> Result<ImageInfo, MediaError> {
    if data.len() > ART_LIMIT as usize {
        return Err(artwork_error());
    }
    let (extension, dimensions) =
        if data.starts_with(&[0xff, 0xd8, 0xff]) && data.ends_with(&[0xff, 0xd9]) {
            ("jpg", jpeg_dimensions(&data))
        } else if data.starts_with(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR")
            && data.len() > 33
            && data[data.len() - 8..data.len() - 4] == *b"IEND"
        {
            let width = u32::from_be_bytes(data[16..20].try_into().expect("PNG width"));
            let height = u32::from_be_bytes(data[20..24].try_into().expect("PNG height"));
            ("png", Some((width, height)))
        } else if (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"))
            && data.len() > 13
            && data.ends_with(&[0x3b])
        {
            let width = u16::from_le_bytes([data[6], data[7]]).into();
            let height = u16::from_le_bytes([data[8], data[9]]).into();
            ("gif", Some((width, height)))
        } else if data.starts_with(b"RIFF")
            && data.get(8..12) == Some(b"WEBP")
            && data.len() >= 25
            && usize::try_from(u32::from_le_bytes(
                data[4..8].try_into().expect("WebP RIFF size"),
            ))
            .ok()
            .and_then(|len| len.checked_add(8))
                == Some(data.len())
        {
            ("webp", webp_dimensions(&data))
        } else {
            return Err(artwork_error());
        };
    if dimensions.is_none_or(|(width, height)| width == 0 || height == 0) {
        return Err(artwork_error());
    }
    Ok(ImageInfo {
        extension,
        dimensions,
    })
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 4 < data.len() {
        if data[index] != 0xff {
            return None;
        }
        index += 1;
        if data[index] == 0xff {
            continue;
        }
        let marker = data[index];
        index += 1;
        let length = usize::from(u16::from_be_bytes([data[index], data[index + 1]]));
        if length < 2 || index + length > data.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && length >= 7 {
            let height = u16::from_be_bytes([data[index + 3], data[index + 4]]).into();
            let width = u16::from_be_bytes([data[index + 5], data[index + 6]]).into();
            return Some((width, height));
        }
        if marker == 0xda {
            return None;
        }
        index += length;
    }
    None
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    match data.get(12..16)? {
        b"VP8X" if data.len() >= 30 => {
            let dimension =
                |index| u32::from_le_bytes([data[index], data[index + 1], data[index + 2], 0]) + 1;
            Some((dimension(24), dimension(27)))
        }
        b"VP8L" if data[20] == 0x2f => {
            let header = u32::from_le_bytes(data[21..25].try_into().ok()?);
            Some(((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1))
        }
        b"VP8 " if data.get(23..26) == Some(&[0x9d, 0x01, 0x2a]) && data.len() >= 30 => {
            let dimension =
                |index| u32::from(u16::from_le_bytes([data[index], data[index + 1]]) & 0x3fff);
            Some((dimension(26), dimension(28)))
        }
        _ => None,
    }
}

pub(super) fn verify_pdf(path: &Path) -> Result<(), MediaError> {
    let invalid = || {
        MediaError::new(
            "booklet_format",
            "The booklet transfer did not contain a complete PDF.",
            true,
        )
    };
    let mut file = fs::File::open(path).map_err(MediaError::disk)?;
    let mut header = [0_u8; 8];
    file.read_exact(&mut header).map_err(|_| invalid())?;
    if !header.starts_with(b"%PDF-")
        || !header[5].is_ascii_digit()
        || header[6] != b'.'
        || !header[7].is_ascii_digit()
    {
        return Err(invalid());
    }
    let tail_length = file.metadata().map_err(MediaError::disk)?.len().min(2048) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))
        .map_err(MediaError::disk)?;
    let mut tail = vec![0_u8; tail_length];
    file.read_exact(&mut tail).map_err(MediaError::disk)?;
    let end = tail
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    if !tail[..end].ends_with(b"%%EOF") {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn extra_directory(path: &Path) -> Result<PathBuf, MediaError> {
    let path = path.to_owned();
    let ancestor = blocking({
        let path = path.clone();
        move || {
            let invalid = || {
                MediaError::new(
                    "output_location",
                    "The asset location is not a safe output folder.",
                    false,
                )
            };
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(invalid());
            }
            let mut closest = None;
            for ancestor in path.ancestors() {
                // A UNC or drive prefix with no root is not a filesystem component.
                if !ancestor.is_absolute() {
                    break;
                }
                match fs::symlink_metadata(ancestor) {
                    Ok(info) => {
                        if is_reparse_or_symlink(&info) || !info.is_dir() {
                            return Err(invalid());
                        }
                        if closest.is_none() {
                            closest = Some(ancestor.to_owned());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(MediaError::disk(error)),
                }
            }
            closest.ok_or_else(invalid)
        }
    })
    .await?;
    safe_directory(&ancestor, &path).await
}
