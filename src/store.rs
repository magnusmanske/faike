//! Append-only detection store.
//!
//! JSONL on disk plus an in-memory ring for the web UI. This is deliberately
//! small: one process writes, the same process serves. A future multi-process
//! deployment can swap this for ToolsDB without touching the watcher.

use anyhow::{Context, Result};

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::mediawiki::Cursor;
use crate::model::Detection;

pub struct Store {
    data: RwLock<StoreData>,
    file: Mutex<tokio::fs::File>,
    add_lock: Mutex<()>,
    store_path: PathBuf,
    cursor_path: PathBuf,
    max_records: usize,
    max_seen: usize,
}

struct StoreData {
    records: VecDeque<Detection>,
    seen_order: VecDeque<String>,
    seen: HashSet<String>,
}

impl StoreData {
    fn new() -> Self {
        Self {
            records: VecDeque::new(),
            seen_order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

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

impl Store {
    pub async fn open(config: &crate::config::Config) -> Result<Self> {
        ensure_parent(&config.store_file).await?;
        let max_seen = config.max_records.saturating_mul(20).max(10_000);
        let (records, compacted) = load_records(&config.store_file, config.max_records).await?;
        if let Some(lines) = compacted {
            write_lines(&config.store_file, &lines).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.store_file)
            .await
            .with_context(|| format!("Cannot open {}", config.store_file.display()))?;
        let store = Self {
            data: RwLock::new(StoreData::new()),
            file: Mutex::new(file),
            add_lock: Mutex::new(()),
            store_path: config.store_file.clone(),
            cursor_path: config.cursor_file.clone(),
            max_records: config.max_records,
            max_seen,
        };
        {
            let mut data = store.write();
            for detection in records {
                data.mark_seen(&detection.id, store.max_seen);
                data.records.push_back(detection);
            }
        }
        Ok(store)
    }

    pub fn is_seen(&self, id: &str) -> bool {
        self.read().seen.contains(id)
    }

    pub fn mark_seen(&self, id: &str) {
        self.write().mark_seen(id, self.max_seen);
    }

    /// Store a detection. Returns `false` if the id was already seen.
    pub async fn add(&self, detection: Detection) -> Result<bool> {
        // Serialise check-and-insert: two concurrent downloads of the same
        // bytes must not write two records.
        let _guard = self.add_lock.lock().await;
        if self.is_seen(&detection.id) {
            return Ok(false);
        }
        let mut line = serde_json::to_vec(&detection).context("Serializing detection")?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line)
            .await
            .with_context(|| format!("Writing {}", self.store_path.display()))?;
        file.flush().await?;
        drop(file);
        let mut data = self.write();
        data.mark_seen(&detection.id, self.max_seen);
        data.records.push_front(detection);
        while data.records.len() > self.max_records {
            data.records.pop_back();
        }
        Ok(true)
    }

    pub fn recent(&self, filter: &Filter) -> Vec<Detection> {
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

    pub fn count(&self) -> usize {
        self.read().records.len()
    }

    pub async fn load_cursor(&self) -> Option<Cursor> {
        let contents = tokio::fs::read_to_string(&self.cursor_path).await.ok()?;
        serde_json::from_str(&contents).ok()
    }

    pub async fn save_cursor(&self, cursor: &Cursor) -> Result<()> {
        ensure_parent(&self.cursor_path).await?;
        let temporary = self.cursor_path.with_extension("json.tmp");
        let contents = serde_json::to_vec(cursor).context("Serializing cursor")?;
        tokio::fs::write(&temporary, contents)
            .await
            .with_context(|| format!("Writing {}", temporary.display()))?;
        tokio::fs::rename(&temporary, &self.cursor_path)
            .await
            .with_context(|| format!("Replacing {}", self.cursor_path.display()))?;
        Ok(())
    }

    fn read(&self) -> RwLockReadGuard<'_, StoreData> {
        self.data
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, StoreData> {
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

/// Read the last `max_records` JSONL lines. Returns them newest-first and,
/// when the file has grown well past the in-memory cap, a compacted line set
/// that can be written back.
async fn load_records(
    path: &Path,
    max_records: usize,
) -> Result<(VecDeque<Detection>, Option<Vec<String>>)> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((VecDeque::new(), None));
        }
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    let mut reader = BufReader::new(file).lines();
    let mut kept: VecDeque<String> = VecDeque::new();
    let mut total = 0usize;
    while let Some(line) = reader
        .next_line()
        .await
        .with_context(|| format!("Reading {}", path.display()))?
    {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        kept.push_back(line);
        if kept.len() > max_records {
            kept.pop_front();
        }
    }

    let mut seen = HashSet::new();
    let mut records = VecDeque::new();
    let mut valid = Vec::new();
    for line in &kept {
        let Ok(detection) = serde_json::from_str::<Detection>(line) else {
            continue;
        };
        if !seen.insert(detection.id.clone()) {
            continue;
        }
        records.push_front(detection);
        valid.push(line.clone());
    }
    let compact = (total > max_records.saturating_mul(2)).then_some(valid);
    Ok((records, compact))
}

/// Atomically replace a file with the given lines.
async fn write_lines(path: &Path, lines: &[String]) -> Result<()> {
    let temporary = path.with_extension("jsonl.tmp");
    let mut contents = lines.join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    tokio::fs::write(&temporary, contents)
        .await
        .with_context(|| format!("Writing {}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

async fn ensure_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Creating {}", parent.display()))?;
    Ok(())
}

/// A simple builder for tests.
#[cfg(test)]
fn detection(id: &str, score: u8) -> Detection {
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
    use crate::config::Config;
    use tempfile::TempDir;

    fn config(dir: &TempDir) -> Config {
        Config {
            store_file: dir.path().join("detections.jsonl"),
            cursor_file: dir.path().join("cursor.json"),
            max_records: 10,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn detects_are_persisted_and_loaded() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        let store = Store::open(&config).await.unwrap();
        assert!(store.add(detection("a", 90)).await.unwrap());
        assert!(!store.add(detection("a", 90)).await.unwrap());
        assert!(store.add(detection("b", 40)).await.unwrap());
        assert_eq!(store.count(), 2);
        drop(store);

        let store = Store::open(&config).await.unwrap();
        assert_eq!(store.count(), 2);
        assert!(store.is_seen("a"));
        let recent = store.recent(&Filter::default());
        assert_eq!(recent[0].id, "b", "newest record comes first");
    }

    #[tokio::test]
    async fn filter_matches_text_and_score() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&config(&dir)).await.unwrap();
        store.add(detection("a", 90)).await.unwrap();
        store.add(detection("b", 20)).await.unwrap();
        let filter = Filter {
            min_score: 50,
            ..Filter::default()
        };
        assert_eq!(store.recent(&filter).len(), 1);
        let filter = Filter {
            text: Some("a.png".into()),
            ..Filter::default()
        };
        assert_eq!(store.recent(&filter).len(), 1);
    }

    #[tokio::test]
    async fn cursor_round_trips() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&config(&dir)).await.unwrap();
        let cursor = Cursor {
            timestamp: "2026-01-01T00:00:00Z".into(),
            rc_id: 42,
        };
        store.save_cursor(&cursor).await.unwrap();
        assert_eq!(store.load_cursor().await, Some(cursor));
    }

    #[tokio::test]
    async fn memory_is_bounded() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            max_records: 2,
            ..config(&dir)
        };
        let store = Store::open(&config).await.unwrap();
        for id in ["a", "b", "c"] {
            store.add(detection(id, 90)).await.unwrap();
        }
        assert_eq!(store.count(), 2);
        assert!(
            store.is_seen("a"),
            "seen set is independent of the display ring"
        );
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        tokio::fs::write(&config.store_file, "not json\n")
            .await
            .unwrap();
        let store = Store::open(&config).await.unwrap();
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn store_file_is_compacted() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            max_records: 2,
            ..config(&dir)
        };
        let store = Store::open(&config).await.unwrap();
        for index in 0..10 {
            store
                .add(detection(&format!("id{index}"), 90))
                .await
                .unwrap();
        }
        drop(store);

        let store = Store::open(&config).await.unwrap();
        assert_eq!(store.count(), 2);
        let contents = tokio::fs::read_to_string(&config.store_file).await.unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
