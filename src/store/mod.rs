//! Detection store backends.
//!
//! Both backends share [`Cache`], the in-memory ring the web UI reads and the
//! set of IDs already analysed. The backend persists detections, the wiki list
//! and the per-wiki recent-changes cursors; [`Store`] dispatches between them.

mod file;
#[cfg(feature = "toolsdb")]
mod toolsdb;

use anyhow::Result;

use std::collections::{HashSet, VecDeque};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::config::{Config, StoreKind};
use crate::mediawiki::Cursor;
use crate::model::Detection;
use crate::wiki::Wiki;

pub use file::FileStore;
#[cfg(feature = "toolsdb")]
pub use toolsdb::ToolsDbStore;

/// Query for [`Store::recent`].
#[derive(Debug, Clone)]
pub struct Filter {
    /// Only detections for this wiki domain, or all when `None`.
    pub wiki: Option<String>,
    pub min_score: u8,
    pub text: Option<String>,
    pub limit: usize,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            wiki: None,
            min_score: 0,
            text: None,
            limit: 200,
        }
    }
}

pub enum Store {
    File(Box<FileStore>),
    #[cfg(feature = "toolsdb")]
    ToolsDb(Box<ToolsDbStore>),
}

impl Store {
    /// Open the configured backend for writing.
    pub async fn open(config: &Config) -> Result<Self> {
        Self::build(config, false).await
    }

    /// Open the configured backend for reading only. Used by `serve` while
    /// the bot owns the writer.
    pub async fn open_read_only(config: &Config) -> Result<Self> {
        Self::build(config, true).await
    }

    async fn build(config: &Config, read_only: bool) -> Result<Self> {
        match config.store.kind {
            StoreKind::File if read_only => Ok(Self::File(Box::new(
                FileStore::open_read_only(config).await?,
            ))),
            StoreKind::File => Ok(Self::File(Box::new(FileStore::open(config).await?))),
            StoreKind::Toolsdb => {
                #[cfg(feature = "toolsdb")]
                {
                    Ok(Self::ToolsDb(Box::new(if read_only {
                        ToolsDbStore::open_read_only(config).await?
                    } else {
                        ToolsDbStore::open(config).await?
                    })))
                }
                #[cfg(not(feature = "toolsdb"))]
                {
                    let _ = read_only;
                    anyhow::bail!("The ToolsDB store needs the `toolsdb` feature")
                }
            }
        }
    }

    /// Reload detections and the wiki list, replacing the in-memory views.
    pub async fn reload(&self) -> Result<()> {
        match self {
            Self::File(store) => store.reload().await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.reload().await,
        }
    }

    /// Refresh only the wiki list and cursors. The bot calls this each poll so
    /// wikis added, disabled or removed in the database take effect live.
    pub async fn refresh_wikis(&self) -> Result<()> {
        match self {
            Self::File(store) => store.refresh_wikis().await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.refresh_wikis().await,
        }
    }

    /// All configured wikis, including disabled ones, newest configuration
    /// first loaded. Sorted by `sort_order`, then domain.
    pub fn wikis(&self) -> Vec<Wiki> {
        match self {
            Self::File(store) => store.wikis(),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.wikis(),
        }
    }

    pub fn is_seen(&self, wiki: &str, id: &str) -> bool {
        match self {
            Self::File(store) => store.is_seen(wiki, id),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.is_seen(wiki, id),
        }
    }

    pub fn mark_seen(&self, wiki: &str, id: &str) {
        match self {
            Self::File(store) => store.mark_seen(wiki, id),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.mark_seen(wiki, id),
        }
    }

    /// Store a detection. Returns `false` if the (wiki, id) pair was already
    /// stored.
    pub async fn add(&self, detection: Detection) -> Result<bool> {
        match self {
            Self::File(store) => store.add(detection).await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.add(detection).await,
        }
    }

    pub fn recent(&self, filter: &Filter) -> Vec<Detection> {
        match self {
            Self::File(store) => store.recent(filter),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.recent(filter),
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::File(store) => store.count(),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.count(),
        }
    }

    pub async fn load_cursor(&self, wiki: &str) -> Option<Cursor> {
        match self {
            Self::File(store) => store.load_cursor(wiki).await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.load_cursor(wiki).await,
        }
    }

    pub async fn save_cursor(&self, wiki: &str, cursor: &Cursor) -> Result<()> {
        match self {
            Self::File(store) => store.save_cursor(wiki, cursor).await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.save_cursor(wiki, cursor).await,
        }
    }
}

/// Recent detections (newest first) and the (wiki, id) pairs already analysed.
pub(crate) struct Cache {
    data: RwLock<CacheData>,
    max_records: usize,
    max_seen: usize,
}

#[derive(Default)]
struct CacheData {
    records: VecDeque<Detection>,
    seen_order: VecDeque<String>,
    seen: HashSet<String>,
}

/// The same SHA-1 can be uploaded to two wikis; key those separately.
fn seen_key(wiki: &str, id: &str) -> String {
    format!("{wiki}\u{1f}{id}")
}

impl CacheData {
    fn mark_seen(&mut self, wiki: &str, id: &str, max_seen: usize) {
        if id.is_empty() {
            return;
        }
        let key = seen_key(wiki, id);
        if !self.seen.insert(key.clone()) {
            return;
        }
        self.seen_order.push_back(key);
        while self.seen_order.len() > max_seen {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

impl Cache {
    pub(crate) fn new(max_records: usize) -> Self {
        Self {
            data: RwLock::new(CacheData::default()),
            max_records,
            max_seen: max_records.saturating_mul(20).max(10_000),
        }
    }

    pub(crate) fn max_records(&self) -> usize {
        self.max_records
    }

    /// Replace the ring with records loaded from the backend, keeping at
    /// most `max_records` per wiki and marking their IDs as seen.
    pub(crate) fn replace(&self, records: VecDeque<Detection>) {
        let mut data = self.write();
        data.records = records;
        trim_per_wiki(&mut data.records, self.max_records);
        data.seen.clear();
        data.seen_order.clear();
        let keys: Vec<(String, String)> = data
            .records
            .iter()
            .map(|detection| (detection.wiki.clone(), detection.id.clone()))
            .collect();
        for (wiki, id) in keys {
            data.mark_seen(&wiki, &id, self.max_seen);
        }
    }

    pub(crate) fn is_seen(&self, wiki: &str, id: &str) -> bool {
        self.read().seen.contains(&seen_key(wiki, id))
    }

    pub(crate) fn mark_seen(&self, wiki: &str, id: &str) {
        self.write().mark_seen(wiki, id, self.max_seen);
    }

    pub(crate) fn push(&self, detection: Detection) {
        let mut data = self.write();
        data.mark_seen(&detection.wiki, &detection.id, self.max_seen);
        let wiki = detection.wiki.clone();
        data.records.push_front(detection);
        if data
            .records
            .iter()
            .filter(|entry| entry.wiki == wiki)
            .count()
            > self.max_records
            && let Some(index) = data.records.iter().rposition(|entry| entry.wiki == wiki)
        {
            data.records.remove(index);
        }
    }

    pub(crate) fn recent(&self, filter: &Filter) -> Vec<Detection> {
        let text = filter.text.as_ref().map(|text| text.to_lowercase());
        let data = self.read();
        data.records
            .iter()
            .filter(|detection| {
                filter
                    .wiki
                    .as_deref()
                    .is_none_or(|wiki| detection.wiki == wiki)
            })
            .filter(|detection| detection.score >= filter.min_score)
            .filter(|detection| match &text {
                Some(text) => matches(detection, text),
                None => true,
            })
            .take(filter.limit)
            .cloned()
            .collect()
    }

    pub(crate) fn count(&self) -> usize {
        self.read().records.len()
    }

    fn read(&self) -> RwLockReadGuard<'_, CacheData> {
        self.data
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, CacheData> {
        self.data
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Keep the newest `max_records` entries for each wiki, preserving order.
fn trim_per_wiki(records: &mut VecDeque<Detection>, max_records: usize) {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    records.retain(|record| {
        let count = counts.entry(record.wiki.clone()).or_default();
        if *count < max_records {
            *count += 1;
            true
        } else {
            false
        }
    });
}

fn matches(detection: &Detection, needle: &str) -> bool {
    detection.title.to_lowercase().contains(needle)
        || detection.user.to_lowercase().contains(needle)
        || detection.comment.to_lowercase().contains(needle)
        || detection.mime.to_lowercase().contains(needle)
        || detection.wiki.to_lowercase().contains(needle)
        || detection.signals.iter().any(|signal| {
            signal.code.to_lowercase().contains(needle)
                || signal.detail.to_lowercase().contains(needle)
                || signal
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.to_lowercase().contains(needle))
        })
}

/// A simple builder for tests.
#[cfg(test)]
pub(crate) fn detection(wiki: &str, id: &str, score: u8) -> Detection {
    use crate::model::{Confidence, Level, Signal, Source};

    Detection {
        id: id.to_string(),
        wiki: wiki.to_string(),
        title: format!("File:{id}.png"),
        timestamp: "2026-01-01T00:00:00Z".into(),
        user: "Example".into(),
        comment: String::new(),
        url: String::new(),
        description_url: String::new(),
        mime: "image/png".into(),
        size: 1,
        sha1: id.to_string(),
        score,
        level: Level::from_score(score),
        signals: vec![Signal::new(Source::C2pa, Confidence::High, "test", "test")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_filter_matches_signal_fields() {
        let cache = Cache::new(10);
        cache.push(detection("en.wikipedia.org", "abc", 90));
        let filter = Filter {
            text: Some("test".into()),
            ..Filter::default()
        };
        assert_eq!(cache.recent(&filter).len(), 1);
        assert_eq!(cache.recent(&Filter::default()).len(), 1);
    }

    #[test]
    fn wiki_filter_and_seen_keys_are_separate() {
        let cache = Cache::new(10);
        cache.push(detection("commons.wikimedia.org", "same", 90));
        cache.push(detection("de.wikipedia.org", "same", 90));
        let filter = Filter {
            wiki: Some("de.wikipedia.org".into()),
            ..Filter::default()
        };
        assert_eq!(cache.recent(&filter).len(), 1);
        assert!(cache.is_seen("de.wikipedia.org", "same"));
        assert!(cache.is_seen("commons.wikimedia.org", "same"));
        assert!(!cache.is_seen("en.wikipedia.org", "same"));
    }
}
