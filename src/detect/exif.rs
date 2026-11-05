//! EXIF / TIFF detection.
//!
//! `nom-exif` gives us a pure-Rust parser covering JPEG, PNG, WebP, TIFF,
//! HEIF/AVIF and the common RAW formats. We only inspect fields that carry
//! software/creator intent; camera fields like exposure time are not useful.

use crate::detect::Input;
use crate::model::{Confidence, Signal, Source};
use crate::patterns;
use nom_exif::{EntryValue, Exif, ExifTag, ImageMetadata, MediaParser, MediaSource};

pub fn detect(input: &Input) -> Vec<Signal> {
    let source = match MediaSource::from_memory(bytes::Bytes::copy_from_slice(input.bytes)) {
        Ok(source) => source,
        Err(_) => return Vec::new(),
    };
    let mut parser = MediaParser::new();
    let metadata = match parser.parse_image_metadata(source) {
        Ok(metadata) => metadata,
        Err(_) => return Vec::new(),
    };
    let metadata: ImageMetadata<Exif> = metadata.into();
    let Some(exif) = metadata.exif else {
        return Vec::new();
    };
    let mut signals = Vec::new();
    for (tag, label, weight, code) in [
        (ExifTag::Software, "Software", 85, "exif.software"),
        (
            ExifTag::ProcessingSoftware,
            "ProcessingSoftware",
            80,
            "exif.processing_software",
        ),
        (ExifTag::Make, "Make", 65, "exif.make"),
        (ExifTag::Model, "Model", 65, "exif.model"),
        (
            ExifTag::HostComputer,
            "HostComputer",
            60,
            "exif.host_computer",
        ),
        (ExifTag::Copyright, "Copyright", 55, "exif.copyright"),
        (
            ExifTag::ImageDescription,
            "ImageDescription",
            55,
            "exif.description",
        ),
        (ExifTag::UserComment, "UserComment", 60, "exif.user_comment"),
    ] {
        let Some(value) = exif.get(tag) else {
            continue;
        };
        let Some(value) = entry_string(value) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        signals.extend(scan_value(label, weight, code, value));
    }
    signals
}

/// Shape flags used by the generator-fingerprint heuristic.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    /// A camera maker or model is present.
    pub camera: bool,
    /// A `Software` or `ProcessingSoftware` tag is present.
    pub software: bool,
}

/// Parse just the flags the fingerprint detector needs. Returns the default
/// (no camera, no software) when the file has no parseable EXIF.
pub fn flags(bytes: &[u8]) -> Flags {
    let Ok(source) = MediaSource::from_memory(bytes::Bytes::copy_from_slice(bytes)) else {
        return Flags::default();
    };
    let Ok(metadata) = MediaParser::new().parse_image_metadata(source) else {
        return Flags::default();
    };
    let metadata: ImageMetadata<Exif> = metadata.into();
    let Some(exif) = metadata.exif else {
        return Flags::default();
    };
    Flags {
        camera: exif.get(ExifTag::Make).is_some() || exif.get(ExifTag::Model).is_some(),
        software: exif.get(ExifTag::Software).is_some()
            || exif.get(ExifTag::ProcessingSoftware).is_some(),
    }
}

/// Inspect one EXIF field.
fn scan_value(label: &str, weight: u8, code: &'static str, value: &str) -> Vec<Signal> {
    let mut signals = Vec::new();
    if let Some((tool, matched)) = patterns::match_value(value).first() {
        signals.push(
            Signal::new(
                Source::Exif,
                Confidence::from_weight(weight),
                code,
                patterns::truncate(&format!("{label}: {value} (matched {matched})"), 300),
            )
            .with_tool(*tool),
        );
        return signals;
    }
    if let Some(phrase) = patterns::ai_phrase(value) {
        signals.push(Signal::new(
            Source::Exif,
            Confidence::from_weight(weight.min(55)),
            format!("{code}.ai_phrase"),
            patterns::truncate(&format!("{label}: {value} (matched {phrase})"), 300),
        ));
        return signals;
    }
    let markers = patterns::prompt_markers(value);
    if markers.len() >= 2 {
        signals.push(Signal::new(
            Source::Exif,
            Confidence::Medium,
            format!("{code}.prompt_parameters"),
            format!("{label}: {} prompt/parameter markers", markers.len()),
        ));
    }
    signals
}

/// Convert an EXIF value to text, including the common comment encodings.
fn entry_string(value: &EntryValue) -> Option<String> {
    match value {
        EntryValue::Text(text) => Some(text.clone()),
        EntryValue::Undefined(bytes) | EntryValue::U8Array(bytes) => Some(decode_undefined(bytes)),
        EntryValue::U32Array(values) => Some(
            values
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        EntryValue::DateTime(datetime) => Some(datetime.to_string()),
        EntryValue::NaiveDateTime(datetime) => Some(datetime.to_string()),
        _ => value.as_str().map(str::to_string),
    }
}

/// Decode the character-set prefix used by EXIF `UserComment`.
fn decode_undefined(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if let Some(rest) = bytes.strip_prefix(b"ASCII\0\0\0") {
        return String::from_utf8_lossy(rest).trim_matches('\0').to_string();
    }
    if let Some(rest) = bytes.strip_prefix(b"UNICODE\0") {
        return decode_utf16(rest);
    }
    String::from_utf8_lossy(bytes)
        .trim_matches('\0')
        .to_string()
}

fn decode_utf16(bytes: &[u8]) -> String {
    let (little_endian, body) = match bytes {
        [0xFE, 0xFF, rest @ ..] => (false, rest),
        [0xFF, 0xFE, rest @ ..] => (true, rest),
        _ => (true, bytes),
    };
    let (pairs, _) = body.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
        .trim_matches('\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_user_comment_is_decoded() {
        let mut bytes = b"ASCII\0\0\0".to_vec();
        bytes.extend_from_slice(b"Midjourney v6");
        assert_eq!(decode_undefined(&bytes), "Midjourney v6");
    }

    #[test]
    fn little_endian_unicode_comment_is_decoded() {
        let mut bytes = b"UNICODE\0".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        for unit in "DALL·E".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_utf16(&bytes[8..]), "DALL·E");
    }

    #[test]
    fn software_field_is_detected() {
        let signals = scan_value("Software", 85, "exif.software", "Stable Diffusion XL");
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].confidence, Confidence::High);
        assert_eq!(signals[0].tool.as_deref(), Some("Stable Diffusion"));
    }

    #[test]
    fn camera_software_is_ignored() {
        assert!(scan_value("Software", 85, "exif.software", "Adobe Photoshop 25.0").is_empty());
        assert!(scan_value("Make", 65, "exif.make", "Canon").is_empty());
    }

    #[test]
    fn prompt_parameters_in_user_comment_are_detected() {
        let signals = scan_value(
            "UserComment",
            60,
            "exif.user_comment",
            "Negative prompt: blurry, Steps: 30, Sampler: Euler a, Seed: 42",
        );
        assert!(
            signals
                .iter()
                .any(|signal| signal.code.ends_with("prompt_parameters"))
        );
    }
}
