//! AbuseFilter rule generation.
//!
//! AbuseFilter cannot inspect embedded EXIF/XMP/C2PA data. What it *can* see
//! for an upload is the description wikitext (`added_lines`), the upload
//! summary, and the file metadata variables (`file_sha1`, `file_mime`, ...).
//! The generated rules therefore cover the two practical cases:
//!
//! 1. AI tools, prompts and AI templates named in the upload description.
//! 2. Byte-exact matches of files this tool flagged with high confidence.
//!
//! Neither is suitable for `disallow`: metadata can be stripped and a
//! re-encoded file has a different SHA-1. The suggestions use `warn`/`tag`.

use crate::model::Detection;
use crate::patterns;
use serde::Serialize;
use std::collections::HashSet;

/// Only include this many hashes; filters are not a database.
const MAX_HASHES: usize = 200;
/// Hashes per filter, to keep each expression readable.
const HASHES_PER_RULE: usize = 50;
const ACTIONS: &[&str] = &["warn", "tag"];

#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub name: String,
    pub description: String,
    pub expression: String,
    pub actions: Vec<String>,
    pub notes: String,
}

/// Build AbuseFilter suggestions. `min_score` is the minimum detection score
/// for the exact-SHA1 rules.
pub fn rules(detections: &[Detection], min_score: u8) -> Vec<Rule> {
    let mut rules = vec![
        text_rule(
            "AI tool named in upload",
            "Upload description or summary names a known AI generator.",
            patterns::abuse_tool_regex(),
            "High precision for distinctive vendor names. Review the hits; do not disallow on this alone.",
        ),
        text_rule(
            "AI prompt/parameters in upload",
            "Upload description contains Stable Diffusion/ComfyUI-style prompt parameters.",
            patterns::abuse_prompt_regex(),
            "Prompt parameters are strong evidence of a local generator, but can also appear in documentation. Use warn/tag.",
        ),
        text_rule(
            "AI template or category in upload",
            "Upload description uses an AI-labelling template or category.",
            patterns::abuse_wikitext_regex(),
            "This is usually an explicit declaration by the uploader.",
        ),
        text_rule(
            "Explicit AI statement in upload",
            "Upload description says the file was AI-generated/created.",
            patterns::abuse_phrase_regex(),
            "Covers generic statements that do not name a vendor.",
        ),
    ];

    let mut seen_hashes = HashSet::new();
    let mut hashes: Vec<String> = detections
        .iter()
        .filter(|detection| detection.score >= min_score)
        .filter(|detection| !detection.sha1.is_empty())
        .map(|detection| detection.sha1.to_lowercase())
        .filter(|hash| seen_hashes.insert(hash.clone()))
        .collect();
    hashes.truncate(MAX_HASHES);

    let chunks: Vec<&[String]> = hashes.chunks(HASHES_PER_RULE).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let comparisons: Vec<String> = chunk.iter().map(|hash| format!("\"{hash}\"")).collect();
        let expression = format!(
            "(action == \"upload\" | action == \"stashupload\")\n& equals_to_any(file_sha1, {})",
            comparisons.join(", ")
        );
        let suffix = if chunks.len() > 1 {
            format!(" ({}/{})", index + 1, chunks.len())
        } else {
            String::new()
        };
        rules.push(Rule {
            name: format!("Known AI file SHA-1{suffix}"),
            description: "Exact byte-for-byte match of a file aimer flagged as AI-generated."
                .to_string(),
            expression,
            actions: ACTIONS.iter().map(|action| (*action).to_string()).collect(),
            notes: "Exact SHA-1s only catch identical files; re-encoding or editing changes the hash. Use warn/tag, never disallow."
                .to_string(),
        });
    }
    rules
}

fn text_rule(name: &str, description: &str, regex: &str, notes: &str) -> Rule {
    let pattern = abuse_filter_string(regex);
    let expression = format!(
        "action == \"upload\"\n& page_namespace == 6\n& (\n    added_lines irlike \"{pattern}\"\n    | summary irlike \"{pattern}\"\n)"
    );
    Rule {
        name: name.to_string(),
        description: description.to_string(),
        expression,
        actions: ACTIONS.iter().map(|action| (*action).to_string()).collect(),
        notes: notes.to_string(),
    }
}

/// Escape a regex for use inside a double-quoted AbuseFilter string.
///
/// AbuseFilter's string syntax uses PHP-style escapes, so every backslash in
/// the regex must be doubled; quotes must be escaped too.
fn abuse_filter_string(regex: &str) -> String {
    regex.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Human-readable form for copy/paste into the AbuseFilter editor.
pub fn text(rules: &[Rule]) -> String {
    let mut out = String::new();
    out.push_str("# aimer — AbuseFilter suggestions\n");
    out.push_str("# AbuseFilter cannot inspect EXIF/XMP/C2PA. Rule 1-4 match the upload\n");
    out.push_str("# description/summary; the SHA-1 rules match known file contents.\n");
    out.push_str("# Start with warn/tag. Do not disallow on metadata alone.\n\n");
    for (index, rule) in rules.iter().enumerate() {
        out.push_str(&format!("## {}. {}\n", index + 1, rule.name));
        out.push_str(&format!("# {}\n", rule.description));
        out.push_str(&format!(
            "# Suggested actions: {}\n",
            rule.actions.join(", ")
        ));
        out.push_str(&format!("# Note: {}\n", rule.notes));
        out.push_str(&rule.expression);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, Detection, Level, Signal, Source};

    fn detection(sha1: &str, score: u8) -> Detection {
        Detection {
            id: sha1.to_string(),
            title: "File:Test.png".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            user: "Example".into(),
            comment: String::new(),
            url: String::new(),
            description_url: String::new(),
            mime: "image/png".into(),
            size: 1,
            sha1: sha1.to_string(),
            score,
            level: Level::from_score(score),
            signals: vec![Signal::new(Source::C2pa, Confidence::High, "test", "test")],
        }
    }

    #[test]
    fn description_rules_are_generated() {
        let rules = rules(&[], 80);
        assert!(rules.iter().any(|rule| rule.name.contains("AI tool")));
        let tool_rule = rules
            .iter()
            .find(|rule| rule.name.contains("AI tool"))
            .unwrap();
        assert!(tool_rule.expression.contains("added_lines irlike"));
        assert!(tool_rule.expression.contains("page_namespace == 6"));
        let prompt_rule = rules
            .iter()
            .find(|rule| rule.name.contains("prompt"))
            .unwrap();
        // Regex backslashes must be doubled for AbuseFilter's string syntax.
        assert!(prompt_rule.expression.contains("\\\\b"));
    }

    #[test]
    fn sha1_rules_respect_threshold_and_dedupe() {
        let rules = rules(
            &[
                detection("aaaaaaaa", 90),
                detection("aaaaaaaa", 90),
                detection("bbbbbbbb", 40),
            ],
            80,
        );
        let sha_rules: Vec<_> = rules
            .iter()
            .filter(|rule| rule.name.contains("SHA-1"))
            .collect();
        assert_eq!(sha_rules.len(), 1);
        assert!(sha_rules[0].expression.contains("\"aaaaaaaa\""));
        assert!(!sha_rules[0].expression.contains("\"bbbbbbbb\""));
        assert!(sha_rules[0].expression.contains("equals_to_any"));
        assert!(sha_rules[0].expression.contains("stashupload"));
    }

    #[test]
    fn many_hashes_are_chunked() {
        let detections: Vec<Detection> = (0..HASHES_PER_RULE + 1)
            .map(|index| detection(&format!("{index:040x}"), 90))
            .collect();
        let rules = rules(&detections, 80);
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.name.contains("SHA-1"))
                .count(),
            2
        );
    }

    #[test]
    fn text_output_is_pasteable() {
        let output = text(&rules(&[], 80));
        assert!(output.contains("# aimer"));
        assert!(output.contains("## 1."));
        assert!(output.contains("Suggested actions: warn, tag"));
    }
}
