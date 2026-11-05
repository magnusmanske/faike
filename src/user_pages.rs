//! Cached user-page existence lookups for the web UI.
//!
//! The UI links an uploader to `User:` and `User_talk:` on the wiki where the
//! file was uploaded. To style missing pages like MediaWiki redlinks it asks
//! each wiki's Action API once per user and caches the answer. Lookups are
//! best-effort: if a wiki is slow or the API fails, links stay unmarked
//! instead of blocking the page.

use crate::config::Config;
use crate::model::Detection;
use crate::wiki::Wiki;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a positive or negative answer is reused.
const TTL: Duration = Duration::from_secs(3600);
/// Upper bound on cached answers, so a long-running web process stays bounded.
const MAX_ENTRIES: usize = 20_000;
/// The Action API accepts 50 titles; each user needs two (`User:` and `User_talk:`).
const USERS_PER_REQUEST: usize = 25;
/// Chunks fetched in parallel.
const MAX_CONCURRENT: usize = 4;
/// Keep the page responsive when a wiki is unavailable.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the uploader's user and user-talk pages exist. `None` means the
/// answer is unknown (lookup disabled, failed, or the wiki is not configured).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Exists {
    pub user: Option<bool>,
    pub talk: Option<bool>,
}

pub struct UserPages {
    client: Option<Client>,
    entries: Mutex<HashMap<String, (Exists, Instant)>>,
}

impl UserPages {
    pub fn new(config: &Config) -> Self {
        let client = (config.check_user_pages && !config.user_agent.trim().is_empty())
            .then(|| {
                Client::builder()
                    .user_agent(&config.user_agent)
                    .timeout(TIMEOUT)
                    .gzip(true)
                    .build()
                    .ok()
            })
            .flatten();
        Self {
            client,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A checker that never contacts a wiki, used by tests.
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            client: None,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the pages for every non-empty uploader in `detections`.
    /// Unknown answers are omitted rather than guessed.
    pub async fn resolve(
        &self,
        detections: &[Detection],
        wikis: &[Wiki],
    ) -> HashMap<(String, String), Exists> {
        let mut resolved = HashMap::new();
        let Some(client) = &self.client else {
            return resolved;
        };
        let api_by_domain: HashMap<&str, &str> = wikis
            .iter()
            .map(|wiki| (wiki.domain.as_str(), wiki.api_url.as_str()))
            .collect();

        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        for detection in detections {
            let user = detection.user.trim();
            if user.is_empty()
                || !safe_domain(&detection.wiki)
                || !api_by_domain.contains_key(detection.wiki.as_str())
            {
                continue;
            }
            let key = (detection.wiki.clone(), user.to_string());
            if seen.insert(key.clone()) {
                wanted.push(key);
            }
        }
        if wanted.is_empty() {
            return resolved;
        }

        let now = Instant::now();
        {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if entries.len() > MAX_ENTRIES {
                entries.clear();
            }
            wanted.retain(|key| match entries.get(&cache_key(key)) {
                Some((exists, fetched)) if now.duration_since(*fetched) < TTL => {
                    resolved.insert(key.clone(), *exists);
                    false
                }
                _ => true,
            });
        }
        if wanted.is_empty() {
            return resolved;
        }

        let mut by_domain: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for key in wanted {
            by_domain.entry(key.0.clone()).or_default().push(key);
        }
        let mut chunks = Vec::new();
        for (domain, users) in by_domain {
            let Some(api_url) = api_by_domain
                .get(domain.as_str())
                .map(|url| url.to_string())
            else {
                continue;
            };
            for chunk in users.chunks(USERS_PER_REQUEST) {
                chunks.push((api_url.clone(), chunk.to_vec()));
            }
        }

        let results = stream::iter(chunks)
            .map(|(api_url, users)| fetch_chunk(client, api_url, users))
            .buffer_unordered(MAX_CONCURRENT)
            .collect::<Vec<_>>()
            .await;

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for chunk in results {
            for (key, exists) in chunk {
                entries.insert(cache_key(&key), (exists, now));
                resolved.insert(key, exists);
            }
        }
        resolved
    }
}

fn cache_key((domain, user): &(String, String)) -> String {
    format!("{domain}\n{user}")
}

/// Domains come from the database; restrict them to a hostname shape before
/// interpolating into a URL.
pub fn safe_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 255
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

async fn fetch_chunk(
    client: &Client,
    api_url: String,
    users: Vec<(String, String)>,
) -> Vec<((String, String), Exists)> {
    let titles = users
        .iter()
        .flat_map(|(_, user)| [format!("User:{user}"), format!("User_talk:{user}")])
        .collect::<Vec<_>>()
        .join("|");
    let response = client
        .get(api_url)
        .query(&[
            ("action", "query"),
            ("format", "json"),
            ("formatversion", "2"),
            ("prop", "info"),
            ("maxlag", "5"),
            ("titles", titles.as_str()),
        ])
        .send()
        .await;
    let Ok(response) = response else {
        return Vec::new();
    };
    let Ok(response) = response.error_for_status() else {
        return Vec::new();
    };
    let Ok(value) = response.json::<Value>().await else {
        return Vec::new();
    };
    parse_pages(&value, &users)
}

/// Map an API response back to the requested `(wiki domain, user)` pairs.
fn parse_pages(value: &Value, users: &[(String, String)]) -> Vec<((String, String), Exists)> {
    let Some(pages) = value.pointer("/query/pages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let exists: HashMap<&str, bool> = pages
        .iter()
        .filter_map(|page| {
            let title = page.get("title")?.as_str()?;
            let missing = page
                .get("missing")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let invalid = page
                .get("invalid")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some((title, !missing && !invalid))
        })
        .collect();
    let normalized: HashMap<&str, &str> = value
        .pointer("/query/normalized")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| Some((item.get("from")?.as_str()?, item.get("to")?.as_str()?)))
                .collect()
        })
        .unwrap_or_default();

    users
        .iter()
        .map(|(domain, user)| {
            let user_title = format!("User:{user}");
            let talk_title = format!("User_talk:{user}");
            let user_final = normalized
                .get(user_title.as_str())
                .copied()
                .unwrap_or(&user_title);
            let talk_final = normalized
                .get(talk_title.as_str())
                .copied()
                .unwrap_or(&talk_title);
            (
                (domain.clone(), user.clone()),
                Exists {
                    user: exists.get(user_final).copied(),
                    talk: exists.get(talk_final).copied(),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_and_invalid_titles_are_absent() {
        let value = json!({"query": {
            "normalized": [
                {"from": "User:foo", "to": "User:Foo"},
                {"from": "User_talk:foo", "to": "User talk:Foo"}
            ],
            "pages": [
                {"title": "User:Foo", "missing": true},
                {"title": "User talk:Foo", "missing": false},
                {"title": "User:Bar", "invalid": true}
            ]
        }});
        let users = vec![
            ("commons.wikimedia.org".to_string(), "foo".to_string()),
            ("commons.wikimedia.org".to_string(), "Bar".to_string()),
        ];
        let result = parse_pages(&value, &users);
        assert_eq!(
            result[0].1,
            Exists {
                user: Some(false),
                talk: Some(true)
            }
        );
        assert_eq!(
            result[1].1,
            Exists {
                user: Some(false),
                talk: None
            }
        );
    }

    #[test]
    fn unsafe_domains_are_rejected() {
        assert!(safe_domain("commons.wikimedia.org"));
        assert!(!safe_domain("evil.example/\""));
        assert!(!safe_domain(""));
    }
}
