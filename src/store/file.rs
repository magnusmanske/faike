//! Append-only JSONL on disk, with the wiki list and cursors in `wikis.json`.
//!
//! One process writes; the same process, or another with `open_read_only`,
//! reads. A multi-process deployment should use the ToolsDB backend.

use anyhow::{Context, Result, bail};

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use super::Cache;
use crate::config::Config;
use crate::mediawiki::Cursor;
use crate::model::Detection;
use crate::wiki::Wiki;

pub struct FileStore {
    cache: Cache,
    file: Option<Mutex<tokio::fs::File>>,
    add_lock: Mutex<()>,
    store_path: PathBuf,
    wikis: RwLock<Vec<Wiki>>,
    wikis_path: PathBuf,
}

impl FileStore {
    pub async fn open(config: &Config) -> Result<Self> {
        let store_path = &config.store.file;
        ensure_parent(store_path).await?;
        let (records, compacted) = load_records(store_path, config.max_records).await?;
        if let Some(lines) = compacted {
            write_lines(store_path, &lines).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(store_path)
            .await
            .with_context(|| format!("Cannot open {}", store_path.display()))?;
        let wikis_path = &config.store.wikis_file;
        ensure_parent(wikis_path).await?;
        let wikis = if wikis_path.exists() {
            load_wikis(wikis_path).await?
        } else {
            let wikis = Wiki::defaults();
            write_wikis(wikis_path, &wikis).await?;
            wikis
        };
        Ok(Self::build(config, Some(file), records, wikis))
    }

    /// Open an existing store without taking a writer handle or compacting it.
    pub async fn open_read_only(config: &Config) -> Result<Self> {
        let (records, _) = load_records(&config.store.file, config.max_records).await?;
        let wikis = load_wikis_or_default(&config.store.wikis_file).await?;
        Ok(Self::build(config, None, records, wikis))
    }

    fn build(
        config: &Config,
        file: Option<tokio::fs::File>,
        records: VecDeque<Detection>,
        wikis: Vec<Wiki>,
    ) -> Self {
        let cache = Cache::new(config.max_records);
        cache.replace(records);
        Self {
            cache,
            file: file.map(Mutex::new),
            add_lock: Mutex::new(()),
            store_path: config.store.file.clone(),
            wikis: RwLock::new(wikis),
            wikis_path: config.store.wikis_file.clone(),
        }
    }

    /// Reload the JSONL and wiki list from disk, replacing the in-memory view.
    pub async fn reload(&self) -> Result<()> {
        let (records, _) = load_records(&self.store_path, self.cache.max_records()).await?;
        self.cache.replace(records);
        self.refresh_wikis().await
    }

    /// Re-read the wiki list. Missing files keep the current list, so a
    /// transient read error does not disable every wiki.
    pub async fn refresh_wikis(&self) -> Result<()> {
        if !self.wikis_path.exists() {
            return Ok(());
        }
        let wikis = load_wikis(&self.wikis_path).await?;
        *self.write_wikis() = wikis;
        Ok(())
    }

    pub fn wikis(&self) -> Vec<Wiki> {
        self.read_wikis().clone()
    }

    pub fn is_seen(&self, wiki: &str, id: &str) -> bool {
        self.cache.is_seen(wiki, id)
    }

    pub fn mark_seen(&self, wiki: &str, id: &str) {
        self.cache.mark_seen(wiki, id);
    }

    /// Store a detection. Returns `false` if the (wiki, id) pair was already
    /// seen.
    pub async fn add(&self, detection: Detection) -> Result<bool> {
        // Serialise check-and-insert: two concurrent downloads of the same
        // bytes must not write two records.
        let _guard = self.add_lock.lock().await;
        if self.cache.is_seen(&detection.wiki, &detection.id) {
            return Ok(false);
        }
        let Some(file_lock) = &self.file else {
            bail!("Detection store is read-only");
        };
        let mut line = serde_json::to_vec(&detection).context("Serializing detection")?;
        line.push(b'\n');
        let mut file = file_lock.lock().await;
        file.write_all(&line)
            .await
            .with_context(|| format!("Writing {}", self.store_path.display()))?;
        file.flush().await?;
        drop(file);
        self.cache.push(detection);
        Ok(true)
    }

    pub fn recent(&self, filter: &super::Filter) -> Vec<Detection> {
        self.cache.recent(filter)
    }

    pub fn count(&self) -> usize {
        self.cache.count()
    }

    pub async fn load_cursor(&self, wiki: &str) -> Option<Cursor> {
        self.read_wikis()
            .iter()
            .find(|entry| entry.domain == wiki)
            .and_then(|entry| entry.cursor.clone())
    }

    pub async fn save_cursor(&self, wiki: &str, cursor: &Cursor) -> Result<()> {
        {
            let mut wikis = self.write_wikis();
            let Some(entry) = wikis.iter_mut().find(|entry| entry.domain == wiki) else {
                bail!("Wiki {wiki} is not configured");
            };
            entry.cursor = Some(cursor.clone());
        }
        let wikis = self.read_wikis().clone();
        write_wikis(&self.wikis_path, &wikis).await
    }

    fn read_wikis(&self) -> RwLockReadGuard<'_, Vec<Wiki>> {
        self.wikis
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_wikis(&self) -> RwLockWriteGuard<'_, Vec<Wiki>> {
        self.wikis
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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

    let mut seen = std::collections::HashSet::new();
    let mut records = VecDeque::new();
    let mut valid = Vec::new();
    for line in &kept {
        let Ok(detection) = serde_json::from_str::<Detection>(line) else {
            continue;
        };
        if !seen.insert((detection.wiki.clone(), detection.id.clone())) {
            continue;
        }
        records.push_front(detection);
        valid.push(line.clone());
    }
    let compact = (total > max_records.saturating_mul(2)).then_some(valid);
    Ok((records, compact))
}

async fn load_wikis(path: &Path) -> Result<Vec<Wiki>> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("Reading {}", path.display()))?;
    let mut wikis: Vec<Wiki> = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid JSON in {}", path.display()))?;
    sort_wikis(&mut wikis);
    Ok(wikis)
}

async fn load_wikis_or_default(path: &Path) -> Result<Vec<Wiki>> {
    match load_wikis(path).await {
        Ok(wikis) => Ok(wikis),
        Err(error) if !path.exists() => {
            let _ = error;
            Ok(Wiki::defaults())
        }
        Err(error) => Err(error),
    }
}

fn sort_wikis(wikis: &mut [Wiki]) {
    wikis.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.domain.cmp(&b.domain))
    });
}

async fn write_wikis(path: &Path, wikis: &[Wiki]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(wikis).context("Serializing wiki list")?;
    tokio::fs::write(&temporary, contents)
        .await
        .with_context(|| format!("Writing {}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, StoreConfig};
    use crate::store::{Filter, detection};
    use tempfile::TempDir;

    const COMMONS: &str = "commons.wikimedia.org";

    fn config(dir: &TempDir) -> Config {
        Config {
            store: StoreConfig {
                file: dir.path().join("detections.jsonl"),
                wikis_file: dir.path().join("wikis.json"),
                ..StoreConfig::default()
            },
            max_records: 10,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn detects_are_persisted_and_loaded() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        let store = FileStore::open(&config).await.unwrap();
        assert!(store.add(detection(COMMONS, "a", 90)).await.unwrap());
        assert!(!store.add(detection(COMMONS, "a", 90)).await.unwrap());
        assert!(store.add(detection(COMMONS, "b", 40)).await.unwrap());
        assert_eq!(store.count(), 2);
        drop(store);

        let store = FileStore::open(&config).await.unwrap();
        assert_eq!(store.count(), 2);
        assert!(store.is_seen(COMMONS, "a"));
        let recent = store.recent(&Filter::default());
        assert_eq!(recent[0].id, "b", "newest record comes first");
    }

    #[tokio::test]
    async fn the_same_file_on_two_wikis_is_stored_twice() {
        let dir = TempDir::new().unwrap();
        let store = FileStore::open(&config(&dir)).await.unwrap();
        assert!(store.add(detection(COMMONS, "same", 90)).await.unwrap());
        assert!(
            store
                .add(detection("en.wikipedia.org", "same", 90))
                .await
                .unwrap()
        );
        assert_eq!(store.count(), 2);
    }

    #[tokio::test]
    async fn filter_matches_text_score_and_wiki() {
        let dir = TempDir::new().unwrap();
        let store = FileStore::open(&config(&dir)).await.unwrap();
        store.add(detection(COMMONS, "a", 90)).await.unwrap();
        store
            .add(detection("de.wikipedia.org", "b", 20))
            .await
            .unwrap();
        let filter = Filter {
            min_score: 50,
            ..Filter::default()
        };
        assert_eq!(store.recent(&filter).len(), 1);
        let filter = Filter {
            wiki: Some("de.wikipedia.org".into()),
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
    async fn cursors_round_trip_per_wiki() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        let store = FileStore::open(&config).await.unwrap();
        assert_eq!(store.wikis().len(), 3);
        let cursor = Cursor {
            timestamp: "2026-01-01T00:00:00Z".into(),
            rc_id: 42,
        };
        store.save_cursor(COMMONS, &cursor).await.unwrap();
        assert_eq!(store.load_cursor(COMMONS).await, Some(cursor));
        assert_eq!(store.load_cursor("en.wikipedia.org").await, None);

        drop(store);
        let store = FileStore::open(&config).await.unwrap();
        assert_eq!(store.load_cursor(COMMONS).await.unwrap().rc_id, 42);
    }

    #[tokio::test]
    async fn memory_is_bounded() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            max_records: 2,
            ..config(&dir)
        };
        let store = FileStore::open(&config).await.unwrap();
        for id in ["a", "b", "c"] {
            store.add(detection(COMMONS, id, 90)).await.unwrap();
        }
        assert_eq!(store.count(), 2);
        assert!(
            store.is_seen(COMMONS, "a"),
            "seen set is independent of the display ring"
        );
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        tokio::fs::write(&config.store.file, "not json\n")
            .await
            .unwrap();
        let store = FileStore::open(&config).await.unwrap();
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn store_file_is_compacted() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            max_records: 2,
            ..config(&dir)
        };
        let store = FileStore::open(&config).await.unwrap();
        for index in 0..10 {
            store
                .add(detection(COMMONS, &format!("id{index}"), 90))
                .await
                .unwrap();
        }
        drop(store);

        let store = FileStore::open(&config).await.unwrap();
        assert_eq!(store.count(), 2);
        let contents = tokio::fs::read_to_string(&config.store.file).await.unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[tokio::test]
    async fn read_only_store_reloads() {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        let writer = FileStore::open(&config).await.unwrap();
        writer.add(detection(COMMONS, "a", 90)).await.unwrap();
        drop(writer);

        let reader = FileStore::open_read_only(&config).await.unwrap();
        assert_eq!(reader.count(), 1);
        assert!(reader.add(detection(COMMONS, "b", 90)).await.is_err());

        let writer = FileStore::open(&config).await.unwrap();
        writer.add(detection(COMMONS, "b", 90)).await.unwrap();
        writer
            .save_cursor(
                COMMONS,
                &Cursor {
                    timestamp: "2026-01-01T00:00:00Z".into(),
                    rc_id: 7,
                },
            )
            .await
            .unwrap();
        drop(writer);
        reader.reload().await.unwrap();
        assert_eq!(reader.count(), 2);
        assert_eq!(reader.load_cursor(COMMONS).await.unwrap().rc_id, 7);
    }
}
