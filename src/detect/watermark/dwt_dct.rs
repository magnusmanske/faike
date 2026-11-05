//! Stable Diffusion `invisible-watermark` DWT-DCT payload detection.
//!
//! This follows the `dwtDct` decoder in ShieldMnt's `invisible-watermark`
//! (MIT): the image is converted to YUV, a one-level Haar DWT is applied to the
//! U plane, and the magnitude of the largest non-DC coefficient in each 4×4 LL
//! block votes for one payload bit. The vote is taken modulo the payload
//! length, so an unknown-length mark can still be measured by bit stability.
//!
//! The encoder passes `scales=[0, 36, 36]` but loops over channels `0..2`, so
//! only channel 1 (U) is actually marked. Scanning V as well would add
//! unwatermarked content and dilute the votes, so we match the encoder.
//!
//! The original Stable Diffusion v1 pipeline embedded the ASCII payload
//! `StableDiffusionV1` (136 bits); other tools use different payloads. We test a
//! set of known payloads and report high margin even when the payload is
//! unknown.

use crate::config::WatermarkConfig;
use crate::model::{Confidence, Signal, Source};
use image::RgbImage;
use serde::Serialize;
use std::collections::BTreeSet;

const SCALE: f64 = 36.0;
const BLOCK: usize = 4;
/// The encoder refuses images below 256×256.
const MIN_PIXELS: u64 = 256 * 256;
/// The encoder places the max coefficient at `0.25 * SCALE` or
/// `0.75 * SCALE` within a quantization step. Distance from those centers is a
/// useful presence metric for flat or JPEG-smoothed images, which otherwise
/// produce unanimous zero bits and a misleadingly high stability margin.
const LOCK_TOLERANCE: f64 = 4.0;

/// At least this many block votes per bit are needed for a meaningful average.
const MIN_VOTES_PER_BIT: usize = 4;

#[derive(Debug, Clone, Copy)]
struct BlockVote {
    bit: u8,
    distance: f64,
}

/// Known payloads. Lengths are derived from the byte strings.
const KNOWN_PAYLOADS: &[(&str, &str)] = &[
    ("StableDiffusionV1", "StableDiffusionV1"),
    ("StableDiffusionXL", "StableDiffusionXL"),
    ("SDV1", "SDV1"),
    ("SDXL", "SDXL"),
    ("SD3", "SD3"),
    ("FLUX", "FLUX"),
];

/// Generic lengths to probe when the payload is unknown.
const GENERIC_LENGTHS: &[usize] = &[24, 32, 56, 64, 128, 136, 160, 256];

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub length: usize,
    /// 0 = random, 1 = every block votes unanimously.
    pub margin: f64,
    /// Fraction of block coefficients close to the quantization centers.
    pub locked_fraction: f64,
    pub mean_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub best_length: usize,
    pub best_margin: f64,
    pub best_locked_fraction: f64,
    pub best_mean_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_match: Option<f64>,
    pub candidates: Vec<Candidate>,
}

/// Measure watermark-like periodicity without applying thresholds.
pub fn report(image: &RgbImage) -> Report {
    let (width, height) = image.dimensions();
    let mut report = Report {
        width,
        height,
        skipped: None,
        best_length: 0,
        best_margin: 0.0,
        best_locked_fraction: 0.0,
        best_mean_distance: 0.0,
        best_payload: None,
        best_match: None,
        candidates: Vec::new(),
    };
    if u64::from(width) * u64::from(height) < MIN_PIXELS {
        report.skipped = Some("image is smaller than 256×256".to_string());
        return report;
    }
    let Some(scores) = block_scores(image) else {
        report.skipped = Some("image has no usable 4×4 blocks".to_string());
        return report;
    };
    if scores.is_empty() {
        report.skipped = Some("image has no usable 4×4 blocks".to_string());
        return report;
    }

    let mut lengths: BTreeSet<usize> = GENERIC_LENGTHS.iter().copied().collect();
    for (_, payload) in KNOWN_PAYLOADS {
        lengths.insert(payload.len() * 8);
    }
    for length in lengths {
        let Some((margin, bits, locked_fraction, mean_distance)) = evaluate(&scores, length) else {
            continue;
        };
        let mut best_payload = None;
        let mut best_match: Option<f64> = None;
        for (name, payload) in KNOWN_PAYLOADS {
            if payload.len() * 8 != length {
                continue;
            }
            let expected = payload_bits(payload);
            let matches = bits
                .iter()
                .zip(expected.iter())
                .filter(|(actual, expected)| actual == expected)
                .count();
            let match_score = matches as f64 / length as f64;
            if best_match.is_none_or(|current| match_score > current) {
                best_match = Some(match_score);
                best_payload = Some((*name).to_string());
            }
        }
        report.candidates.push(Candidate {
            length,
            margin,
            locked_fraction,
            mean_distance,
            payload: best_payload,
            match_score: best_match,
        });
    }
    report
        .candidates
        .sort_by(|a, b| b.margin.total_cmp(&a.margin));
    if let Some(best) = report.candidates.first() {
        report.best_length = best.length;
        report.best_margin = best.margin;
        report.best_locked_fraction = best.locked_fraction;
        report.best_mean_distance = best.mean_distance;
    }
    if let Some(best) = report
        .candidates
        .iter()
        .filter(|candidate| candidate.payload.is_some())
        .max_by(|a, b| {
            a.match_score
                .unwrap_or(0.0)
                .total_cmp(&b.match_score.unwrap_or(0.0))
                .then_with(|| a.margin.total_cmp(&b.margin))
        })
    {
        report.best_payload = best.payload.clone();
        report.best_match = best.match_score;
    }
    report
}

/// Apply the configured thresholds to a report.
pub fn detect(image: &RgbImage, config: &WatermarkConfig) -> Vec<Signal> {
    let report = report(image);
    if report.skipped.is_some() {
        return Vec::new();
    }
    let mut signals = Vec::new();
    let known = report
        .candidates
        .iter()
        .filter(|candidate| candidate.margin >= config.dwt_margin_threshold)
        .filter(|candidate| candidate.locked_fraction >= config.dwt_lock_threshold)
        .filter_map(|candidate| {
            candidate
                .payload
                .as_ref()
                .zip(candidate.match_score)
                .map(|(payload, score)| (candidate, payload, score))
        })
        .max_by(|a, b| {
            a.2.total_cmp(&b.2)
                .then_with(|| a.0.margin.total_cmp(&b.0.margin))
        });

    if let Some((candidate, payload, match_score)) = known
        && match_score >= config.dwt_match_threshold
    {
        signals.push(
            Signal::new(
                Source::Watermark,
                Confidence::High,
                "watermark.dwt_dct.payload",
                format!(
                    "DWT-DCT payload {payload}: {:.0}% of {} bits match, margin {:.2}, locked {:.0}%",
                    match_score * 100.0,
                    candidate.length,
                    candidate.margin,
                    candidate.locked_fraction * 100.0
                ),
            )
            .with_tool("Stable Diffusion DWT-DCT"),
        );
    } else if report.best_margin >= config.dwt_generic_margin_threshold
        && report.best_locked_fraction >= config.dwt_lock_threshold
    {
        signals.push(
            Signal::new(
                Source::Watermark,
                Confidence::Medium,
                "watermark.dwt_dct.generic",
                format!(
                    "DWT-DCT watermark-like periodicity: {} bits, margin {:.2}, locked {:.0}%",
                    report.best_length,
                    report.best_margin,
                    report.best_locked_fraction * 100.0
                ),
            )
            .with_tool("DWT-DCT watermark"),
        );
    }
    signals
}

/// Block votes for U, in the order the Python implementation processes them.
fn block_scores(image: &RgbImage) -> Option<Vec<BlockVote>> {
    let (width, height) = image.dimensions();
    let crop_width = (width / 4 * 4) as usize;
    let crop_height = (height / 4 * 4) as usize;
    if crop_width < 8 || crop_height < 8 {
        return None;
    }
    let mut u = vec![0f64; crop_width * crop_height];
    for y in 0..crop_height {
        for x in 0..crop_width {
            let pixel = image.get_pixel(x as u32, y as u32);
            let r = f64::from(pixel[0]);
            let g = f64::from(pixel[1]);
            let b = f64::from(pixel[2]);
            let index = y * crop_width + x;
            // OpenCV converts to uint8 YUV with rounding. Keeping the
            // fractional value shifts many block coefficients across the
            // quantization threshold, so round like the reference decoder.
            u[index] = (-0.14713 * r - 0.28886 * g + 0.436 * b + 128.0)
                .round()
                .clamp(0.0, 255.0);
        }
    }
    let (u_ll, u_width, u_height) = haar_ll(&u, crop_width, crop_height)?;
    let mut scores = Vec::new();
    collect_block_scores(&u_ll, u_width, u_height, &mut scores);
    Some(scores)
}

/// One-level 2D Haar low-pass: `(a+b+c+d)/2` for each 2×2 block.
fn haar_ll(plane: &[f64], width: usize, height: usize) -> Option<(Vec<f64>, usize, usize)> {
    if width < 2 || height < 2 || plane.len() < width * height {
        return None;
    }
    let out_width = width / 2;
    let out_height = height / 2;
    let mut out = vec![0f64; out_width * out_height];
    for y in 0..out_height {
        let row0 = 2 * y * width;
        let row1 = row0 + width;
        for x in 0..out_width {
            let column = 2 * x;
            out[y * out_width + x] = (plane[row0 + column]
                + plane[row0 + column + 1]
                + plane[row1 + column]
                + plane[row1 + column + 1])
                * 0.5;
        }
    }
    Some((out, out_width, out_height))
}

fn collect_block_scores(ll: &[f64], width: usize, height: usize, out: &mut Vec<BlockVote>) {
    let blocks_x = width / BLOCK;
    let blocks_y = height / BLOCK;
    for block_y in 0..blocks_y {
        for block_x in 0..blocks_x {
            let mut max_abs = -1.0;
            let mut max_index = 0;
            for y in 0..BLOCK {
                for x in 0..BLOCK {
                    if x == 0 && y == 0 {
                        continue;
                    }
                    let index = (block_y * BLOCK + y) * width + block_x * BLOCK + x;
                    let absolute = ll[index].abs();
                    if absolute > max_abs {
                        max_abs = absolute;
                        max_index = index;
                    }
                }
            }
            let value = ll[max_index].abs();
            let remainder = value % SCALE;
            let distance = (remainder - 0.25 * SCALE)
                .abs()
                .min((remainder - 0.75 * SCALE).abs());
            out.push(BlockVote {
                bit: u8::from(remainder > 0.5 * SCALE),
                distance,
            });
        }
    }
}

/// Average block votes per bit index, plus locked-coefficient statistics.
fn evaluate(scores: &[BlockVote], length: usize) -> Option<(f64, Vec<u8>, f64, f64)> {
    if length == 0 || scores.len() / length < MIN_VOTES_PER_BIT {
        return None;
    }
    let mut sums = vec![0f64; length];
    let mut counts = vec![0usize; length];
    let mut locked = 0usize;
    let mut distance_sum = 0.0;
    for (index, score) in scores.iter().enumerate() {
        let bit = index % length;
        sums[bit] += f64::from(score.bit);
        counts[bit] += 1;
        distance_sum += score.distance;
        if score.distance <= LOCK_TOLERANCE {
            locked += 1;
        }
    }
    let mut margin = 0.0;
    let mut bits = Vec::with_capacity(length);
    for (sum, count) in sums.iter().zip(counts.iter()) {
        let average = sum / *count as f64;
        margin += (average - 0.5).abs() * 2.0;
        bits.push(u8::from(average > 0.5));
    }
    let total = scores.len() as f64;
    Some((
        margin / length as f64,
        bits,
        locked as f64 / total,
        distance_sum / total,
    ))
}

/// `np.unpackbits`: most-significant bit first.
fn payload_bits(payload: &str) -> Vec<u8> {
    payload
        .bytes()
        .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haar_low_pass_of_a_constant_plane() {
        let plane = vec![100.0; 8 * 8];
        let (ll, width, height) = haar_ll(&plane, 8, 8).unwrap();
        assert_eq!((width, height), (4, 4));
        assert!(ll.iter().all(|value| (*value - 200.0).abs() < f64::EPSILON));
    }

    fn votes(bits: &[u8], distance: f64) -> Vec<BlockVote> {
        bits.iter()
            .map(|bit| BlockVote {
                bit: *bit,
                distance,
            })
            .collect()
    }

    #[test]
    fn unanimous_votes_have_full_margin() {
        let payload = payload_bits("SDV1");
        let bits: Vec<u8> = payload.iter().copied().cycle().take(3200).collect();
        let (margin, decoded, locked, _) = evaluate(&votes(&bits, 0.0), payload.len()).unwrap();
        assert_eq!(margin, 1.0);
        assert_eq!(decoded, payload);
        assert_eq!(locked, 1.0);
    }

    #[test]
    fn alternating_votes_have_zero_margin() {
        // Within each bit index, zeros and ones are equally represented.
        let length = 32;
        let bits: Vec<u8> = (0..3200)
            .map(|index| ((index / length) % 2) as u8)
            .collect();
        let (margin, _, _, _) = evaluate(&votes(&bits, 0.0), length).unwrap();
        assert!(margin < 1e-9);
    }

    #[test]
    fn small_images_are_skipped() {
        let image = RgbImage::new(100, 100);
        assert!(report(&image).skipped.is_some());
    }

    #[test]
    fn payload_bits_are_big_endian() {
        assert_eq!(payload_bits("A"), vec![0, 1, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn known_payload_is_recovered_from_blocks() {
        // Build a synthetic LL plane where every block encodes SDV1.
        let payload = payload_bits("SDV1");
        let width = 128;
        let height = 128;
        let mut ll = vec![0.0; width * height];
        let blocks_x = width / BLOCK;
        let blocks_y = height / BLOCK;
        for block_y in 0..blocks_y {
            for block_x in 0..blocks_x {
                let bit = payload[(block_y * blocks_x + block_x) % payload.len()];
                let value = if bit == 1 { 36.0 * 7.75 } else { 36.0 * 7.25 };
                let index = (block_y * BLOCK + 1) * width + block_x * BLOCK + 1;
                ll[index] = value;
            }
        }
        let mut scores = Vec::new();
        collect_block_scores(&ll, width, height, &mut scores);
        let (margin, bits, locked, _) = evaluate(&scores, payload.len()).unwrap();
        assert_eq!(bits, payload);
        assert_eq!(margin, 1.0);
        assert_eq!(locked, 1.0);
    }

    /// Build an RGB image whose YUV U plane carries the payload. This is a
    /// test-only encoder using flat 2×2 blocks, which preserves the LL plane.
    fn watermarked_image(payload: &str, size: u32) -> RgbImage {
        let ll_width = (size / 2) as usize;
        let ll_height = (size / 2) as usize;
        let bits = payload_bits(payload);
        let mut ll = vec![256.0; ll_width * ll_height]; // constant U = 128
        let blocks_x = ll_width / BLOCK;
        let blocks_y = ll_height / BLOCK;
        for block_y in 0..blocks_y {
            for block_x in 0..blocks_x {
                let bit = bits[(block_y * blocks_x + block_x) % bits.len()];
                let value = if bit == 1 { 36.0 * 7.75 } else { 36.0 * 7.25 };
                let index = (block_y * BLOCK + 1) * ll_width + block_x * BLOCK + 1;
                ll[index] = value;
            }
        }
        let mut u = vec![0f64; (size * size) as usize];
        let mut v = vec![0f64; (size * size) as usize];
        for y in 0..size as usize {
            for x in 0..size as usize {
                let value = ll[(y / 2) * ll_width + x / 2] / 2.0;
                u[y * size as usize + x] = value;
                v[y * size as usize + x] = value;
            }
        }
        RgbImage::from_fn(size, size, |x, y| {
            let index = y as usize * size as usize + x as usize;
            let y_value = 128.0;
            let u_value = u[index] - 128.0;
            let v_value = v[index] - 128.0;
            let r = (y_value + 1.13983 * v_value).clamp(0.0, 255.0);
            let g = (y_value - 0.39465 * u_value - 0.58060 * v_value).clamp(0.0, 255.0);
            let b = (y_value + 2.03211 * u_value).clamp(0.0, 255.0);
            image::Rgb([r as u8, g as u8, b as u8])
        })
    }

    #[test]
    fn constant_image_is_not_a_watermark() {
        let image = RgbImage::from_pixel(512, 512, image::Rgb([128, 128, 128]));
        let report = report(&image);
        assert!(report.best_locked_fraction < 0.2, "{report:?}");
        assert!(
            detect(&image, &WatermarkConfig::default()).is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn end_to_end_watermark_report() {
        let image = watermarked_image("SDV1", 512);
        let report = report(&image);
        assert!(report.skipped.is_none(), "{report:?}");
        assert_eq!(report.best_payload.as_deref(), Some("SDV1"), "{report:?}");
        assert!(report.best_match.unwrap_or(0.0) > 0.8, "{report:?}");
        assert!(report.best_margin > 0.5, "{report:?}");
    }

    #[test]
    fn unwatermarked_jpeg_has_no_signal() {
        // JPEG smoothing creates flat low-frequency blocks; the locked-coefficient
        // guard must prevent those from looking like a watermark.
        let image = RgbImage::from_fn(512, 512, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let decoded = image::load_from_memory(encoded.get_ref())
            .unwrap()
            .to_rgb8();
        assert!(detect(&decoded, &WatermarkConfig::default()).is_empty());
    }
}
