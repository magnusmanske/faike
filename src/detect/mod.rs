//! Detectors.
//!
//! Each module is independent and returns [`Signal`]s. The orchestrator runs
//! them in a fixed order, removes exact duplicates, and keeps the strongest
//! evidence for display.

pub mod c2pa;
pub mod container;
pub mod description;
pub mod exif;
pub mod png;
pub mod watermark;
pub mod xmp;

use crate::config::DetectConfig;
use crate::model::{Signal, dedup};
use std::sync::OnceLock;
use std::time::Instant;

fn debug_enabled() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("AIMER_DEBUG").is_some())
}

macro_rules! timed {
    ($name:expr, $expression:expr) => {{
        let started = Instant::now();
        let result = $expression;
        if debug_enabled() {
            eprintln!(
                "aimer: detector {} -> {} signal(s) in {:?}",
                $name,
                result.len(),
                started.elapsed()
            );
        }
        result
    }};
}

/// MediaWiki metadata that is available without downloading the file.
#[derive(Debug, Default, Clone)]
pub struct Metadata {
    /// `imageinfo` `extmetadata` key/value pairs, with HTML left intact.
    pub extmetadata: Vec<(String, String)>,
    /// Current file-page wikitext.
    pub wikitext: String,
    /// Upload log comment.
    pub comment: String,
}

/// Everything a detector may inspect.
pub struct Input<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
    pub metadata: &'a Metadata,
}

/// Run all enabled detectors.
pub fn detect(input: &Input, config: &DetectConfig) -> Vec<Signal> {
    let mut signals = Vec::new();
    if !input.bytes.is_empty() {
        if config.c2pa {
            signals.extend(timed!("c2pa", c2pa::detect(input)));
        }
        if config.xmp {
            signals.extend(timed!("xmp", xmp::detect(input)));
        }
        if config.exif {
            signals.extend(timed!("exif", exif::detect(input)));
        }
        if config.png_text {
            signals.extend(timed!("png", png::detect(input)));
        }
        // The raw-byte scan would mostly re-find strings the structured
        // detectors above already parsed (C2PA JSON, XMP packets). Run it only
        // when they found nothing, so one piece of evidence counts once.
        if config.container && signals.is_empty() {
            signals.extend(timed!("container", container::detect(input)));
        }
        if config.watermark.enabled {
            signals.extend(timed!(
                "watermark",
                watermark::detect(input, &config.watermark)
            ));
        }
    }
    if config.metadata {
        signals.extend(timed!("description", description::detect(input)));
    }
    dedup(&mut signals);
    signals.sort_by_key(|signal| std::cmp::Reverse(signal.confidence));
    signals.truncate(40);
    signals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, Level, Source, score};

    /// Build a minimal PNG with only the chunks our parser cares about.
    fn png_with_text(keyword: &str, text: &str) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut body = keyword.as_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(text.as_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(b"tEXt");
        out.extend_from_slice(&body);
        out.extend_from_slice(&[0; 4]); // CRC is intentionally ignored
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"IEND");
        out.extend_from_slice(&[0; 4]);
        out
    }

    #[test]
    fn a1111_parameters_are_high_confidence() {
        let bytes = png_with_text(
            "parameters",
            "a cat\nNegative prompt: blurry\nSteps: 30, Sampler: Euler a, CFG scale: 7, Seed: 42",
        );
        let metadata = Metadata::default();
        let input = Input {
            bytes: &bytes,
            mime: "image/png",
            metadata: &metadata,
        };
        let signals = detect(&input, &DetectConfig::default());
        assert!(signals.iter().any(|signal| {
            signal.source == Source::PngText && signal.confidence == Confidence::High
        }));
        assert_eq!(Level::from_score(score(&signals)), Level::High);
    }

    #[test]
    fn ordinary_png_has_no_signals() {
        let bytes = png_with_text("Comment", "a photograph of a cat");
        let metadata = Metadata::default();
        let input = Input {
            bytes: &bytes,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(detect(&input, &DetectConfig::default()).is_empty());
    }

    #[test]
    fn container_is_only_a_fallback() {
        let mut bytes = png_with_text(
            "parameters",
            "a cat\nSteps: 20, Sampler: Euler a, CFG scale: 7, Seed: 1",
        );
        bytes.extend_from_slice(b"trainedAlgorithmicMedia");
        let metadata = Metadata::default();
        let input = Input {
            bytes: &bytes,
            mime: "image/png",
            metadata: &metadata,
        };
        let signals = detect(&input, &DetectConfig::default());
        assert!(
            signals
                .iter()
                .any(|signal| signal.source == Source::PngText)
        );
        assert!(
            !signals
                .iter()
                .any(|signal| signal.source == Source::Container),
            "raw strings already covered by PNG text must not count twice"
        );
    }
}
