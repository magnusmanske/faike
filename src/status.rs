//! Shared run-time status for the web UI and `/api/status`.

use crate::mediawiki::Cursor;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct WikiStatus {
    pub wiki: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_poll: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub uptime_seconds: u64,
    pub polls: u64,
    pub uploads_seen: u64,
    pub files_analyzed: u64,
    pub detections: u64,
    pub stored: usize,
    pub wikis: Vec<WikiStatus>,
    pub last_error: Option<String>,
}

pub struct Status {
    started: Instant,
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    polls: u64,
    uploads_seen: u64,
    files_analyzed: u64,
    detections: u64,
    stored: usize,
    wikis: BTreeMap<String, WikiStatus>,
    last_error: Option<String>,
}

impl Inner {
    fn wiki(&mut self, wiki: &str) -> &mut WikiStatus {
        self.wikis
            .entry(wiki.to_string())
            .or_insert_with(|| WikiStatus {
                wiki: wiki.to_string(),
                cursor: None,
                last_poll: None,
                last_error: None,
            })
    }
}

impl Status {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Record one successful poll of `wiki`.
    pub fn record_poll(&self, wiki: &str, uploads: usize, cursor: Option<&Cursor>) {
        let mut inner = self.write();
        inner.polls += 1;
        inner.uploads_seen += uploads as u64;
        let entry = inner.wiki(wiki);
        entry.last_error = None;
        if let Some(cursor) = cursor {
            entry.last_poll = Some(cursor.timestamp.clone());
            entry.cursor = Some(cursor.clone());
        }
    }

    /// Seed the cursor shown before this process has polled (used by `serve`).
    pub fn set_cursor(&self, wiki: &str, cursor: Option<&Cursor>) {
        let mut inner = self.write();
        let entry = inner.wiki(wiki);
        entry.cursor = cursor.cloned();
        entry.last_poll = cursor.map(|cursor| cursor.timestamp.clone());
    }

    pub fn clear_error(&self) {
        self.write().last_error = None;
    }

    pub fn record_analyzed(&self, count: usize) {
        self.write().files_analyzed += count as u64;
    }

    pub fn record_detection(&self) {
        self.write().detections += 1;
    }

    pub fn set_stored(&self, stored: usize) {
        self.write().stored = stored;
    }

    /// An error that is not tied to one wiki, e.g. a store reload failure.
    pub fn set_error(&self, error: impl Into<String>) {
        self.write().last_error = Some(error.into());
    }

    pub fn set_wiki_error(&self, wiki: &str, error: impl Into<String>) {
        self.write().wiki(wiki).last_error = Some(error.into());
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.read();
        Snapshot {
            uptime_seconds: self.started.elapsed().as_secs(),
            polls: inner.polls,
            uploads_seen: inner.uploads_seen,
            files_analyzed: inner.files_analyzed,
            detections: inner.detections,
            stored: inner.stored,
            wikis: inner.wikis.values().cloned().collect(),
            last_error: inner.last_error.clone(),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_advance_per_wiki() {
        let status = Status::new();
        status.record_poll(
            "commons.wikimedia.org",
            3,
            Some(&Cursor {
                timestamp: "2026-01-01T00:00:00Z".into(),
                rc_id: 1,
            }),
        );
        status.record_poll("de.wikipedia.org", 2, None);
        status.record_analyzed(2);
        status.record_detection();
        let snapshot = status.snapshot();
        assert_eq!(snapshot.polls, 2);
        assert_eq!(snapshot.uploads_seen, 5);
        assert_eq!(snapshot.files_analyzed, 2);
        assert_eq!(snapshot.detections, 1);
        assert_eq!(snapshot.wikis.len(), 2);
        assert_eq!(
            snapshot.wikis[0].cursor.as_ref().map(|cursor| cursor.rc_id),
            Some(1)
        );
        assert!(snapshot.last_error.is_none());
    }

    #[test]
    fn errors_are_scoped_and_cleared_by_a_successful_poll() {
        let status = Status::new();
        status.set_error("store down");
        status.set_wiki_error("en.wikipedia.org", "maxlag");
        let snapshot = status.snapshot();
        assert_eq!(snapshot.last_error.as_deref(), Some("store down"));
        assert_eq!(
            snapshot.wikis[0].last_error.as_deref(),
            Some("maxlag"),
            "wiki errors are separate"
        );
        status.clear_error();
        status.record_poll("en.wikipedia.org", 0, None);
        let snapshot = status.snapshot();
        assert!(snapshot.last_error.is_none());
        assert!(snapshot.wikis[0].last_error.is_none());
    }
}
