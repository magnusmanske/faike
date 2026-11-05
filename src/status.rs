//! Shared run-time status for the web UI and `/api/status`.

use crate::mediawiki::Cursor;
use serde::Serialize;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub uptime_seconds: u64,
    pub polls: u64,
    pub uploads_seen: u64,
    pub files_analyzed: u64,
    pub detections: u64,
    pub stored: usize,
    pub cursor: Option<Cursor>,
    pub last_poll: Option<String>,
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
    cursor: Option<Cursor>,
    last_poll: Option<String>,
    last_error: Option<String>,
}

impl Status {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: RwLock::new(Inner::default()),
        }
    }

    pub fn record_poll(&self, uploads: usize, cursor: Option<&Cursor>) {
        let mut inner = self.write();
        inner.polls += 1;
        inner.uploads_seen += uploads as u64;
        inner.last_poll = cursor.map(|cursor| cursor.timestamp.clone());
        if let Some(cursor) = cursor {
            inner.cursor = Some(cursor.clone());
        }
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

    pub fn set_cursor(&self, cursor: Option<&Cursor>) {
        let mut inner = self.write();
        inner.cursor = cursor.cloned();
        inner.last_poll = cursor.map(|cursor| cursor.timestamp.clone());
    }

    pub fn set_error(&self, error: impl Into<String>) {
        self.write().last_error = Some(error.into());
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
            cursor: inner.cursor.clone(),
            last_poll: inner.last_poll.clone(),
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
    fn counters_advance() {
        let status = Status::new();
        status.record_poll(
            3,
            Some(&Cursor {
                timestamp: "2026-01-01T00:00:00Z".into(),
                rc_id: 1,
            }),
        );
        status.record_analyzed(2);
        status.record_detection();
        let snapshot = status.snapshot();
        assert_eq!(snapshot.polls, 1);
        assert_eq!(snapshot.uploads_seen, 3);
        assert_eq!(snapshot.files_analyzed, 2);
        assert_eq!(snapshot.detections, 1);
        assert!(snapshot.last_error.is_none());
    }

    #[test]
    fn errors_are_cleared_by_a_successful_poll() {
        let status = Status::new();
        status.set_error("boom");
        assert_eq!(status.snapshot().last_error.as_deref(), Some("boom"));
        status.clear_error();
        assert!(status.snapshot().last_error.is_none());
    }
}
