//! Container fallback.
//!
//! Some producers put metadata in containers we do not parse structurally, or
//! emit malformed XML/CBOR. Scan the head and tail of the file for the few
//! unambiguous AI-provenance strings. This is deliberately narrower than the
//! structured detectors to avoid matching image data.

use crate::detect::Input;
use crate::model::{Confidence, Signal, Source};
use crate::patterns;

const HEAD_BYTES: usize = 4 * 1024 * 1024;
const TAIL_BYTES: usize = 1024 * 1024;

pub fn detect(input: &Input) -> Vec<Signal> {
    if input.bytes.is_empty() {
        return Vec::new();
    }
    let bytes = head_and_tail(input.bytes);
    let text = String::from_utf8_lossy(&bytes);
    let mut signals = Vec::new();

    // Only lower-case/scan for these when a distinctive byte sequence is
    // present; PNG pixel data is otherwise a expensive thing to run regexes on.
    let may_have_source_type = text.contains("Algorithmic")
        || text.contains("algorithmic")
        || text.contains("Synthetic")
        || text.contains("synthetic")
        || text.contains("DataDriven")
        || text.contains("dataDriven");
    if may_have_source_type
        && let Some((needle, weight, description)) = patterns::ai_source_type(&text)
    {
        signals.push(Signal::new(
            Source::Container,
            Confidence::from_weight(weight),
            "container.digital_source_type",
            patterns::truncate(&format!("{description} ({needle})"), 300),
        ));
    }
    if text.contains("AISystemUsed") || text.contains("aisystemused") {
        signals.push(Signal::new(
            Source::Container,
            Confidence::High,
            "container.ai_system_used",
            "container contains AISystemUsed",
        ));
    }
    if text.contains("AIPromptInformation") || text.contains("aipromptinformation") {
        signals.push(Signal::new(
            Source::Container,
            Confidence::High,
            "container.ai_prompt",
            "container contains AIPromptInformation",
        ));
    }

    let is_svg = input.mime.starts_with("image/svg");
    if is_svg {
        if let Some((tool, matched)) = patterns::match_text(&text).first() {
            signals.push(
                Signal::new(
                    Source::Container,
                    Confidence::Medium,
                    "container.svg_tool",
                    patterns::truncate(&format!("SVG mentions {matched}"), 300),
                )
                .with_tool(*tool),
            );
        }
        if let Some(phrase) = patterns::ai_phrase(&text) {
            signals.push(Signal::new(
                Source::Container,
                Confidence::Medium,
                "container.svg_ai_phrase",
                patterns::truncate(&format!("SVG contains {phrase}"), 300),
            ));
        }
        let markers = patterns::prompt_markers(&text);
        if markers.len() >= 2 {
            signals.push(Signal::new(
                Source::Container,
                Confidence::Medium,
                "container.prompt_parameters",
                format!("{} prompt/parameter markers", markers.len()),
            ));
        }
    }

    signals
}

fn head_and_tail(data: &[u8]) -> Vec<u8> {
    if data.len() <= HEAD_BYTES + TAIL_BYTES {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(HEAD_BYTES + TAIL_BYTES + 1);
    out.extend_from_slice(&data[..HEAD_BYTES]);
    out.push(b'\n');
    out.extend_from_slice(&data[data.len() - TAIL_BYTES..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Metadata;

    fn input<'a>(bytes: &'a [u8], mime: &'a str, metadata: &'a Metadata) -> Input<'a> {
        Input {
            bytes,
            mime,
            metadata,
        }
    }

    #[test]
    fn trained_media_is_detected() {
        let metadata = Metadata::default();
        let signals = detect(&input(
            b"\xff\xd8 ... trainedAlgorithmicMedia ...",
            "image/jpeg",
            &metadata,
        ));
        assert!(signals.iter().any(|signal| {
            signal.code == "container.digital_source_type" && signal.confidence == Confidence::High
        }));
    }

    #[test]
    fn ai_system_property_is_detected() {
        let metadata = Metadata::default();
        let signals = detect(&input(b"<x>AISystemUsed</x>", "image/tiff", &metadata));
        assert!(
            signals
                .iter()
                .any(|signal| signal.code == "container.ai_system_used")
        );
    }

    #[test]
    fn svg_tool_mentions_are_detected() {
        let metadata = Metadata::default();
        let signals = detect(&input(
            br#"<svg><metadata>created with Midjourney</metadata></svg>"#,
            "image/svg+xml",
            &metadata,
        ));
        assert!(
            signals
                .iter()
                .any(|signal| signal.tool.as_deref() == Some("Midjourney"))
        );
    }

    #[test]
    fn random_bytes_have_no_signals() {
        let metadata = Metadata::default();
        let signals = detect(&input(
            b"\x00\x01\x02 a normal image",
            "image/jpeg",
            &metadata,
        ));
        assert!(signals.is_empty());
    }
}
