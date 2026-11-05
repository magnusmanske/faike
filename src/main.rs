mod abuse_filter;
mod config;
mod detect;
mod mediawiki;
mod model;
mod patterns;
mod status;
mod store;
mod telemetry;
mod watcher;
mod web;
mod wiki;

use anyhow::{Context, Result, bail};
use config::Config;
use detect::{Input, Metadata};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const USAGE: &str = "Usage: faike <run|watch|serve|scan|calibrate|corpus> [config.json | files | index.json]\n\
    run [config]        watch uploads and serve the web UI\n\
    watch [config]      watch uploads only (bot process)\n\
    serve [config]      serve the web UI from an existing detection store\n\
    scan <files>        analyse local files and print JSON\n\
    calibrate <files>   print watermark and classifier metrics for local files\n\
    corpus <index.json> [--bytes-only]\n\
                        evaluate detectors over an index with MediaWiki metadata";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("faike: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    match command {
        "run" => {
            telemetry::start_bot();
            let config = Arc::new(load_config(&args)?);
            config.validate_for_run()?;
            let store = Arc::new(store::Store::open(&config).await?);
            let status = Arc::new(status::Status::new());
            for wiki in store.wikis() {
                status.set_cursor(&wiki.domain, wiki.cursor.as_ref());
            }
            let watcher_config = config.clone();
            let watcher_store = store.clone();
            let watcher_status = status.clone();
            tokio::spawn(async move {
                watcher::run(watcher_config, watcher_store, watcher_status).await;
            });
            web::run(config, store, status, false).await
        }
        "watch" => {
            telemetry::start_bot();
            let config = Arc::new(load_config(&args)?);
            config.validate_for_run()?;
            let store = Arc::new(store::Store::open(&config).await?);
            let status = Arc::new(status::Status::new());
            for wiki in store.wikis() {
                status.set_cursor(&wiki.domain, wiki.cursor.as_ref());
            }
            tokio::select! {
                _ = watcher::run(config, store, status) => {},
                _ = web::shutdown_signal() => {},
            }
            Ok(())
        }
        "serve" => {
            telemetry::start_web();
            let config = Arc::new(load_config(&args)?);
            let store = Arc::new(store::Store::open_read_only(&config).await?);
            let status = Arc::new(status::Status::new());
            for wiki in store.wikis() {
                status.set_cursor(&wiki.domain, wiki.cursor.as_ref());
            }
            web::run(config, store, status, true).await
        }
        "scan" => {
            let files = &args[2..];
            if files.is_empty() {
                bail!(USAGE);
            }
            for file in files {
                scan(file).await?;
            }
            Ok(())
        }
        "calibrate" => {
            let files = &args[2..];
            if files.is_empty() {
                bail!(USAGE);
            }
            for file in files {
                calibrate(file).await?;
            }
            Ok(())
        }
        "corpus" => {
            let (index, bytes_only) = parse_corpus_args(&args[2..])?;
            corpus(index, bytes_only).await
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("Unknown command {other:?}.\n{USAGE}"),
    }
}

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    path: String,
    #[serde(default)]
    extmetadata: BTreeMap<String, Option<String>>,
    #[serde(default)]
    wikitext: String,
    #[serde(default)]
    comment: String,
}

/// Parse `corpus <index.json> [--bytes-only]`.
fn parse_corpus_args(args: &[String]) -> Result<(&str, bool)> {
    let mut index = None;
    let mut bytes_only = false;
    for arg in args {
        match arg.as_str() {
            "--bytes-only" => bytes_only = true,
            other if other.starts_with("--") => bail!("Unknown option {other:?}.\n{USAGE}"),
            other => {
                if index.is_some() {
                    bail!("Only one index file is supported.\n{USAGE}");
                }
                index = Some(other);
            }
        }
    }
    let index = index.ok_or_else(|| anyhow::anyhow!(USAGE))?;
    Ok((index, bytes_only))
}

/// Evaluate the detectors over an index of `{title: {path, extmetadata, ...}}`.
///
/// `--bytes-only` ignores the MediaWiki metadata, which simulates an upload
/// whose page does not declare AI. Output is one JSON object per file on stdout;
/// a summary goes to stderr.
async fn corpus(index_path: &str, bytes_only: bool) -> Result<()> {
    let raw =
        std::fs::read_to_string(index_path).with_context(|| format!("Cannot read {index_path}"))?;
    let index: BTreeMap<String, CorpusEntry> =
        serde_json::from_str(&raw).with_context(|| format!("Invalid JSON in {index_path}"))?;
    let config = optional_config();
    let mut files = 0usize;
    let mut flagged = 0usize;
    let mut above_min = 0usize;
    let mut sources: BTreeMap<&'static str, usize> = BTreeMap::new();

    for (title, entry) in index {
        let bytes = match std::fs::read(&entry.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("faike: skipping {}: {error}", entry.path);
                continue;
            }
        };
        let mime = mime_for(Path::new(&entry.path), &bytes);
        let metadata = if bytes_only {
            Metadata::default()
        } else {
            Metadata {
                extmetadata: entry
                    .extmetadata
                    .into_iter()
                    .filter_map(|(key, value)| value.map(|value| (key, value)))
                    .collect(),
                wikitext: entry.wikitext,
                comment: entry.comment,
            }
        };
        let input = Input {
            bytes: &bytes,
            mime: &mime,
            metadata: &metadata,
        };
        let signals = detect::detect(&input, &config.detect);
        let score = model::score(&signals);
        let level = model::Level::from_score(score);
        if score > 0 {
            flagged += 1;
            let file_sources: std::collections::BTreeSet<&'static str> = signals
                .iter()
                .map(|signal| signal.source.as_str())
                .collect();
            for source in file_sources {
                *sources.entry(source).or_default() += 1;
            }
        }
        if score >= config.min_score {
            above_min += 1;
        }
        files += 1;
        println!(
            "{}",
            serde_json::to_string(&json!({
                "title": title,
                "file": entry.path,
                "mime": mime,
                "size": bytes.len(),
                "score": score,
                "level": level.as_str(),
                "signals": signals,
            }))?
        );
    }
    eprintln!(
        "faike corpus: {files} files, {flagged} with signals, {above_min} at or above min_score {}",
        config.min_score
    );
    eprintln!("faike corpus: files per source: {sources:?}");
    Ok(())
}

fn load_config(args: &[String]) -> Result<Config> {
    let path = args
        .get(2)
        .cloned()
        .or_else(|| std::env::var("FAIKE_CONFIG").ok())
        .unwrap_or_else(|| "config.json".to_string());
    Config::load(path)
}

async fn scan(path: &str) -> Result<()> {
    let path_ref = Path::new(path);
    let bytes =
        std::fs::read(path_ref).with_context(|| format!("Cannot read {}", path_ref.display()))?;
    let mime = mime_for(path_ref, &bytes);
    let metadata = Metadata::default();
    let config = optional_config();
    let input = Input {
        bytes: &bytes,
        mime: &mime,
        metadata: &metadata,
    };
    let signals = detect::detect(&input, &config.detect);
    let score = model::score(&signals);
    let level = model::Level::from_score(score);
    let output = json!({
        "file": path,
        "mime": mime,
        "size": bytes.len(),
        "score": score,
        "level": level.as_str(),
        "signals": signals,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn calibrate(path: &str) -> Result<()> {
    let path_ref = Path::new(path);
    let bytes =
        std::fs::read(path_ref).with_context(|| format!("Cannot read {}", path_ref.display()))?;
    let mime = mime_for(path_ref, &bytes);
    let config = optional_config();
    let Some(image) = detect::watermark::load_image(&bytes) else {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "file": path,
                "mime": mime,
                "skipped": "image could not be decoded or is too large"
            }))?
        );
        return Ok(());
    };
    let dwt_dct = detect::watermark::dwt_dct::report(&image.to_rgb8());
    let classifier = detect::classifier::report(&image, &config.detect.classifier);
    let trustmark = detect::watermark::trustmark::report(image, &config.detect.watermark);
    let output = json!({
        "file": path,
        "mime": mime,
        "size": bytes.len(),
        "dwt_dct": dwt_dct,
        "classifier": classifier,
        "trustmark": trustmark,
        "thresholds": {
            "dwt_match": config.detect.watermark.dwt_match_threshold,
            "dwt_margin": config.detect.watermark.dwt_margin_threshold,
            "dwt_generic_margin": config.detect.watermark.dwt_generic_margin_threshold,
            "dwt_lock": config.detect.watermark.dwt_lock_threshold,
        }
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn optional_config() -> Config {
    let path = std::env::var("FAIKE_CONFIG").unwrap_or_else(|_| "config.json".to_string());
    Config::load(path).unwrap_or_default()
}

fn mime_for(path: &Path, bytes: &[u8]) -> String {
    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_string();
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        "avif" => "image/avif",
        "heic" | "heif" => "image/heif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "webm" => "video/webm",
        "mp4" | "m4v" => "video/mp4",
        "ogg" | "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
    .to_string()
}
