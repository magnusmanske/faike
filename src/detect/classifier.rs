//! Optional ONNX AI-image classifier.
//!
//! This is the content-level fallback for files whose C2PA/XMP/EXIF and
//! watermarks have been stripped. A classifier is probabilistic evidence, so it
//! reports **Medium** confidence at most; it must never drive a `disallow`.
//!
//! Two independently trained models can be configured. With
//! `require_agreement`, both must cross their own thresholds; that is the main
//! defence against a single model's confident mistakes on retouched photos.
//! `scripts/fetch-classifier.sh` downloads both models; they are not bundled.
//!
//! The models are:
//! * primary: Swin, 224×224, ImageNet normalization, label 0 = artificial,
//!   `SMOGY-Ai-images-detector` (CC-BY-NC-4.0);
//! * secondary: ViT-Small/16, 384×384, shortest-edge 440 + centre crop, CLIP
//!   normalization, single logit, `fev`/Community Forensics (MIT).

use crate::config::ClassifierConfig;
use crate::detect::Input;
use crate::model::Signal;
use image::DynamicImage;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ModelReport {
    pub name: String,
    pub available: bool,
    pub threshold: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability: Option<f32>,
    pub models: Vec<ModelReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(feature = "classifier")]
mod enabled {
    use super::*;
    use crate::config::{ClassifierModelConfig, Normalization};
    use crate::model::{Confidence, Source};
    use image::GenericImageView;
    use image::imageops::FilterType;
    use ndarray::Array4;
    use ort::session::{Session, builder::GraphOptimizationLevel};
    use ort::value::{TensorValueType, Value};
    use std::sync::{Mutex, OnceLock};

    const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];
    const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
    const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];

    struct LoadedModel {
        name: &'static str,
        config: ClassifierModelConfig,
        session: Mutex<Session>,
    }

    struct Runtime {
        primary: LoadedModel,
        secondary: Option<LoadedModel>,
    }

    static MODEL: OnceLock<Option<Runtime>> = OnceLock::new();

    fn debug() -> bool {
        std::env::var_os("FAIKE_DEBUG").is_some()
    }

    fn load_model(name: &'static str, config: &ClassifierModelConfig) -> Option<LoadedModel> {
        if !config.model_path.exists() {
            if debug() {
                eprintln!(
                    "faike: {name} classifier disabled; missing {} (run scripts/fetch-classifier.sh)",
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
                        "faike: {name} classifier model loaded from {}",
                        config.model_path.display()
                    );
                }
                Some(LoadedModel {
                    name,
                    config: config.clone(),
                    session: Mutex::new(session),
                })
            }
            None => {
                eprintln!(
                    "faike: {name} classifier model failed to load: {}",
                    config.model_path.display()
                );
                None
            }
        }
    }

    fn runtime(config: &ClassifierConfig) -> Option<&'static Runtime> {
        MODEL
            .get_or_init(|| {
                let primary = load_model("primary", &config.primary)?;
                let secondary = config
                    .secondary
                    .as_ref()
                    .and_then(|secondary| load_model("secondary", secondary));
                Some(Runtime { primary, secondary })
            })
            .as_ref()
    }

    /// Resize/crop to the model input and normalize.
    fn tensor(
        image: &DynamicImage,
        model: &ClassifierModelConfig,
    ) -> Option<Value<TensorValueType<f32>>> {
        let size = model.input_size;
        let cropped = match model.resize_shortest {
            Some(shortest) => {
                let (width, height) = image.dimensions();
                let scale = shortest as f32 / width.min(height) as f32;
                let width = ((width as f32 * scale).round() as u32).max(1);
                let height = ((height as f32 * scale).round() as u32).max(1);
                if width < size || height < size {
                    return None;
                }
                image
                    .resize_exact(width, height, FilterType::CatmullRom)
                    .crop_imm((width - size) / 2, (height - size) / 2, size, size)
            }
            None => image.resize_to_fill(size, size, FilterType::Triangle),
        };
        let rgb = cropped.to_rgb8();
        let (mean, std) = match model.normalization {
            Normalization::Imagenet => (IMAGENET_MEAN, IMAGENET_STD),
            Normalization::Clip => (CLIP_MEAN, CLIP_STD),
        };
        let size = size as usize;
        let mut data = vec![0f32; 3 * size * size];
        for (x, y, pixel) in rgb.enumerate_pixels() {
            let index = y as usize * size + x as usize;
            for channel in 0..3 {
                data[channel * size * size + index] =
                    (f32::from(pixel[channel]) / 255.0 - mean[channel]) / std[channel];
            }
        }
        let array = Array4::from_shape_vec((1, 3, size, size), data).ok()?;
        Value::from_array(array).ok()
    }

    /// Softmax for logits, or sigmoid for a single-logit model.
    fn probability(output: &[f32], ai_label: Option<usize>) -> Option<f32> {
        if output.is_empty() {
            return None;
        }
        if output.len() == 1 {
            return Some(1.0 / (1.0 + (-output[0]).exp()));
        }
        let max = output.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let sum: f32 = output.iter().map(|value| (value - max).exp()).sum();
        let index = ai_label.unwrap_or(0).min(output.len() - 1);
        Some((output[index] - max).exp() / sum)
    }

    fn score(model: &LoadedModel, image: &DynamicImage) -> Result<f32, String> {
        let input = tensor(image, &model.config).ok_or("preprocessing failed")?;
        let mut session = model
            .session
            .lock()
            .map_err(|_| "session is poisoned".to_string())?;
        let outputs = session
            .run(ort::inputs![input])
            .map_err(|error| error.to_string())?;
        let output = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|error| error.to_string())?;
        let values: Vec<f32> = output.iter().copied().collect();
        probability(&values, model.config.ai_label).ok_or_else(|| "empty output".to_string())
    }

    pub fn detect(input: &Input, config: &ClassifierConfig) -> Vec<Signal> {
        if !input.mime.starts_with("image/") {
            return Vec::new();
        }
        let Some(image) = crate::detect::watermark::load_image(input.bytes) else {
            return Vec::new();
        };
        // Skip inputs the models handle badly: tiny images and extreme banners.
        let (width, height) = image.dimensions();
        let (small, large) = (width.min(height), width.max(height));
        if small < 128 || (large as f32 / small as f32) > 4.0 {
            return Vec::new();
        }

        let Some(runtime) = runtime(config) else {
            return Vec::new();
        };
        let primary = match score(&runtime.primary, &image) {
            Ok(probability) => probability,
            Err(error) => {
                if debug() {
                    eprintln!("faike: primary classifier: {error}");
                }
                return Vec::new();
            }
        };
        let mut accepted = f64::from(primary) >= runtime.primary.config.threshold;
        let mut detail = format!(
            "{}: {:.0}% (≥{:.0}%)",
            runtime.primary.name,
            primary * 100.0,
            runtime.primary.config.threshold * 100.0
        );

        if config.secondary.is_some() {
            match &runtime.secondary {
                Some(secondary) => match score(secondary, &image) {
                    Ok(probability) => {
                        let secondary_ok = f64::from(probability) >= secondary.config.threshold;
                        accepted = if config.require_agreement {
                            accepted && secondary_ok
                        } else {
                            accepted || secondary_ok
                        };
                        detail.push_str(&format!(
                            ", {}: {:.0}% (≥{:.0}%)",
                            secondary.name,
                            probability * 100.0,
                            secondary.config.threshold * 100.0
                        ));
                    }
                    Err(error) => {
                        if debug() {
                            eprintln!("faike: secondary classifier: {error}");
                        }
                        if config.require_agreement {
                            accepted = false;
                        }
                    }
                },
                None => {
                    if config.require_agreement {
                        if debug() {
                            eprintln!(
                                "faike: secondary classifier configured but unavailable; suppressing the classifier"
                            );
                        }
                        return Vec::new();
                    }
                }
            }
        }

        if !accepted {
            return Vec::new();
        }
        vec![
            Signal::new(
                Source::Classifier,
                Confidence::Medium,
                "classifier.ai_generated",
                format!("AI image classifier: {detail}"),
            )
            .with_tool("AI image classifier"),
        ]
    }

    pub fn report(image: &DynamicImage, config: &ClassifierConfig) -> Report {
        let Some(runtime) = runtime(config) else {
            return Report {
                available: false,
                probability: None,
                models: Vec::new(),
                error: Some("primary classifier model unavailable".to_string()),
            };
        };
        let mut models: Vec<ModelReport> = Vec::new();
        let (probability, error) = match score(&runtime.primary, image) {
            Ok(probability) => (Some(probability), None),
            Err(error) => (None, Some(error)),
        };
        models.push(ModelReport {
            name: runtime.primary.name.to_string(),
            available: true,
            threshold: runtime.primary.config.threshold,
            probability,
            error,
        });
        if let Some(secondary_config) = &config.secondary {
            match &runtime.secondary {
                Some(secondary) => {
                    let (probability, error) = match score(secondary, image) {
                        Ok(probability) => (Some(probability), None),
                        Err(error) => (None, Some(error)),
                    };
                    models.push(ModelReport {
                        name: secondary.name.to_string(),
                        available: true,
                        threshold: secondary.config.threshold,
                        probability,
                        error,
                    });
                }
                None => models.push(ModelReport {
                    name: "secondary".to_string(),
                    available: false,
                    threshold: secondary_config.threshold,
                    probability: None,
                    error: Some(format!("missing {}", secondary_config.model_path.display())),
                }),
            }
        }
        Report {
            available: true,
            probability: models.first().and_then(|model| model.probability),
            models,
            error: None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn softmax_sums_to_one() {
            let total: f32 = (0..3)
                .map(|index| probability(&[1.0, 2.0, 3.0], Some(index)).unwrap())
                .sum();
            assert!((total - 1.0).abs() < 1e-5);
        }

        #[test]
        fn single_logit_uses_sigmoid() {
            assert!((probability(&[0.0], None).unwrap() - 0.5).abs() < 1e-6);
            assert!(probability(&[10.0], None).unwrap() > 0.99);
        }

        #[test]
        fn tensor_has_the_expected_shape_in_both_modes() {
            let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                100,
                60,
                image::Rgb([255, 255, 255]),
            ));
            for model in [
                ClassifierModelConfig {
                    input_size: 32,
                    ..ClassifierModelConfig::default()
                },
                ClassifierModelConfig {
                    input_size: 32,
                    resize_shortest: Some(40),
                    normalization: Normalization::Clip,
                    ..ClassifierModelConfig::default()
                },
            ] {
                let tensor = tensor(&image, &model).unwrap();
                let array = tensor.try_extract_array::<f32>().unwrap();
                assert_eq!(array.shape(), &[1, 3, 32, 32]);
            }
        }

        #[test]
        fn model_is_used_when_present() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models/ai-image-detector.onnx");
            if !path.exists() {
                return;
            }
            let config = ClassifierConfig {
                primary: ClassifierModelConfig {
                    model_path: path,
                    ..ClassifierModelConfig::default()
                },
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
            models: Vec::new(),
            error: Some("faike was built without the `classifier` feature".to_string()),
        }
    }
}

#[cfg(not(feature = "classifier"))]
pub use disabled::{detect, report};
#[cfg(feature = "classifier")]
pub use enabled::{detect, report};
