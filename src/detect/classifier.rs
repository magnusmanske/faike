//! Optional ONNX AI-image classifier.
//!
//! This is the content-level fallback for files whose C2PA/XMP/EXIF and
//! watermarks have been stripped. A classifier is probabilistic evidence, so it
//! reports **Medium** confidence at most and only above a configurable
//! threshold; it must never drive a `disallow`.
//!
//! The model is not bundled. `scripts/fetch-classifier.sh` downloads the
//! Swin-based `SMOGY-Ai-images-detector` ONNX export (label 0 = artificial,
//! label 1 = human), which uses ImageNet normalization and 224×224 input.
//! Other single-image classification models with the same layout work too; set
//! `classifier.ai_label` and `classifier.threshold` accordingly.

use crate::config::ClassifierConfig;
use crate::detect::Input;
use crate::model::Signal;
use image::DynamicImage;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(feature = "classifier")]
mod enabled {
    use super::*;
    use crate::model::{Confidence, Source};
    use image::GenericImageView;
    use ndarray::Array4;
    use ort::session::{Session, builder::GraphOptimizationLevel};
    use ort::value::{TensorValueType, Value};
    use std::sync::{Arc, Mutex, OnceLock};

    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];

    static MODEL: OnceLock<Option<Arc<Mutex<Session>>>> = OnceLock::new();

    fn debug() -> bool {
        std::env::var_os("FAIKE_DEBUG").is_some()
    }

    fn model(config: &ClassifierConfig) -> Option<&'static Arc<Mutex<Session>>> {
        MODEL
            .get_or_init(|| {
                if !config.model_path.exists() {
                    if debug() {
                        eprintln!(
                            "faike: classifier disabled; missing {} (run scripts/fetch-classifier.sh)",
                            config.model_path.display()
                        );
                    }
                    return None;
                }
                let session = Session::builder()
                    .ok()
                    .and_then(|builder| {
                        builder
                            .with_optimization_level(GraphOptimizationLevel::Level3)
                            .ok()
                    })
                    .and_then(|builder| builder.with_intra_threads(2).ok())
                    .and_then(|builder| builder.commit_from_file(&config.model_path).ok());
                match session {
                    Some(session) => {
                        if debug() {
                            eprintln!(
                                "faike: classifier model loaded from {}",
                                config.model_path.display()
                            );
                        }
                        Some(Arc::new(Mutex::new(session)))
                    }
                    None => {
                        eprintln!(
                            "faike: classifier model failed to load: {}",
                            config.model_path.display()
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Resize/crop to the model input and normalize with ImageNet statistics.
    fn tensor(image: &DynamicImage, size: u32) -> Option<Value<TensorValueType<f32>>> {
        let size = size as usize;
        let rgb = image
            .resize_to_fill(
                size as u32,
                size as u32,
                image::imageops::FilterType::Triangle,
            )
            .to_rgb8();
        let mut data = vec![0f32; 3 * size * size];
        for (x, y, pixel) in rgb.enumerate_pixels() {
            let index = y as usize * size + x as usize;
            for channel in 0..3 {
                data[channel * size * size + index] =
                    (f32::from(pixel[channel]) / 255.0 - MEAN[channel]) / STD[channel];
            }
        }
        let array = Array4::from_shape_vec((1, 3, size, size), data).ok()?;
        Value::from_array(array).ok()
    }

    /// Softmax for logits, or sigmoid for a single-logit model.
    fn probability(output: &[f32], ai_label: usize) -> Option<f32> {
        if output.is_empty() {
            return None;
        }
        if output.len() == 1 {
            return Some(1.0 / (1.0 + (-output[0]).exp()));
        }
        let max = output.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let sum: f32 = output.iter().map(|value| (value - max).exp()).sum();
        let index = ai_label.min(output.len() - 1);
        Some((output[index] - max).exp() / sum)
    }

    fn score(image: &DynamicImage, config: &ClassifierConfig) -> Result<f32, String> {
        let model = model(config).ok_or("classifier model unavailable")?;
        let input = tensor(image, config.input_size).ok_or("classifier preprocessing failed")?;
        let mut session = model
            .lock()
            .map_err(|_| "classifier session is poisoned".to_string())?;
        let outputs = session
            .run(ort::inputs![input])
            .map_err(|error| error.to_string())?;
        let output = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|error| error.to_string())?;
        let values: Vec<f32> = output.iter().copied().collect();
        probability(&values, config.ai_label)
            .ok_or_else(|| "classifier output was empty".to_string())
    }

    pub fn detect(input: &Input, config: &ClassifierConfig) -> Vec<Signal> {
        if !input.mime.starts_with("image/") {
            return Vec::new();
        }
        let Some(image) = crate::detect::watermark::load_image(input.bytes) else {
            return Vec::new();
        };
        // A classifier on a 17:1 banner or a camera photo is usually wrong:
        // the first is squeezed into the model input, the second already has
        // trustworthy provenance that the model tends to override.
        let (width, height) = image.dimensions();
        let (small, large) = (width.min(height), width.max(height));
        if small < 128 || (large as f32 / small as f32) > 4.0 {
            return Vec::new();
        }
        if crate::detect::exif::flags(input.bytes).camera {
            return Vec::new();
        }
        let probability = match score(&image, config) {
            Ok(probability) => probability,
            Err(error) => {
                if debug() {
                    eprintln!("faike: classifier: {error}");
                }
                return Vec::new();
            }
        };
        if f64::from(probability) < config.threshold {
            return Vec::new();
        }
        vec![
            Signal::new(
                Source::Classifier,
                Confidence::Medium,
                "classifier.ai_generated",
                format!(
                    "AI image classifier: {:.0}% (threshold {:.0}%)",
                    probability * 100.0,
                    config.threshold * 100.0
                ),
            )
            .with_tool("AI image classifier"),
        ]
    }

    pub fn report(image: &DynamicImage, config: &ClassifierConfig) -> Report {
        match score(image, config) {
            Ok(probability) => Report {
                available: true,
                probability: Some(probability),
                error: None,
            },
            Err(error) => Report {
                available: model(config).is_some(),
                probability: None,
                error: Some(error),
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn softmax_sums_to_one() {
            let probability = probability(&[1.0, 2.0, 3.0], 0).unwrap();
            assert!((0.0..=1.0).contains(&probability));
            let total: f32 = [1.0, 2.0, 3.0]
                .iter()
                .enumerate()
                .map(|(index, _)| probability_of(&[1.0, 2.0, 3.0], index))
                .sum();
            assert!((total - 1.0).abs() < 1e-5);
        }

        fn probability_of(output: &[f32], index: usize) -> f32 {
            let max = output.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let sum: f32 = output.iter().map(|value| (value - max).exp()).sum();
            (output[index] - max).exp() / sum
        }

        #[test]
        fn single_logit_uses_sigmoid() {
            assert!((probability(&[0.0], 0).unwrap() - 0.5).abs() < 1e-6);
            assert!(probability(&[10.0], 0).unwrap() > 0.99);
        }

        #[test]
        fn tensor_has_the_expected_shape() {
            let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                10,
                10,
                image::Rgb([255, 255, 255]),
            ));
            let tensor = tensor(&image, 8).unwrap();
            let array = tensor.try_extract_array::<f32>().unwrap();
            assert_eq!(array.shape(), &[1, 3, 8, 8]);
        }

        #[test]
        fn model_is_used_when_present() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models/ai-image-detector.onnx");
            if !path.exists() {
                return;
            }
            let config = ClassifierConfig {
                model_path: path,
                ..ClassifierConfig::default()
            };
            let image = image::open(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/c2pa-firefly.jpg"),
            )
            .unwrap();
            let report = report(&image, &config);
            assert!(report.available, "{report:?}");
            let probability = report.probability.unwrap();
            assert!((0.0..=1.0).contains(&probability), "{report:?}");
        }
    }
}

#[cfg(not(feature = "classifier"))]
mod disabled {
    use super::*;

    pub fn detect(_input: &Input, _config: &ClassifierConfig) -> Vec<Signal> {
        Vec::new()
    }

    pub fn report(_image: &DynamicImage, _config: &ClassifierConfig) -> Report {
        Report {
            available: false,
            probability: None,
            error: Some("faike was built without the `classifier` feature".to_string()),
        }
    }
}

#[cfg(not(feature = "classifier"))]
pub use disabled::{detect, report};
#[cfg(feature = "classifier")]
pub use enabled::{detect, report};
