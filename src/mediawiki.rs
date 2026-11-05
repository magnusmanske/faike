//! Minimal MediaWiki Action API client.
//!
//! We read only public data, so no authentication is needed. The client uses
//! the same recent-changes cursor model as `wdrc_rs`: rows are consumed in
//! `(timestamp, rcid)` order and the cursor advances over *all* rows, including
//! ones this tool ignores.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const MAX_TITLES_PER_REQUEST: usize = 20;
const MAX_CONTINUATION_PAGES: usize = 5;
const MAX_RETRIES: usize = 3;
const MAX_RETRY_DELAY_SECS: u64 = 60;
/// Do not let a pathological comment consume memory.
const MAX_COMMENT_CHARS: usize = 8000;

/// Position in `recentchanges`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cursor {
    pub timestamp: String,
    pub rc_id: u64,
}

/// One upload log event.
#[derive(Debug, Clone)]
pub struct UploadEvent {
    pub title: String,
    pub timestamp: String,
    pub rc_id: u64,
    pub user: String,
    pub comment: String,
    /// Base-36 `img_sha1` from the log parameters, if present.
    pub sha1_base36: String,
}

/// A batch of recent changes.
#[derive(Debug, Default)]
pub struct RecentBatch {
    pub uploads: Vec<UploadEvent>,
    /// Newest row seen, including rows that were not uploads.
    pub cursor: Option<Cursor>,
    /// The API has more rows waiting; poll again without sleeping.
    pub more: bool,
}

/// File information returned by `prop=imageinfo`.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub title: String,
    pub url: String,
    pub description_url: String,
    pub mime: String,
    pub size: u64,
    /// Hex SHA-1, as returned by the API (AbuseFilter uses hex too).
    pub sha1: String,
    pub extmetadata: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    api_url: String,
    rc_namespace: i32,
    max_download_bytes: u64,
    max_wikitext_bytes: usize,
}

impl ApiClient {
    pub fn new(config: &Config) -> Result<Self> {
        if config.user_agent.trim().is_empty() {
            bail!("A contactable `user_agent` is required");
        }
        let client = Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(config.request_timeout_sec))
            .gzip(true)
            .build()
            .context("Cannot build HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.clone(),
            rc_namespace: config.rc_namespace,
            max_download_bytes: config.max_download_bytes,
            max_wikitext_bytes: config.max_wikitext_bytes,
        })
    }

    /// Newest `count` upload events, newest first. Used for the small initial
    /// backfill. With `count == 0`, only fetches a cursor.
    pub async fn latest_uploads(&self, count: usize) -> Result<RecentBatch> {
        let mut params = self.recent_params();
        params.insert("rcdir".into(), "older".into());
        params.insert("rclimit".into(), count.clamp(1, 500).to_string());
        let value = self.api_get(&params).await?;
        let batch = self.parse_recent(&value, None, 1)?;
        if count == 0 {
            // Bootstrap mode: keep the cursor, discard the one row we fetched.
            return Ok(RecentBatch {
                uploads: Vec::new(),
                cursor: batch.cursor,
                more: false,
            });
        }
        Ok(batch)
    }

    /// Uploads after `cursor`, in ascending order. The returned cursor is the
    /// newest row seen, even if it was not an upload.
    pub async fn uploads_since(&self, cursor: &Cursor, limit: usize) -> Result<RecentBatch> {
        let mut params = self.recent_params();
        params.insert("rcdir".into(), "newer".into());
        params.insert("rcstart".into(), cursor.timestamp.clone());
        params.insert("rclimit".into(), limit.clamp(1, 500).to_string());

        let mut batch = RecentBatch::default();
        let mut continuation: Option<HashMap<String, String>> = None;
        for page in 0..MAX_CONTINUATION_PAGES {
            let mut request = params.clone();
            if let Some(continuation) = &continuation {
                request.extend(continuation.clone());
            }
            let value = self.api_get(&request).await?;
            let page_batch = self.parse_recent(&value, Some(cursor), limit)?;
            batch.uploads.extend(page_batch.uploads);
            if let Some(new_cursor) = page_batch.cursor
                && batch
                    .cursor
                    .as_ref()
                    .is_none_or(|current| new_cursor > *current)
            {
                batch.cursor = Some(new_cursor);
            }
            continuation = continue_params(&value);
            if continuation.is_none() {
                break;
            }
            if page + 1 == MAX_CONTINUATION_PAGES {
                batch.more = true;
            }
        }
        // A full page usually means more is waiting.
        if batch.uploads.len() >= limit {
            batch.more = true;
        }
        Ok(batch)
    }

    fn base_params(&self) -> HashMap<String, String> {
        HashMap::from([
            ("action".into(), "query".into()),
            ("format".into(), "json".into()),
            ("formatversion".into(), "2".into()),
            ("maxlag".into(), "5".into()),
            ("curtimestamp".into(), "1".into()),
        ])
    }

    fn recent_params(&self) -> HashMap<String, String> {
        let mut params = self.base_params();
        params.insert("list".into(), "recentchanges".into());
        params.insert("rctype".into(), "log".into());
        params.insert("rcnamespace".into(), self.rc_namespace.to_string());
        params.insert(
            "rcprop".into(),
            "title|timestamp|ids|user|comment|loginfo".into(),
        );
        params
    }

    fn parse_recent(
        &self,
        value: &Value,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<RecentBatch> {
        let rows = value
            .pointer("/query/recentchanges")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut batch = RecentBatch::default();
        for row in rows {
            let title = row.get("title").and_then(Value::as_str).unwrap_or("");
            let timestamp = row.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let rc_id = row.get("rcid").and_then(Value::as_u64).unwrap_or(0);
            if timestamp.is_empty() || rc_id == 0 {
                continue;
            }
            let row_cursor = Cursor {
                timestamp: timestamp.to_string(),
                rc_id,
            };
            if batch
                .cursor
                .as_ref()
                .is_none_or(|current| row_cursor > *current)
            {
                batch.cursor = Some(row_cursor.clone());
            }
            if after.is_some_and(|after| row_cursor <= *after) {
                continue;
            }
            let logtype = row.get("logtype").and_then(Value::as_str).unwrap_or("");
            let logaction = row.get("logaction").and_then(Value::as_str).unwrap_or("");
            if logtype != "upload" || !matches!(logaction, "upload" | "overwrite") {
                continue;
            }
            let sha1_base36 = row
                .pointer("/logparams/img_sha1")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            batch.uploads.push(UploadEvent {
                title: title.to_string(),
                timestamp: timestamp.to_string(),
                rc_id,
                user: row
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                comment: crate::patterns::truncate(
                    row.get("comment").and_then(Value::as_str).unwrap_or(""),
                    MAX_COMMENT_CHARS,
                ),
                sha1_base36,
            });
        }
        // The API returns rows in order; a full first page means more may wait.
        if batch.uploads.len() >= limit {
            batch.more = true;
        }
        Ok(batch)
    }

    /// Fetch `imageinfo` for up to 20 titles.
    pub async fn imageinfo(&self, titles: &[String]) -> Result<HashMap<String, FileInfo>> {
        let mut out = HashMap::new();
        for chunk in titles.chunks(MAX_TITLES_PER_REQUEST) {
            let mut params = self.base_params();
            params.insert("prop".into(), "imageinfo".into());
            params.insert("iiprop".into(), "url|size|mime|sha1|extmetadata".into());
            params.insert("iilimit".into(), "1".into());
            params.insert("titles".into(), chunk.join("|"));
            let value = self.api_get(&params).await?;
            let pages = value
                .pointer("/query/pages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for page in pages {
                if page.get("missing").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(info) = page
                    .get("imageinfo")
                    .and_then(Value::as_array)
                    .and_then(|infos| infos.first())
                else {
                    continue;
                };
                let title = page
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let extmetadata = info
                    .get("extmetadata")
                    .map(extmetadata_pairs)
                    .unwrap_or_default();
                out.insert(
                    title.clone(),
                    FileInfo {
                        title,
                        url: info
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        description_url: info
                            .get("descriptionurl")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        mime: info
                            .get("mime")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        size: info.get("size").and_then(Value::as_u64).unwrap_or(0),
                        sha1: info
                            .get("sha1")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_lowercase(),
                        extmetadata,
                    },
                );
            }
        }
        Ok(out)
    }

    /// Fetch current file-page wikitext for up to 20 titles.
    pub async fn wikitext(&self, titles: &[String]) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        for chunk in titles.chunks(MAX_TITLES_PER_REQUEST) {
            let mut params = self.base_params();
            params.insert("prop".into(), "revisions".into());
            params.insert("rvprop".into(), "content".into());
            params.insert("rvslots".into(), "main".into());
            params.insert("titles".into(), chunk.join("|"));
            let value = self.api_get(&params).await?;
            let pages = value
                .pointer("/query/pages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for page in pages {
                if page.get("missing").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(content) = page
                    .pointer("/revisions/0/slots/main/content")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let title = page
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content: String = content.chars().take(self.max_wikitext_bytes).collect();
                out.insert(title, content);
            }
        }
        Ok(out)
    }

    /// Download a file, refusing to exceed the configured size.
    pub async fn download(&self, url: &str) -> Result<Bytes> {
        if url.is_empty() {
            bail!("Empty file URL");
        }
        let mut last_error = None;
        for attempt in 0..MAX_RETRIES {
            let response = match self.client.get(url).send().await {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_before_retry(attempt, None).await;
                    continue;
                }
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after = retry_after(&response);
                last_error = Some(format!("HTTP {status}"));
                sleep_before_retry(attempt, retry_after).await;
                continue;
            }
            let response = response
                .error_for_status()
                .with_context(|| format!("Downloading {url}"))?;
            if let Some(length) = response.content_length()
                && length > self.max_download_bytes
            {
                bail!(
                    "File is {length} bytes, above the {} byte download limit",
                    self.max_download_bytes
                );
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.with_context(|| format!("Downloading {url}"))?;
                if bytes.len() as u64 + chunk.len() as u64 > self.max_download_bytes {
                    bail!(
                        "File exceeded the {} byte download limit",
                        self.max_download_bytes
                    );
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Bytes::from(bytes));
        }
        bail!(
            "Download failed after {MAX_RETRIES} attempts: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )
    }

    async fn api_get(&self, params: &HashMap<String, String>) -> Result<Value> {
        let mut last_error = None;
        for attempt in 0..MAX_RETRIES {
            let response = match self.client.get(&self.api_url).query(params).send().await {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_before_retry(attempt, None).await;
                    continue;
                }
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after = retry_after(&response);
                last_error = Some(format!("HTTP {status}"));
                sleep_before_retry(attempt, retry_after).await;
                continue;
            }
            let body = response
                .error_for_status()
                .context("MediaWiki API request")?
                .text()
                .await
                .context("Reading MediaWiki API response")?;
            let value: Value = match serde_json::from_str(&body) {
                Ok(value) => value,
                Err(error) => {
                    // A rate-limit or captcha page can arrive with HTTP 200.
                    // Treat it as retryable rather than crashing the watcher.
                    let preview: String = body.chars().take(160).collect();
                    last_error = Some(format!("invalid JSON: {error}; body: {preview}"));
                    sleep_before_retry(attempt, None).await;
                    continue;
                }
            };
            if let Some(error) = value.get("error") {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let info = error.get("info").and_then(Value::as_str).unwrap_or("");
                if code == "maxlag" {
                    last_error = Some(format!("maxlag: {info}"));
                    sleep_before_retry(attempt, None).await;
                    continue;
                }
                bail!("MediaWiki API error {code}: {info}");
            }
            return Ok(value);
        }
        bail!(
            "MediaWiki API request failed after {MAX_RETRIES} attempts: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

fn continue_params(value: &Value) -> Option<HashMap<String, String>> {
    let object = value.get("continue")?.as_object()?;
    let params: HashMap<String, String> = object
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
        .collect();
    (!params.is_empty()).then_some(params)
}

fn extmetadata_pairs(value: &Value) -> Vec<(String, String)> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .filter_map(|(key, entry)| {
            let value = entry.get("value")?;
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Null => return None,
                other => other.to_string(),
            };
            (!value.is_empty()).then_some((key.clone(), value))
        })
        .collect()
}

fn retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.min(MAX_RETRY_DELAY_SECS))
}

async fn sleep_before_retry(attempt: usize, retry_after: Option<u64>) {
    let seconds = retry_after.unwrap_or((1u64 << attempt).min(MAX_RETRY_DELAY_SECS));
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client() -> ApiClient {
        let config = Config {
            user_agent: "aimer-test/0.1 (test@example.org)".into(),
            ..Config::default()
        };
        ApiClient::new(&config).unwrap()
    }

    #[test]
    fn upload_rows_are_extracted() {
        let value = json!({
            "query": {"recentchanges": [
                {
                    "type": "log", "ns": 6, "title": "File:Test.png",
                    "rcid": 10, "timestamp": "2026-01-01T00:00:00Z",
                    "user": "Example", "comment": "upload",
                    "logtype": "upload", "logaction": "upload",
                    "logparams": {"img_sha1": "abc123"}
                },
                {
                    "type": "log", "ns": 6, "title": "File:Other.png",
                    "rcid": 11, "timestamp": "2026-01-01T00:00:01Z",
                    "logtype": "delete", "logaction": "delete"
                }
            ]}
        });
        let batch = client().parse_recent(&value, None, 10).unwrap();
        assert_eq!(batch.uploads.len(), 1);
        assert_eq!(batch.uploads[0].title, "File:Test.png");
        assert_eq!(batch.uploads[0].sha1_base36, "abc123");
        assert_eq!(
            batch.cursor,
            Some(Cursor {
                timestamp: "2026-01-01T00:00:01Z".into(),
                rc_id: 11
            })
        );
    }

    #[test]
    fn cursor_filters_old_rows() {
        let value = json!({
            "query": {"recentchanges": [
                {
                    "type": "log", "title": "File:Old.png",
                    "rcid": 9, "timestamp": "2026-01-01T00:00:00Z",
                    "logtype": "upload", "logaction": "upload"
                },
                {
                    "type": "log", "title": "File:New.png",
                    "rcid": 12, "timestamp": "2026-01-01T00:00:02Z",
                    "logtype": "upload", "logaction": "upload"
                }
            ]}
        });
        let cursor = Cursor {
            timestamp: "2026-01-01T00:00:00Z".into(),
            rc_id: 10,
        };
        let batch = client().parse_recent(&value, Some(&cursor), 10).unwrap();
        assert_eq!(batch.uploads.len(), 1);
        assert_eq!(batch.uploads[0].title, "File:New.png");
    }

    #[test]
    fn extmetadata_flattens_values() {
        let value = json!({
            "Software": {"value": "Midjourney"},
            "AuthorCount": {"value": 2},
            "Empty": {"value": ""}
        });
        let pairs = extmetadata_pairs(&value);
        assert!(pairs.contains(&("Software".into(), "Midjourney".into())));
        assert!(pairs.contains(&("AuthorCount".into(), "2".into())));
        assert!(!pairs.iter().any(|(key, _)| key == "Empty"));
    }
}
