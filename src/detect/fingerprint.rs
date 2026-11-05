//! Generator-fingerprint heuristics.
//!
//! When all provenance metadata has been stripped, a few properties still hint
//! at a diffusion model: exact output dimensions that generators use, no
//! camera EXIF, no software tag, and no PNG text chunks. This is deliberately a
//! **low-confidence** signal; screenshots and diagrams can look similar. It
//! runs only when the structured detectors found nothing, and the web UI can
//! hide it with `min_score` above 30.

use crate::detect::{Input, exif, png};
use crate::model::{Confidence, Signal, Source};
use image::ImageReader;
use std::io::Cursor;

/// Exact output sizes used by common text-to-image tools. Deliberately
/// conservative: a broad "multiple of 64" rule also matches 4K photos and
/// screenshots, so it is not used.
const GENERATOR_SIZES: &[(u32, u32)] = &[
    // Stable Diffusion 1.x / 2.x and common web UI presets
    (512, 512),
    (512, 768),
    (768, 512),
    (768, 768),
    (640, 512),
    (512, 640),
    // SDXL
    (1024, 1024),
    (1152, 896),
    (896, 1152),
    (1216, 832),
    (832, 1216),
    (1344, 768),
    (768, 1344),
    (1536, 640),
    (640, 1536),
    // DALL-E 3
    (1792, 1024),
    (1024, 1792),
    // Midjourney
    (1456, 816),
    (816, 1456),
    (2048, 2048),
    // Large upscales and 16:9 outputs
    (2048, 1024),
    (1024, 2048),
    (3072, 3072),
    (1536, 768),
    (768, 1536),
    (1408, 768),
    (768, 1408),
];

pub fn detect(input: &Input) -> Vec<Signal> {
    if !input.mime.starts_with("image/") {
        return Vec::new();
    }
    let Some((width, height)) = dimensions(input.bytes) else {
        return Vec::new();
    };
    if !is_generator_size(width, height) {
        return Vec::new();
    }
    let flags = exif::flags(input.bytes);
    if flags.camera || flags.software {
        return Vec::new();
    }
    if png::has_text_chunks(input.bytes) {
        return Vec::new();
    }
    vec![
        Signal::new(
            Source::Fingerprint,
            Confidence::Low,
            "fingerprint.generator_dimensions",
            format!(
                "{width}×{height}, a common generator output; no camera/software metadata or PNG text"
            ),
        )
        .with_tool("generator-size heuristic"),
    ]
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

/// Exact known sizes. Screen sizes such as 1920×1080, 1280×768 and 4096×2304
/// are intentionally absent.
fn is_generator_size(width: u32, height: u32) -> bool {
    GENERATOR_SIZES.contains(&(width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Metadata;
    use image::{ImageFormat, RgbImage};
    use std::io::Cursor;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, image::Rgb([120, 130, 140]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn exact_generator_size_is_recognised() {
        let metadata = Metadata::default();
        let bytes = png(1024, 1024);
        let input = Input {
            bytes: &bytes,
            mime: "image/png",
            metadata: &metadata,
        };
        let signals = detect(&input);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].confidence, Confidence::Low);
        assert_eq!(signals[0].code, "fingerprint.generator_dimensions");
    }

    #[test]
    fn ordinary_screen_size_is_ignored() {
        assert!(!is_generator_size(1920, 1080));
        assert!(!is_generator_size(1280, 768));
        assert!(!is_generator_size(800, 600));
        assert!(!is_generator_size(4096, 2304));
    }

    #[test]
    fn generator_sizes_are_recognised() {
        assert!(is_generator_size(2048, 1024));
        assert!(is_generator_size(1536, 768));
        assert!(!is_generator_size(1024, 768));
    }
}
