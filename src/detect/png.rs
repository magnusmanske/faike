//! PNG text-chunk detection.
//!
//! Stable Diffusion WebUI writes an A1111 `parameters` chunk, ComfyUI writes
//! JSON `prompt`/`workflow` chunks, and various tools write `Software`,
//! `Comment` or `Description`. We parse chunks tolerantly: CRC errors and
//! unknown chunk types must not stop the scan.

use crate::detect::Input;
use crate::model::{Confidence, Signal, Source};
use crate::patterns;

/// Largest text value we inflate or scan.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Largest hex-encoded XMP profile we decode.
const MAX_HEX_XMP: usize = 4 * 1024 * 1024;

pub fn detect(input: &Input) -> Vec<Signal> {
    if !input.bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Vec::new();
    }
    let mut signals = Vec::new();
    let mut position = 8;
    while position + 12 <= input.bytes.len() {
        let Some(length_bytes) = input.bytes.get(position..position + 4) else {
            break;
        };
        let length = u32::from_be_bytes(length_bytes.try_into().unwrap_or([0; 4])) as usize;
        let Some(end) = position
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
        else {
            break;
        };
        if end > input.bytes.len() {
            break;
        }
        let kind = &input.bytes[position + 4..position + 8];
        let body = &input.bytes[position + 8..position + 8 + length];
        match kind {
            b"tEXt" => {
                if let Some((keyword, text)) = parse_text(body) {
                    signals.extend(check_text(&keyword, &text));
                }
            }
            b"zTXt" => {
                if let Some((keyword, text)) = parse_ztxt(body) {
                    signals.extend(check_text(&keyword, &text));
                }
            }
            b"iTXt" => {
                if let Some((keyword, text)) = parse_itxt(body) {
                    signals.extend(check_text(&keyword, &text));
                }
            }
            b"IEND" => break,
            _ => {}
        }
        position = end;
    }
    signals
}

fn split_nul(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let position = bytes.iter().position(|&byte| byte == 0)?;
    Some((&bytes[..position], &bytes[position + 1..]))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

fn parse_text(body: &[u8]) -> Option<(String, String)> {
    let (keyword, text) = split_nul(body)?;
    Some((
        latin1(keyword).trim().to_string(),
        latin1(text).trim_matches('\0').to_string(),
    ))
}

fn parse_ztxt(body: &[u8]) -> Option<(String, String)> {
    let (keyword, rest) = split_nul(body)?;
    let (&method, compressed) = rest.split_first()?;
    if method != 0 {
        return None;
    }
    let text =
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(compressed, MAX_TEXT_BYTES).ok()?;
    Some((
        latin1(keyword).trim().to_string(),
        latin1(&text).trim_matches('\0').to_string(),
    ))
}

fn parse_itxt(body: &[u8]) -> Option<(String, String)> {
    let (keyword, rest) = split_nul(body)?;
    let (&compressed, rest) = rest.split_first()?;
    let (&method, rest) = rest.split_first()?;
    let (_language, rest) = split_nul(rest)?;
    let (_translated, text) = split_nul(rest)?;
    let text = if compressed == 1 {
        if method != 0 {
            return None;
        }
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(text, MAX_TEXT_BYTES).ok()?
    } else {
        text.to_vec()
    };
    Some((
        latin1(keyword).trim().to_string(),
        String::from_utf8_lossy(&text)
            .trim_matches('\0')
            .to_string(),
    ))
}

fn check_text(keyword: &str, text: &str) -> Vec<Signal> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let keyword_lower = keyword.to_lowercase();
    let mut signals = Vec::new();

    match keyword_lower.as_str() {
        "parameters" => {
            let markers = patterns::prompt_markers(text);
            if markers.len() >= 2 {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::High,
                    "png.parameters.prompt",
                    format!(
                        "{keyword}: {} A1111 prompt/parameter markers",
                        markers.len()
                    ),
                ));
            }
            if let Some(signal) =
                tool_signal(keyword, text, Confidence::High, "png.parameters.tool")
            {
                signals.push(signal);
            }
            if let Some(phrase) = patterns::ai_phrase(text) {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::Low,
                    "png.parameters.ai_phrase",
                    patterns::truncate(&format!("{keyword}: {text} (matched {phrase})"), 300),
                ));
            }
        }
        "prompt" | "workflow" => {
            let markers = patterns::prompt_markers(text);
            let comfy = text.contains("\"class_type\"")
                || text.contains("\"KSampler\"")
                || text.contains("\"CheckpointLoaderSimple\"")
                || text.contains("\"nodes\"");
            if markers.len() >= 2 || comfy {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::High,
                    "png.workflow.prompt",
                    format!(
                        "{keyword}: {} prompt/parameter markers, ComfyUI-like JSON: {comfy}",
                        markers.len()
                    ),
                ));
            }
            if let Some(signal) = tool_signal(keyword, text, Confidence::High, "png.workflow.tool")
            {
                signals.push(signal);
            }
        }
        "software" => {
            if let Some(signal) = tool_signal(keyword, text, Confidence::High, "png.software") {
                signals.push(signal);
            } else if let Some(phrase) = patterns::ai_phrase(text) {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::Medium,
                    "png.software.ai_phrase",
                    patterns::truncate(&format!("{keyword}: {text} (matched {phrase})"), 300),
                ));
            }
        }
        "comment" | "description" | "source" | "author" | "title" | "creation time" => {
            if let Some(signal) = tool_signal(keyword, text, Confidence::Medium, "png.text_tool") {
                signals.push(signal);
            } else if let Some(phrase) = patterns::ai_phrase(text) {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::Medium,
                    "png.text.ai_phrase",
                    patterns::truncate(&format!("{keyword}: {text} (matched {phrase})"), 300),
                ));
            }
            if patterns::prompt_markers(text).len() >= 2 {
                signals.push(Signal::new(
                    Source::PngText,
                    Confidence::Medium,
                    "png.text.prompt_parameters",
                    format!("{keyword}: prompt/parameter markers"),
                ));
            }
        }
        "raw profile type xmp" => {
            if let Some(xmp) = decode_hex_profile(text) {
                signals.extend(crate::detect::xmp::scan_text(&xmp));
            }
        }
        _ => {}
    }
    signals
}

fn tool_signal(
    keyword: &str,
    text: &str,
    confidence: Confidence,
    code: &'static str,
) -> Option<Signal> {
    let (tool, matched) = patterns::match_value(text).into_iter().next()?;
    Some(
        Signal::new(
            Source::PngText,
            confidence,
            code,
            patterns::truncate(
                &format!(
                    "{keyword}: {} (matched {matched})",
                    patterns::truncate(text, 220)
                ),
                300,
            ),
        )
        .with_tool(tool),
    )
}

/// Decode the hex-encoded `Raw profile type xmp` chunk used by ImageMagick.
fn decode_hex_profile(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // Skip the profile name and the decimal length header. XMP hex data
        // is long; short all-hex lines are not payload.
        if line.len() < 16 || !line.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let (pairs, _) = line.as_bytes().as_chunks::<2>();
        for pair in pairs {
            let high = (pair[0] as char).to_digit(16)? as u8;
            let low = (pair[1] as char).to_digit(16)? as u8;
            bytes.push((high << 4) | low);
            if bytes.len() > MAX_HEX_XMP {
                break;
            }
        }
        if bytes.len() > MAX_HEX_XMP {
            break;
        }
    }
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&[0; 4]);
        out
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for chunk in chunks {
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    fn text_body(keyword: &str, text: &str) -> Vec<u8> {
        let mut body = keyword.as_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(text.as_bytes());
        body
    }

    #[test]
    fn a1111_parameters_are_high_confidence() {
        let data = png(&[chunk(
            b"tEXt",
            &text_body(
                "parameters",
                "a cat\nNegative prompt: blurry\nSteps: 30, Sampler: Euler a, CFG scale: 7, Seed: 42",
            ),
        )]);
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &data,
            mime: "image/png",
            metadata: &metadata,
        };
        let signals = detect(&input);
        assert!(signals.iter().any(|signal| {
            signal.code == "png.parameters.prompt" && signal.confidence == Confidence::High
        }));
    }

    #[test]
    fn comfyui_workflow_is_high_confidence() {
        let data = png(&[chunk(
            b"tEXt",
            &text_body(
                "workflow",
                r#"{"nodes":[{"class_type":"KSampler","inputs":{"ckpt_name":"sd_xl.safetensors"}}]}"#,
            ),
        )]);
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &data,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(
            detect(&input)
                .iter()
                .any(|signal| signal.code == "png.workflow.prompt"
                    && signal.confidence == Confidence::High)
        );
    }

    #[test]
    fn ztxt_is_inflated() {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(b"Midjourney v6", 6);
        let mut body = text_body("Software", "");
        body.push(0); // compression method
        body.extend_from_slice(&compressed);
        let data = png(&[chunk(b"zTXt", &body)]);
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &data,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(
            detect(&input)
                .iter()
                .any(|signal| signal.code == "png.software"
                    && signal.tool.as_deref() == Some("Midjourney"))
        );
    }

    #[test]
    fn itxt_is_parsed() {
        let mut body = text_body("Software", "");
        body.extend_from_slice(&[0, 0]); // uncompressed, method 0
        body.extend_from_slice(b"\0\0"); // language, translated keyword
        body.extend_from_slice(b"ComfyUI");
        let data = png(&[chunk(b"iTXt", &body)]);
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &data,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(
            detect(&input)
                .iter()
                .any(|signal| signal.confidence == Confidence::High
                    && signal.tool.as_deref() == Some("ComfyUI"))
        );
    }

    #[test]
    fn benign_comment_is_ignored() {
        let data = png(&[chunk(
            b"tEXt",
            &text_body("Comment", "a photograph of a cat"),
        )]);
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &data,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(detect(&input).is_empty());
    }

    #[test]
    fn truncated_chunk_does_not_panic() {
        let data = b"\x89PNG\r\n\x1a\n\xff\xff\xff\xfftEXt";
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: data,
            mime: "image/png",
            metadata: &metadata,
        };
        assert!(detect(&input).is_empty());
    }
}
