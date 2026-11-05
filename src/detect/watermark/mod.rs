//! Pixel-level watermark detection.
//!
//! Two complementary detectors:
//!
//! * Stable Diffusion's `invisible-watermark` `dwtDct` payload. This is a
//!   fragile spatial watermark, but when present it is a direct link to the
//!   Stable Diffusion v1 pipeline.
//! * Adobe TrustMark, a neural watermark used by Content Credentials. The
//!   model is optional and loaded from disk; see `scripts/fetch-models.sh`.
//!
//! Watermark presence is evidence of provenance, not automatically evidence of
//! AI generation. The TrustMark signal is therefore medium confidence.

pub mod dwt_dct;
pub mod trustmark;

use crate::config::WatermarkConfig;
use crate::detect::Input;
use crate::model::Signal;
use image::DynamicImage;
use std::io::Cursor;

/// Refuse to decode absurdly large images during pixel analysis.
const MAX_PIXELS: u64 = 24_000_000;

pub fn detect(input: &Input, config: &WatermarkConfig) -> Vec<Signal> {
    if !config.enabled || !input.mime.starts_with("image/") {
        return Vec::new();
    }
    let Some(image) = load_image(input.bytes) else {
        return Vec::new();
    };
    let mut signals = Vec::new();
    if config.dwt_dct {
        signals.extend(dwt_dct::detect(&image.to_rgb8(), config));
    }
    if config.trustmark {
        signals.extend(trustmark::detect(image, config));
    }
    signals
}

/// Decode an image once, with a pixel-count guard.
pub fn load_image(bytes: &[u8]) -> Option<DynamicImage> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    image::load_from_memory(bytes).ok()
}
