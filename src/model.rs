//! Detection records and scoring.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// How strong a single signal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn weight(self) -> u8 {
        match self {
            Self::Low => 30,
            Self::Medium => 60,
            Self::High => 90,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Convert an ad-hoc 0..=100 weight into a confidence band.
    pub fn from_weight(weight: u8) -> Self {
        match weight {
            80..=u8::MAX => Self::High,
            50..=79 => Self::Medium,
            _ => Self::Low,
        }
    }
}

/// Overall verdict derived from the score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    None,
    Low,
    Medium,
    High,
}

impl Level {
    pub fn from_score(score: u8) -> Self {
        match score {
            80..=u8::MAX => Self::High,
            50..=79 => Self::Medium,
            25..=49 => Self::Low,
            _ => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Where a signal came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// C2PA / Content Credentials manifest.
    C2pa,
    /// XMP packet (including IPTC Extension properties).
    Xmp,
    /// EXIF / TIFF fields.
    Exif,
    /// PNG text chunks (tEXt, zTXt, iTXt).
    PngText,
    /// Raw container bytes, used as a fallback.
    Container,
    /// MediaWiki `extmetadata` (Commons description-page metadata).
    Metadata,
    /// File-page wikitext.
    Wikitext,
    /// Upload comment/summary.
    Comment,
    /// Pixel-level watermark.
    Watermark,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::C2pa => "C2PA",
            Self::Xmp => "XMP",
            Self::Exif => "EXIF",
            Self::PngText => "PNG text",
            Self::Container => "container",
            Self::Metadata => "description",
            Self::Wikitext => "wikitext",
            Self::Comment => "comment",
            Self::Watermark => "watermark",
        }
    }
}

/// One piece of evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Signal {
    pub source: Source,
    pub confidence: Confidence,
    /// Stable identifier, suitable for filtering or downstream tooling.
    pub code: String,
    /// Human-readable detail, already truncated for storage.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

impl Signal {
    pub fn new(
        source: Source,
        confidence: Confidence,
        code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            source,
            confidence,
            code: code.into(),
            detail: detail.into(),
            tool: None,
        }
    }

    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }
}

/// A stored detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    /// Lowercase hex SHA-1, or a synthetic id if the API did not provide one.
    pub id: String,
    pub title: String,
    pub timestamp: String,
    pub user: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub description_url: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha1: String,
    pub score: u8,
    pub level: Level,
    pub signals: Vec<Signal>,
}

impl Detection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        title: String,
        timestamp: String,
        user: String,
        comment: String,
        url: String,
        description_url: String,
        mime: String,
        size: u64,
        sha1: String,
        signals: Vec<Signal>,
    ) -> Self {
        let score = score(&signals);
        Self {
            id,
            title,
            timestamp,
            user,
            comment,
            url,
            description_url,
            mime,
            size,
            sha1,
            score,
            level: Level::from_score(score),
            signals,
        }
    }

    /// The first identified vendor, for a compact display.
    pub fn tool(&self) -> Option<&str> {
        self.signals
            .iter()
            .find_map(|signal| signal.tool.as_deref())
    }
}

/// Combine signals into a score.
///
/// The strongest signal sets the base; each additional *distinct source* at
/// medium confidence or above adds a small corroboration bonus.
pub fn score(signals: &[Signal]) -> u8 {
    let base = signals
        .iter()
        .map(|signal| signal.confidence.weight())
        .max()
        .unwrap_or(0);
    let sources: HashSet<Source> = signals
        .iter()
        .filter(|signal| signal.confidence >= Confidence::Medium)
        .map(|signal| signal.source)
        .collect();
    let bonus = if sources.len() > 1 { 5 } else { 0 };
    base.saturating_add(bonus).min(99)
}

/// Remove exact duplicates while preserving order.
pub fn dedup(signals: &mut Vec<Signal>) {
    let mut seen = HashSet::new();
    signals.retain(|signal| {
        seen.insert((
            signal.source,
            signal.confidence,
            signal.code.clone(),
            signal.detail.clone(),
            signal.tool.clone(),
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(source: Source, confidence: Confidence, detail: &str) -> Signal {
        Signal::new(source, confidence, "test", detail)
    }

    #[test]
    fn strongest_signal_sets_the_score() {
        assert_eq!(score(&[signal(Source::Exif, Confidence::Low, "a")]), 30);
        assert_eq!(score(&[signal(Source::Exif, Confidence::Medium, "a")]), 60);
        assert_eq!(score(&[signal(Source::C2pa, Confidence::High, "a")]), 90);
    }

    #[test]
    fn corroboration_adds_a_small_bonus() {
        let signals = vec![
            signal(Source::C2pa, Confidence::High, "a"),
            signal(Source::Exif, Confidence::Medium, "b"),
            signal(Source::Wikitext, Confidence::Medium, "c"),
        ];
        assert_eq!(score(&signals), 95);
    }

    #[test]
    fn low_signals_do_not_corroborate() {
        let signals = vec![
            signal(Source::Exif, Confidence::Low, "a"),
            signal(Source::Wikitext, Confidence::Low, "b"),
        ];
        assert_eq!(score(&signals), 30);
    }

    #[test]
    fn levels_are_bounded() {
        assert_eq!(Level::from_score(0), Level::None);
        assert_eq!(Level::from_score(25), Level::Low);
        assert_eq!(Level::from_score(50), Level::Medium);
        assert_eq!(Level::from_score(80), Level::High);
        assert_eq!(Level::from_score(255), Level::High);
    }

    #[test]
    fn dedup_removes_only_exact_duplicates() {
        let mut signals = vec![
            signal(Source::Xmp, Confidence::High, "same"),
            signal(Source::Xmp, Confidence::High, "same"),
            signal(Source::C2pa, Confidence::High, "same"),
        ];
        dedup(&mut signals);
        assert_eq!(signals.len(), 2);
    }
}
