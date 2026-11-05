//! MediaWiki-side detection: `extmetadata`, file-page wikitext and the upload
//! comment. These are available without downloading the file and are also the
//! parts an AbuseFilter can see.

use crate::detect::Input;
use crate::model::{Confidence, Signal, Source};
use crate::patterns;

pub fn detect(input: &Input) -> Vec<Signal> {
    let mut signals = Vec::new();
    for (key, raw) in &input.metadata.extmetadata {
        let value = html_to_text(raw);
        if value.is_empty() {
            continue;
        }
        let key_lower = key.to_lowercase();
        match key_lower.as_str() {
            "digitalsourcetype" => {
                if let Some((needle, weight, description)) = patterns::ai_source_type(&value) {
                    signals.push(Signal::new(
                        Source::Metadata,
                        Confidence::from_weight(weight),
                        "description.digital_source_type",
                        patterns::truncate(&format!("{key}: {description} ({needle})"), 300),
                    ));
                }
            }
            "aisystemused" | "aisystemversionused" => {
                let tool = patterns::match_value(&value).first().map(|(tool, _)| *tool);
                let mut signal = Signal::new(
                    Source::Metadata,
                    Confidence::High,
                    "description.ai_system_used",
                    patterns::truncate(&format!("{key}: {value}"), 300),
                );
                if let Some(tool) = tool {
                    signal = signal.with_tool(tool);
                }
                signals.push(signal);
            }
            "aipromptinformation" => {
                signals.push(Signal::new(
                    Source::Metadata,
                    Confidence::High,
                    "description.ai_prompt",
                    patterns::truncate(&format!("{key}: {value}"), 300),
                ));
            }
            "software" => {
                if let Some((tool, matched)) = patterns::match_value(&value).first() {
                    signals.push(
                        Signal::new(
                            Source::Metadata,
                            Confidence::High,
                            "description.software",
                            patterns::truncate(&format!("{key}: {value} (matched {matched})"), 300),
                        )
                        .with_tool(*tool),
                    );
                } else if let Some(phrase) = patterns::ai_phrase(&value) {
                    signals.push(Signal::new(
                        Source::Metadata,
                        Confidence::Medium,
                        "description.software.ai_phrase",
                        patterns::truncate(&format!("{key}: {value} (matched {phrase})"), 300),
                    ));
                }
                if patterns::prompt_markers(&value).len() >= 2 {
                    signals.push(Signal::new(
                        Source::Metadata,
                        Confidence::Medium,
                        "description.software.prompt_parameters",
                        format!("{key}: prompt/parameter markers"),
                    ));
                }
            }
            "imagedescription" | "credit" | "artist" | "objectname" | "categories"
            | "usageterms" | "attribution" | "permission" => {
                if let Some((tool, matched)) = patterns::match_value(&value).first() {
                    signals.push(
                        Signal::new(
                            Source::Metadata,
                            Confidence::Medium,
                            "description.text_tool",
                            patterns::truncate(&format!("{key}: {value} (matched {matched})"), 300),
                        )
                        .with_tool(*tool),
                    );
                }
                if let Some(phrase) = patterns::ai_phrase(&value) {
                    signals.push(Signal::new(
                        Source::Metadata,
                        Confidence::Medium,
                        "description.ai_phrase",
                        patterns::truncate(&format!("{key}: {value} (matched {phrase})"), 300),
                    ));
                }
                if patterns::prompt_markers(&value).len() >= 2 {
                    signals.push(Signal::new(
                        Source::Metadata,
                        Confidence::Medium,
                        "description.prompt_parameters",
                        format!("{key}: prompt/parameter markers"),
                    ));
                }
            }
            _ => {}
        }
    }

    let wikitext = &input.metadata.wikitext;
    if !wikitext.is_empty() {
        signals.extend(scan_text(Source::Wikitext, "wikitext", wikitext));
    }
    let comment = &input.metadata.comment;
    if !comment.is_empty() {
        signals.extend(scan_text(Source::Comment, "comment", comment));
    }
    signals
}

fn scan_text(source: Source, prefix: &str, text: &str) -> Vec<Signal> {
    let mut signals = Vec::new();
    for (label, weight, matched) in patterns::wikitext_markers(text) {
        signals.push(Signal::new(
            source,
            Confidence::from_weight(weight),
            format!("{prefix}.wikitext_marker"),
            patterns::truncate(&format!("{label}: {matched}"), 300),
        ));
    }
    if let Some((tool, matched)) = patterns::match_text(text).first() {
        signals.push(
            Signal::new(
                source,
                if source == Source::Comment {
                    Confidence::Low
                } else {
                    Confidence::Medium
                },
                format!("{prefix}.tool"),
                patterns::truncate(&format!("mentions {matched}"), 300),
            )
            .with_tool(*tool),
        );
    }
    if let Some(phrase) = patterns::ai_phrase(text) {
        signals.push(Signal::new(
            source,
            if source == Source::Comment {
                Confidence::Low
            } else {
                Confidence::Medium
            },
            format!("{prefix}.ai_phrase"),
            patterns::truncate(&format!("contains {phrase}"), 300),
        ));
    }
    if patterns::prompt_markers(text).len() >= 2 {
        signals.push(Signal::new(
            source,
            Confidence::Medium,
            format!("{prefix}.prompt_parameters"),
            "prompt/parameter markers".to_string(),
        ));
    }
    signals
}

/// Strip tags and collapse whitespace from the HTML in `extmetadata`.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Metadata;

    fn input<'a>(metadata: &'a Metadata) -> Input<'a> {
        Input {
            bytes: &[],
            mime: "image/png",
            metadata,
        }
    }

    #[test]
    fn extmetadata_software_is_high_confidence() {
        let metadata = Metadata {
            extmetadata: vec![("Software".into(), "Midjourney v6".into())],
            ..Default::default()
        };
        let signals = detect(&input(&metadata));
        assert!(signals.iter().any(|signal| {
            signal.code == "description.software" && signal.confidence == Confidence::High
        }));
    }

    #[test]
    fn extmetadata_description_ai_phrase_is_medium() {
        let metadata = Metadata {
            extmetadata: vec![(
                "ImageDescription".into(),
                "<span>This is an <b>AI-generated</b> image</span>".into(),
            )],
            ..Default::default()
        };
        let signals = detect(&input(&metadata));
        assert!(
            signals
                .iter()
                .any(|signal| signal.code == "description.ai_phrase"
                    && signal.confidence == Confidence::Medium)
        );
    }

    #[test]
    fn pd_algorithm_template_is_high_confidence() {
        let metadata = Metadata {
            wikitext: "{{Information}}\n{{PD-algorithm}}".into(),
            ..Default::default()
        };
        let signals = detect(&input(&metadata));
        assert!(
            signals
                .iter()
                .any(|signal| signal.code == "wikitext.wikitext_marker"
                    && signal.confidence == Confidence::High)
        );
    }

    #[test]
    fn created_with_template_is_detected() {
        let metadata = Metadata {
            wikitext: "{{Created with Stable Diffusion}}".into(),
            ..Default::default()
        };
        let signals = detect(&input(&metadata));
        assert!(signals.iter().any(|signal| {
            signal.code == "wikitext.wikitext_marker" && signal.detail.contains("Stable Diffusion")
        }));
    }

    #[test]
    fn upload_comment_is_considered_but_weak() {
        let metadata = Metadata {
            comment: "Uploaded with Midjourney".into(),
            ..Default::default()
        };
        let signals = detect(&input(&metadata));
        assert!(signals
            .iter()
            .any(|signal| signal.code == "comment.tool"
                && signal.confidence == Confidence::Low));
    }

    #[test]
    fn benign_metadata_has_no_signals() {
        let metadata = Metadata {
            extmetadata: vec![
                ("Software".into(), "Adobe Photoshop 25.0".into()),
                ("ImageDescription".into(), "A sunset over the sea".into()),
                ("Categories".into(), "Sunsets|Seas".into()),
            ],
            wikitext: "{{Information|Description=A sunset}}".into(),
            comment: "Uploaded with UploadWizard".into(),
        };
        assert!(detect(&input(&metadata)).is_empty());
    }

    #[test]
    fn html_to_text_strips_tags_and_entities() {
        assert_eq!(
            html_to_text("<span class=\"x\">a &amp; b</span><br>c"),
            "a & b c"
        );
    }
}
