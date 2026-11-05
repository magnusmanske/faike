//! Detection store backends.
//!
//! Both backends share [`Cache`], the in-memory ring the web UI reads and the
//! set of IDs already analysed. The backend persists detections and the
//! recent-changes cursor; [`Store`] dispatches between them.

mod file;
#[cfg(feature = "toolsdb")]
mod toolsdb;

use anyhow::Result;

use std::collections::{HashSet, VecDeque};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::config::{Config, StoreKind};
use crate::mediawiki::Cursor;
use crate::model::Detection;

pub use file::FileStore;
#[cfg(feature = "toolsdb")]
pub use toolsdb::ToolsDbStore;

/// Query for [`Store::recent`].
#[derive(Debug, Clone)]
pub struct Filter {
    pub min_score: u8,
    pub text: Option<String>,
    pub limit: usize,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
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

    pub async fn reload(&self) -> Result<()> {
        match self {
            Self::File(store) => store.reload().await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.reload().await,
        }
    }

    pub fn is_seen(&self, id: &str) -> bool {
        match self {
            Self::File(store) => store.is_seen(id),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.is_seen(id),
        }
    }

    pub fn mark_seen(&self, id: &str) {
        match self {
            Self::File(store) => store.mark_seen(id),
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.mark_seen(id),
        }
    }

    /// Store a detection. Returns `false` if the id was already stored.
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

    pub async fn load_cursor(&self) -> Option<Cursor> {
        match self {
            Self::File(store) => store.load_cursor().await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.load_cursor().await,
        }
    }

    pub async fn save_cursor(&self, cursor: &Cursor) -> Result<()> {
        match self {
            Self::File(store) => store.save_cursor(cursor).await,
            #[cfg(feature = "toolsdb")]
            Self::ToolsDb(store) => store.save_cursor(cursor).await,
        }
    }
}

/// Recent detections (newest first) and the IDs already analysed.
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

impl CacheData {
    fn mark_seen(&mut self, id: &str, max_seen: usize) {
        if id.is_empty() || !self.seen.insert(id.to_string()) {
            return;
        }
        self.seen_order.push_back(id.to_string());
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

    /// Replace the ring with records loaded from the backend, marking their
    /// IDs as seen. The seen set is independent of the display ring, so it
    /// can be seeded with more IDs later if needed.
    pub(crate) fn replace(&self, mut records: VecDeque<Detection>) {
        while records.len() > self.max_records {
            records.pop_back();
        }
        let mut data = self.write();
        data.records = records;
        data.seen.clear();
        data.seen_order.clear();
        let ids: Vec<String> = data
            .records
            .iter()
            .map(|detection| detection.id.clone())
            .collect();
        for id in ids {
            data.mark_seen(&id, self.max_seen);
        }
    }

    pub(crate) fn is_seen(&self, id: &str) -> bool {
        self.read().seen.contains(id)
    }

    pub(crate) fn mark_seen(&self, id: &str) {
        self.write().mark_seen(id, self.max_seen);
    }

    pub(crate) fn push(&self, detection: Detection) {
        let mut data = self.write();
        data.mark_seen(&detection.id, self.max_seen);
        data.records.push_front(detection);
        while data.records.len() > self.max_records {
            data.records.pop_back();
        }
    }

    pub(crate) fn recent(&self, filter: &Filter) -> Vec<Detection> {
        let text = filter.text.as_ref().map(|text| text.to_lowercase());
        let data = self.read();
        data.records
            .iter()
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

fn matches(detection: &Detection, needle: &str) -> bool {
    detection.title.to_lowercase().contains(needle)
        || detection.user.to_lowercase().contains(needle)
        || detection.comment.to_lowercase().contains(needle)
        || detection.mime.to_lowercase().contains(needle)
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
pub(crate) fn detection(id: &str, score: u8) -> Detection {
    use crate::model::{Confidence, Level, Signal, Source};

    Detection {
        id: id.to_string(),
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
        cache.push(detection("abc", 90));
        let filter = Filter {
            text: Some("test".into()),
            ..Filter::default()
        };
        assert_eq!(cache.recent(&filter).len(), 1);
        assert_eq!(cache.recent(&Filter::default()).len(), 1);
    }
}
