//! Wikis to watch.
//!
//! The list lives in the store (a ToolsDB table, or `wikis.json` for the file
//! backend) so it can be changed without a restart. The bot refreshes it every
//! poll; the web service refreshes it on its reload tick. `commons.wikimedia.org`
//! is the default view and is seeded on first start together with the two
//! Wikipedias.

use serde::{Deserialize, Serialize};

use crate::mediawiki::Cursor;

/// The wiki shown when no wiki is selected.
pub const DEFAULT_DOMAIN: &str = "commons.wikimedia.org";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Wiki {
    /// Domain, also the primary key, e.g. `en.wikipedia.org`.
    pub domain: String,
    /// Name for the UI, e.g. `English Wikipedia`.
    pub display_name: String,
    /// Full Action API endpoint, e.g. `https://en.wikipedia.org/w/api.php`.
    pub api_url: String,
    /// Namespace to watch for uploads. Wikimedia file pages are namespace 6.
    #[serde(default = "default_namespace")]
    pub namespace: i32,
    /// Disabled wikis stay configured and keep their cursor, but are not polled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Display order in the wiki selector; ties go by domain.
    #[serde(default)]
    pub sort_order: i32,
    /// Recent-changes position, persisted by the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

fn default_namespace() -> i32 {
    6
}

fn default_true() -> bool {
    true
}

impl Wiki {
    /// The wikis seeded on first start.
    pub fn defaults() -> Vec<Self> {
        [
            (
                "commons.wikimedia.org",
                "Wikimedia Commons",
                "https://commons.wikimedia.org/w/api.php",
                0,
            ),
            (
                "en.wikipedia.org",
                "English Wikipedia",
                "https://en.wikipedia.org/w/api.php",
                1,
            ),
            (
                "de.wikipedia.org",
                "German Wikipedia",
                "https://de.wikipedia.org/w/api.php",
                2,
            ),
        ]
        .into_iter()
        .map(|(domain, display_name, api_url, sort_order)| Self {
            domain: domain.to_string(),
            display_name: display_name.to_string(),
            api_url: api_url.to_string(),
            namespace: default_namespace(),
            enabled: true,
            sort_order,
            cursor: None,
        })
        .collect()
    }

    /// A wiki is usable only if its API endpoint is https (or loopback http for
    /// local tests). The database is trusted, but this keeps a typo or a
    /// compromised row from making the bot fetch an arbitrary host.
    pub fn is_valid(&self) -> bool {
        !self.domain.trim().is_empty()
            && (self.api_url.starts_with("https://")
                || self.api_url.starts_with("http://127.0.0.1")
                || self.api_url.starts_with("http://localhost"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_commons_and_both_wikipedias() {
        let wikis = Wiki::defaults();
        let domains: Vec<&str> = wikis.iter().map(|wiki| wiki.domain.as_str()).collect();
        assert_eq!(
            domains,
            [
                "commons.wikimedia.org",
                "en.wikipedia.org",
                "de.wikipedia.org"
            ]
        );
        assert!(wikis.iter().all(Wiki::is_valid));
    }

    #[test]
    fn non_https_endpoints_are_rejected() {
        let mut wiki = Wiki::defaults().remove(0);
        wiki.api_url = "http://169.254.169.254/latest".into();
        assert!(!wiki.is_valid());
        wiki.api_url = "http://127.0.0.1:8080/w/api.php".into();
        assert!(wiki.is_valid());
    }
}
