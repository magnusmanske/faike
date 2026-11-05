//! Adobe TrustMark neural watermark decoding.
//!
//! TrustMark is a general provenance watermark used by Content Credentials and
//! some AI tools. A decoded mark is strong evidence that a watermark is
//! present, but not proof that the image was AI-generated: camera images can
//! carry TrustMark too. The detector therefore reports medium confidence and
//! includes the decoded payload for a reviewer.
//!
//! Models are not bundled. Run `scripts/fetch-models.sh` or point
//! `watermark.models_dir` at a directory containing `encoder_Q.onnx` and
//! `decoder_Q.onnx`. When the models are missing, the detector is skipped.

use crate::config::WatermarkConfig;
use crate::model::Signal;
use image::DynamicImage;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(feature = "trustmark")]
mod enabled {
    use super::*;
    use crate::model::{Confidence, Source};
    use std::sync::{Arc, OnceLock};
    use trustmark::{Trustmark, Variant, Version};

    static MODEL: OnceLock<Option<Arc<Trustmark>>> = OnceLock::new();

    fn parse_version(value: &str) -> Option<(Version, usize)> {
        match value.to_ascii_lowercase().as_str() {
            "bchsuper" | "super" => Some((Version::BchSuper, 40)),
            "bch5" | "5" => Some((Version::Bch5, 61)),
            "bch4" | "4" => Some((Version::Bch4, 68)),
            "bch3" | "3" => Some((Version::Bch3, 75)),
            _ => None,
        }
    }

    fn model(config: &WatermarkConfig) -> Option<&'static Arc<Trustmark>> {
        MODEL
            .get_or_init(|| {
                let variant = config.trustmark_variant.parse::<Variant>().ok()?;
                let Some((version, _)) = parse_version(&config.trustmark_version) else {
                    eprintln!(
                        "faike: TrustMark disabled; unknown version {:?}",
                        config.trustmark_version
                    );
                    return None;
                };
                let encoder = config.models_dir.join(format!("encoder_{variant}.onnx"));
                let decoder = config.models_dir.join(format!("decoder_{variant}.onnx"));
                if !encoder.exists() || !decoder.exists() {
                    eprintln!(
                        "faike: TrustMark disabled; missing {} or {} (run scripts/fetch-models.sh)",
                        encoder.display(),
                        decoder.display()
                    );
                    return None;
                }
                match Trustmark::new(&config.models_dir, variant, version) {
                    Ok(model) => {
                        if std::env::var_os("FAIKE_DEBUG").is_some() {
                            eprintln!(
                                "faike: TrustMark model loaded from {}",
                                config.models_dir.display()
                            );
                        }
                        Some(Arc::new(model))
                    }
                    Err(error) => {
                        eprintln!("faike: TrustMark model failed to load: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// The Rust TrustMark crate falls back to other BCH schemas when the
    /// declared schema does not decode. That makes random images look like
    /// low-error-correction watermarks, so only accept the configured schema's
    /// payload width. A decoded 75-bit payload while configured for BCH5 is a
    /// fallback, not a BCH5 mark.
    fn accepted_payload(payload: &str, expected_bits: usize) -> bool {
        payload.len() == expected_bits && payload.bytes().any(|byte| byte == b'1')
    }

    pub fn detect(image: DynamicImage, config: &WatermarkConfig) -> Vec<Signal> {
        let Some(model) = model(config) else {
            return Vec::new();
        };
        let Some((_, expected_bits)) = parse_version(&config.trustmark_version) else {
            return Vec::new();
        };
        match model.decode(image) {
            Ok(payload) if accepted_payload(&payload, expected_bits) => vec![
                Signal::new(
                    Source::Watermark,
                    Confidence::Medium,
                    "watermark.trustmark",
                    format!(
                        "TrustMark watermark ({}), payload {}",
                        config.trustmark_version,
                        crate::patterns::truncate(&payload, 120)
                    ),
                )
                .with_tool("TrustMark"),
            ],
            Ok(payload) => {
                if std::env::var_os("FAIKE_DEBUG").is_some() {
                    eprintln!(
                        "faike: TrustMark decoded a {}-bit payload (expected {expected_bits}); ignored",
                        payload.len()
                    );
                }
                Vec::new()
            }
            Err(trustmark::Error::CorruptWatermark) => Vec::new(),
            Err(error) => {
                if std::env::var_os("FAIKE_DEBUG").is_some() {
                    eprintln!("faike: TrustMark decode failed: {error}");
                }
                Vec::new()
            }
        }
    }

    pub fn report(image: DynamicImage, config: &WatermarkConfig) -> Report {
        let Some(model) = model(config) else {
            return Report {
                available: false,
                payload: None,
                error: Some("TrustMark models unavailable".to_string()),
            };
        };
        let Some((_, expected_bits)) = parse_version(&config.trustmark_version) else {
            return Report {
                available: true,
                payload: None,
                error: Some(format!(
                    "unknown TrustMark version {:?}",
                    config.trustmark_version
                )),
            };
        };
        match model.decode(image) {
            Ok(payload) if accepted_payload(&payload, expected_bits) => Report {
                available: true,
                payload: Some(payload),
                error: None,
            },
            Ok(payload) => Report {
                available: true,
                payload: None,
                error: Some(format!(
                    "decoded {} bits, expected {expected_bits} for {}; ignored",
                    payload.len(),
                    config.trustmark_version
                )),
            },
            Err(trustmark::Error::CorruptWatermark) => Report {
                available: true,
                payload: None,
                error: None,
            },
            Err(error) => Report {
                available: true,
                payload: None,
                error: Some(error.to_string()),
            },
        }
    }

    /// Only used by calibration tests to create a watermarked image.
    #[cfg(test)]
    pub fn encode_for_test(
        image: DynamicImage,
        payload: &str,
        models_dir: &std::path::Path,
    ) -> Option<DynamicImage> {
        let model = Trustmark::new(models_dir, Variant::Q, Version::Bch5).ok()?;
        model.encode(payload.to_string(), image, 0.95).ok()
    }
}

#[cfg(feature = "trustmark")]
pub use enabled::{detect, report};

#[cfg(not(feature = "trustmark"))]
pub fn detect(_image: DynamicImage, _config: &WatermarkConfig) -> Vec<Signal> {
    Vec::new()
}

#[cfg(not(feature = "trustmark"))]
pub fn report(_image: DynamicImage, _config: &WatermarkConfig) -> Report {
    Report {
        available: false,
        payload: None,
        error: Some("faike was built without the `trustmark` feature".to_string()),
    }
}

#[cfg(all(test, feature = "trustmark"))]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::path::PathBuf;

    fn models_dir() -> PathBuf {
        PathBuf::from("models")
    }

    fn models_available() -> bool {
        models_dir().join("encoder_Q.onnx").exists() && models_dir().join("decoder_Q.onnx").exists()
    }

    fn test_image() -> DynamicImage {
        let image = RgbImage::from_fn(512, 512, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        DynamicImage::ImageRgb8(image)
    }

    #[test]
    fn encoded_image_is_detected_if_models_are_present() {
        if !models_available() {
            return;
        }
        let Some(encoded) = enabled::encode_for_test(
            test_image(),
            "1011011110011000111111000000011111011111011100000110110110111",
            &models_dir(),
        ) else {
            return;
        };
        let config = WatermarkConfig {
            models_dir: models_dir(),
            ..Default::default()
        };
        let signals = detect(encoded, &config);
        assert!(!signals.is_empty(), "no signal");
        assert!(
            signals
                .iter()
                .any(|signal| signal.code == "watermark.trustmark"),
            "{signals:?}"
        );
    }
}
