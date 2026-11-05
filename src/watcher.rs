//! The upload watcher: recent changes -> metadata -> bytes -> detections.
//!
//! One poll cycle visits every enabled wiki from the store. The wiki list is
//! refreshed each cycle, so adding, disabling or removing a wiki in the
//! database takes effect without a restart.

use crate::config::Config;
use crate::detect::{self, Input, Metadata};
use crate::mediawiki::{ApiClient, Cursor, FileInfo, UploadEvent};
use crate::model::Detection;
use crate::status::Status;
use crate::store::Store;
use crate::telemetry;
use crate::wiki::Wiki;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Shorter pause between cycles when an API reported more pages.
const MORE_WORK_PAUSE: Duration = Duration::from_secs(1);

struct Job {
    event: UploadEvent,
    info: FileInfo,
    metadata: Metadata,
    id: String,
    download: bool,
}

pub async fn run(config: Arc<Config>, store: Arc<Store>, status: Arc<Status>) {
    loop {
        if let Err(error) = store.refresh_wikis().await {
            status.set_error(format!("Wiki list refresh failed: {error:#}"));
            eprintln!("faike: wiki list refresh failed: {error:#}");
        }
        let wikis = store.wikis();
        telemetry::set_wikis(wikis.len());
        let mut more_work = false;
        for wiki in wikis.into_iter().filter(|wiki| wiki.enabled) {
            match poll_wiki(&config, &store, &status, &wiki).await {
                Ok(more) => more_work |= more,
                Err(error) => {
                    status.set_wiki_error(&wiki.domain, format!("{error:#}"));
                    eprintln!("faike: poll of {} failed: {error:#}", wiki.domain);
                }
            }
            status.set_stored(store.count());
            telemetry::set_stored(store.count());
        }
        let pause = if more_work {
            MORE_WORK_PAUSE
        } else {
            Duration::from_secs(config.poll_interval_sec)
        };
        tokio::time::sleep(pause).await;
    }
}

async fn poll_wiki(
    config: &Arc<Config>,
    store: &Arc<Store>,
    status: &Arc<Status>,
    wiki: &Wiki,
) -> Result<bool> {
    let client = ApiClient::new(config, wiki)?;
    let mut cursor = store.load_cursor(&wiki.domain).await;
    poll_once(config, &client, store, status, wiki, &mut cursor).await
}

async fn poll_once(
    config: &Arc<Config>,
    client: &ApiClient,
    store: &Arc<Store>,
    status: &Arc<Status>,
    wiki: &Wiki,
    cursor: &mut Option<Cursor>,
) -> Result<bool> {
    if cursor.is_none() {
        let batch = client.latest_uploads(config.backfill_events).await?;
        status.clear_error();
        let uploads = batch.uploads.len();
        if uploads > 0 {
            eprintln!(
                "faike: backfilling {uploads} upload event(s) on {}",
                wiki.domain
            );
        }
        if !batch.uploads.is_empty() {
            process_batch(config, client, store, status, wiki, batch.uploads).await?;
        }
        if let Some(new_cursor) = &batch.cursor {
            store.save_cursor(&wiki.domain, new_cursor).await?;
            *cursor = Some(new_cursor.clone());
        }
        status.record_poll(&wiki.domain, uploads, cursor.as_ref());
        telemetry::record_poll(&wiki.domain, uploads);
        status.set_stored(store.count());
        if let Some(cursor) = cursor {
            eprintln!(
                "faike: started on {}, cursor {} / {}",
                wiki.domain, cursor.timestamp, cursor.rc_id
            );
        }
        return Ok(false);
    }

    let current = cursor.clone().unwrap_or_default();
    let batch = client.uploads_since(&current, config.max_batch).await?;
    status.clear_error();
    let uploads = batch.uploads.len();
    if !batch.uploads.is_empty() {
        eprintln!("faike: {uploads} upload event(s) on {}", wiki.domain);
        process_batch(config, client, store, status, wiki, batch.uploads).await?;
    }
    if let Some(new_cursor) = &batch.cursor
        && *new_cursor > current
    {
        store.save_cursor(&wiki.domain, new_cursor).await?;
        *cursor = Some(new_cursor.clone());
    }
    status.record_poll(&wiki.domain, uploads, cursor.as_ref());
    telemetry::record_poll(&wiki.domain, uploads);
    status.set_stored(store.count());
    Ok(batch.more)
}

async fn process_batch(
    config: &Arc<Config>,
    client: &ApiClient,
    store: &Arc<Store>,
    status: &Arc<Status>,
    wiki: &Wiki,
    events: Vec<UploadEvent>,
) -> Result<()> {
    // The same file can be uploaded twice in one poll; analyse the latest
    // event. `imageinfo` always describes the current version.
    let mut unique: HashMap<String, UploadEvent> = HashMap::new();
    for event in events {
        unique.insert(event.title.clone(), event);
    }
    let mut events: Vec<UploadEvent> = unique.into_values().collect();
    events.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.rc_id.cmp(&b.rc_id))
    });

    for chunk in events.chunks(20) {
        let titles: Vec<String> = chunk.iter().map(|event| event.title.clone()).collect();
        let mut infos = client.imageinfo(&titles).await?;
        let wikitexts = match client.wikitext(&titles).await {
            Ok(wikitexts) => wikitexts,
            Err(error) => {
                status.set_wiki_error(&wiki.domain, format!("Wikitext fetch failed: {error:#}"));
                eprintln!("faike: wikitext fetch failed on {}: {error:#}", wiki.domain);
                HashMap::new()
            }
        };

        let mut jobs = Vec::new();
        for event in chunk {
            let Some(info) = infos.remove(&event.title) else {
                if std::env::var_os("FAIKE_DEBUG").is_some() {
                    eprintln!("faike: no imageinfo for {}", event.title);
                }
                continue;
            };
            let id = if info.sha1.is_empty() {
                format!("{}@{}", event.title, event.timestamp)
            } else {
                info.sha1.clone()
            };
            if store.is_seen(&wiki.domain, &id) {
                continue;
            }
            let download = config.should_download(&info.mime, info.size) && !info.url.is_empty();
            jobs.push(Job {
                event: event.clone(),
                metadata: Metadata {
                    extmetadata: info.extmetadata.clone(),
                    wikitext: wikitexts.get(&info.title).cloned().unwrap_or_default(),
                    comment: event.comment.clone(),
                },
                info,
                id,
                download,
            });
        }

        let results = stream::iter(jobs)
            .map(|job| {
                analyze_job(
                    config.clone(),
                    client.clone(),
                    store.clone(),
                    status.clone(),
                    wiki.domain.clone(),
                    job,
                )
            })
            .buffer_unordered(config.download_concurrency)
            .collect::<Vec<Result<()>>>()
            .await;
        for result in results {
            if let Err(error) = result {
                status.set_wiki_error(&wiki.domain, format!("{error:#}"));
                eprintln!("faike: analysis failed on {}: {error:#}", wiki.domain);
            }
        }
    }
    Ok(())
}

async fn analyze_job(
    config: Arc<Config>,
    client: ApiClient,
    store: Arc<Store>,
    status: Arc<Status>,
    wiki: String,
    job: Job,
) -> Result<()> {
    if store.is_seen(&wiki, &job.id) {
        return Ok(());
    }
    if std::env::var_os("FAIKE_DEBUG").is_some() {
        eprintln!(
            "faike: analysing {} on {} ({}, {} bytes, base36 {})",
            job.event.title, wiki, job.info.mime, job.info.size, job.event.sha1_base36
        );
    }
    let started = std::time::Instant::now();
    let bytes = if job.download {
        match client.download(&job.info.url).await {
            Ok(bytes) => bytes,
            Err(error) => {
                telemetry::record_download_failure();
                status.set_wiki_error(
                    &wiki,
                    format!("Download of {} failed: {error:#}", job.event.title),
                );
                eprintln!(
                    "faike: download of {} on {} failed: {error:#}",
                    job.event.title, wiki
                );
                Bytes::new()
            }
        }
    } else {
        Bytes::new()
    };
    if std::env::var_os("FAIKE_DEBUG").is_some() {
        eprintln!(
            "faike: downloaded {} bytes for {} in {:?}",
            bytes.len(),
            job.event.title,
            started.elapsed()
        );
    }

    let detect_config = config.detect.clone();
    let mime = job.info.mime.clone();
    let metadata = job.metadata;
    let signals = tokio::task::spawn_blocking(move || {
        let input = Input {
            bytes: &bytes,
            mime: &mime,
            metadata: &metadata,
        };
        detect::detect(&input, &detect_config)
    })
    .await
    .context("Detector task failed")?;
    if std::env::var_os("FAIKE_DEBUG").is_some() {
        eprintln!(
            "faike: detectors ran in {:?} for {}",
            started.elapsed(),
            job.event.title
        );
    }

    status.record_analyzed(1);
    telemetry::record_analysis(&wiki, started.elapsed().as_secs_f64());
    if signals.is_empty() {
        store.mark_seen(&wiki, &job.id);
        return Ok(());
    }

    let detection = Detection::new(
        job.id.clone(),
        wiki,
        job.event.title.clone(),
        job.event.timestamp.clone(),
        job.event.user.clone(),
        job.event.comment.clone(),
        job.info.url.clone(),
        job.info.description_url.clone(),
        job.info.mime.clone(),
        job.info.size,
        job.info.sha1.clone(),
        signals,
    );
    if detection.score < config.min_score {
        store.mark_seen(&detection.wiki, &job.id);
        return Ok(());
    }
    let tool = detection.tool().unwrap_or("unknown").to_string();
    let score = detection.score;
    let title = detection.title.clone();
    let detection_wiki = detection.wiki.clone();
    telemetry::record_detection(&detection.wiki, score);
    if store.add(detection).await? {
        status.record_detection();
        eprintln!("faike: detection {score} [{tool}] {title} on {detection_wiki}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_ordered_by_time() {
        let mut events = [
            UploadEvent {
                title: "File:B".into(),
                timestamp: "2026-01-01T00:00:02Z".into(),
                rc_id: 2,
                user: "u".into(),
                comment: String::new(),
                sha1_base36: String::new(),
            },
            UploadEvent {
                title: "File:A".into(),
                timestamp: "2026-01-01T00:00:01Z".into(),
                rc_id: 1,
                user: "u".into(),
                comment: String::new(),
                sha1_base36: String::new(),
            },
        ];
        events.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.rc_id.cmp(&b.rc_id))
        });
        assert_eq!(events[0].title, "File:A");
    }
}
