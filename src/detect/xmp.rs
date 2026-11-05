//! XMP / IPTC Extension detection.
//!
//! XMP is plain XML embedded in JPEG APP1, PNG iTXt, WebP, TIFF, HEIF and
//! sidecar-ish containers. We locate packets by their XML markers and parse
//! them with a read-only XML parser. A raw fallback keeps working when a
//! producer emits malformed XML.

use crate::detect::Input;
use crate::model::{Confidence, Signal, Source};
use crate::patterns;
use std::collections::HashSet;

/// Largest XMP packet we are willing to parse.
const MAX_PACKET_BYTES: usize = 512 * 1024;
const MAX_PACKETS: usize = 8;
/// Largest individual property value we keep for a detail message.
const MAX_VALUE_CHARS: usize = 1000;
/// Cap property extraction; some editors write enormous history lists.
const MAX_PROPERTIES: usize = 200;

const STRUCTURED_KEYS: &[&str] = &[
    "software",
    "softwareagent",
    "creatortool",
    "processingsoftware",
    "make",
    "model",
    "hostcomputer",
];

const DESCRIPTION_KEYS: &[&str] = &[
    "description",
    "caption",
    "title",
    "keywords",
    "creator",
    "credit",
    "rights",
    "usageterms",
    "history",
];

pub fn detect(input: &Input) -> Vec<Signal> {
    let mut signals = Vec::new();
    for packet in packets(input.bytes) {
        signals.extend(scan_text(&packet));
    }
    signals
}

/// Scan one XMP packet. Also used for XMP stored in PNG text chunks.
pub(crate) fn scan_text(text: &str) -> Vec<Signal> {
    let mut signals = Vec::new();
    let mut properties = extract_properties(text);
    // Avoid duplicate attribute/element representations.
    let mut seen = HashSet::new();
    properties
        .retain(|(key, label, value)| seen.insert((key.clone(), label.clone(), value.clone())));

    let mut has_digital_source = false;
    let mut has_ai_system = false;
    let mut has_ai_prompt = false;
    let mut has_tool = false;
    let mut has_phrase = false;
    let mut has_parameters = false;

    for (key, label, value) in &properties {
        let value = patterns::truncate(value, MAX_VALUE_CHARS);
        match key.as_str() {
            "digitalsourcetype" => {
                if let Some((needle, weight, description)) = patterns::ai_source_type(&value) {
                    signals.push(Signal::new(
                        Source::Xmp,
                        Confidence::from_weight(weight),
                        "xmp.digital_source_type",
                        patterns::truncate(&format!("{label}: {description} ({needle})"), 300),
                    ));
                    has_digital_source = true;
                }
            }
            "aisystemused" | "aisystemversionused" => {
                let tool = patterns::match_value(&value).first().map(|(tool, _)| *tool);
                let mut signal = Signal::new(
                    Source::Xmp,
                    Confidence::High,
                    "xmp.ai_system_used",
                    patterns::truncate(&format!("{label}: {value}"), 300),
                );
                if let Some(tool) = tool {
                    signal = signal.with_tool(tool);
                }
                signals.push(signal);
                has_ai_system = true;
            }
            "aipromptinformation" => {
                signals.push(Signal::new(
                    Source::Xmp,
                    Confidence::High,
                    "xmp.ai_prompt",
                    patterns::truncate(&format!("{label}: {value}"), 300),
                ));
                has_ai_prompt = true;
            }
            key if STRUCTURED_KEYS.contains(&key) => {
                if let Some((tool, matched)) = patterns::match_value(&value).first() {
                    signals.push(
                        Signal::new(
                            Source::Xmp,
                            Confidence::Medium,
                            "xmp.software",
                            patterns::truncate(
                                &format!("{label}: {value} (matched {matched})"),
                                300,
                            ),
                        )
                        .with_tool(*tool),
                    );
                    has_tool = true;
                } else if let Some(phrase) = patterns::ai_phrase(&value) {
                    signals.push(Signal::new(
                        Source::Xmp,
                        Confidence::Medium,
                        "xmp.ai_phrase",
                        patterns::truncate(&format!("{label}: {value} (matched {phrase})"), 300),
                    ));
                    has_phrase = true;
                }
            }
            key if DESCRIPTION_KEYS.contains(&key) => {
                if let Some((tool, matched)) = patterns::match_value(&value).first() {
                    signals.push(
                        Signal::new(
                            Source::Xmp,
                            Confidence::Low,
                            "xmp.description",
                            patterns::truncate(
                                &format!("{label}: {value} (matched {matched})"),
                                300,
                            ),
                        )
                        .with_tool(*tool),
                    );
                    has_tool = true;
                }
                if let Some(phrase) = patterns::ai_phrase(&value) {
                    signals.push(Signal::new(
                        Source::Xmp,
                        Confidence::Medium,
                        "xmp.ai_phrase",
                        patterns::truncate(&format!("{label}: {value} (matched {phrase})"), 300),
                    ));
                    has_phrase = true;
                }
                if patterns::prompt_markers(&value).len() >= 2 {
                    signals.push(Signal::new(
                        Source::Xmp,
                        Confidence::Medium,
                        "xmp.prompt_parameters",
                        patterns::truncate(&format!("{label}: prompt/parameter markers"), 300),
                    ));
                    has_parameters = true;
                }
            }
            _ => {}
        }
    }

    // Fallback for producers whose key spelling we do not enumerate, or for
    // values nested below an unusual structure.
    if !has_digital_source
        && let Some((needle, weight, description)) = patterns::ai_source_type(text)
    {
        signals.push(Signal::new(
            Source::Xmp,
            Confidence::from_weight(weight),
            "xmp.digital_source_type_raw",
            patterns::truncate(&format!("{description} ({needle})"), 300),
        ));
    }
    if !has_ai_system && text.to_lowercase().contains("aisystemused") {
        signals.push(Signal::new(
            Source::Xmp,
            Confidence::High,
            "xmp.ai_system_used_raw",
            "XMP contains AISystemUsed",
        ));
    }
    if !has_ai_prompt && text.to_lowercase().contains("aipromptinformation") {
        signals.push(Signal::new(
            Source::Xmp,
            Confidence::High,
            "xmp.ai_prompt_raw",
            "XMP contains AIPromptInformation",
        ));
    }
    if !has_tool && let Some((tool, matched)) = patterns::match_text(text).first() {
        signals.push(
            Signal::new(
                Source::Xmp,
                Confidence::Low,
                "xmp.tool_reference",
                patterns::truncate(&format!("XMP mentions {matched}"), 300),
            )
            .with_tool(*tool),
        );
    }
    if !has_phrase && let Some(phrase) = patterns::ai_phrase(text) {
        signals.push(Signal::new(
            Source::Xmp,
            Confidence::Medium,
            "xmp.ai_phrase_raw",
            patterns::truncate(&format!("XMP contains {phrase}"), 300),
        ));
    }
    if !has_parameters {
        let markers = patterns::prompt_markers(text);
        if markers.len() >= 2 {
            signals.push(Signal::new(
                Source::Xmp,
                Confidence::Medium,
                "xmp.prompt_parameters_raw",
                format!("{} prompt/parameter markers", markers.len()),
            ));
        }
    }
    signals
}

/// Extract `(lowercase key, label, value)` triples from an XMP packet.
fn extract_properties(text: &str) -> Vec<(String, String, String)> {
    let Ok(document) = roxmltree::Document::parse(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for node in document.descendants().filter(|node| node.is_element()) {
        if out.len() >= MAX_PROPERTIES {
            break;
        }
        let label = node.tag_name().name();
        let key = label.to_lowercase();
        let is_rdf_container = node
            .tag_name()
            .namespace()
            .is_some_and(|namespace| namespace.contains("22-rdf-syntax-ns"));
        // `rdf:Description` carries the properties as attributes; its text
        // content is the whole packet, not a description property.
        if is_relevant_key(&key) && !is_rdf_container {
            let value = node_text(node);
            if !value.is_empty() {
                out.push((key, label.to_string(), value));
            }
        }
        for attribute in node.attributes() {
            if out.len() >= MAX_PROPERTIES {
                break;
            }
            let label = attribute.name();
            let key = label.to_lowercase();
            if is_relevant_key(&key) {
                out.push((key, label.to_string(), attribute.value().to_string()));
            }
        }
    }
    out
}

fn is_relevant_key(key: &str) -> bool {
    matches!(
        key,
        "digitalsourcetype" | "aisystemused" | "aisystemversionused" | "aipromptinformation"
    ) || STRUCTURED_KEYS.contains(&key)
        || DESCRIPTION_KEYS.contains(&key)
}

/// Concatenate an element's text content, with a hard size cap.
fn node_text(node: roxmltree::Node) -> String {
    let mut out = String::new();
    for descendant in node.descendants() {
        if let Some(text) = descendant.text() {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
            if out.len() >= MAX_VALUE_CHARS {
                break;
            }
        }
    }
    out
}

/// Find XMP packets in raw bytes.
fn packets(data: &[u8]) -> Vec<String> {
    for (start_marker, end_marker) in [
        (&b"<x:xmpmeta"[..], &b"</x:xmpmeta>"[..]),
        (&b"<xmpmeta"[..], &b"</xmpmeta>"[..]),
        (&b"<rdf:RDF"[..], &b"</rdf:RDF>"[..]),
    ] {
        let mut out = Vec::new();
        let mut position = 0;
        while out.len() < MAX_PACKETS {
            let Some(relative) = memchr::memmem::find(&data[position..], start_marker) else {
                break;
            };
            let start = position + relative;
            let Some(end) = packet_end(data, start, end_marker) else {
                break;
            };
            out.push(String::from_utf8_lossy(&data[start..end]).into_owned());
            position = end;
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

fn packet_end(data: &[u8], start: usize, marker: &[u8]) -> Option<usize> {
    let limit = (start + MAX_PACKET_BYTES).min(data.len());
    memchr::memmem::find(&data[start..limit], marker)
        .map(|relative| start + relative + marker.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xmp(description: &str) -> String {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:Iptc4xmpExt="http://iptc.org/std/Iptc4xmpExt/2008-02-29/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/"
      {description}/>
  </rdf:RDF>
</x:xmpmeta>"#
        )
    }

    #[test]
    fn digital_source_type_is_high_confidence() {
        let packet = xmp(
            r#"Iptc4xmpExt:DigitalSourceType="http://cv.iptc.org/newscodes/digitalsourcetype/trainedAlgorithmicMedia""#,
        );
        let signals = scan_text(&packet);
        assert!(signals.iter().any(|signal| {
            signal.code == "xmp.digital_source_type" && signal.confidence == Confidence::High
        }));
    }

    #[test]
    fn composite_source_type_is_not_reduced() {
        let packet = xmp(
            r#"Iptc4xmpExt:DigitalSourceType="http://cv.iptc.org/newscodes/digitalsourcetype/compositeWithTrainedAlgorithmicMedia""#,
        );
        let signals = scan_text(&packet);
        assert!(signals.iter().any(|signal| {
            signal.code == "xmp.digital_source_type"
                && signal
                    .detail
                    .contains("compositeWithTrainedAlgorithmicMedia")
        }));
    }

    #[test]
    fn creator_tool_is_medium_confidence() {
        let packet = xmp(r#"xmp:CreatorTool="Midjourney 6.0""#);
        let signals = scan_text(&packet);
        assert!(signals.iter().any(|signal| {
            signal.code == "xmp.software"
                && signal.confidence == Confidence::Medium
                && signal.tool.as_deref() == Some("Midjourney")
        }));
    }

    #[test]
    fn explicit_ai_system_metadata_is_high_confidence() {
        let packet = xmp(r#"Iptc4xmpExt:AISystemUsed="Firefly""#);
        let signals = scan_text(&packet);
        assert!(signals.iter().any(|signal| {
            signal.code == "xmp.ai_system_used" && signal.confidence == Confidence::High
        }));
    }

    #[test]
    fn benign_xmp_has_no_signals() {
        let packet = xmp(r#"xmp:CreatorTool="Adobe Photoshop 25.0" dc:format="image/jpeg""#);
        assert!(scan_text(&packet).is_empty());
    }

    #[test]
    fn finds_packets_in_binary_bytes() {
        let packet = xmp(
            r#"Iptc4xmpExt:DigitalSourceType="http://cv.iptc.org/newscodes/digitalsourcetype/trainedAlgorithmicMedia""#,
        );
        let mut bytes = b"\xff\xd8some jpeg header".to_vec();
        bytes.extend_from_slice(packet.as_bytes());
        bytes.extend_from_slice(b"\xff\xd9");
        let metadata = crate::detect::Metadata::default();
        let input = Input {
            bytes: &bytes,
            mime: "image/jpeg",
            metadata: &metadata,
        };
        assert!(!detect(&input).is_empty());
    }
}
