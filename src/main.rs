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
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const USAGE: &str = "Usage: faike <run|watch|serve|scan|calibrate> [config.json | files]\n\
    run [config]        watch uploads and serve the web UI\n\
    watch [config]      watch uploads only (bot process)\n\
    serve [config]      serve the web UI from an existing detection store\n\
    scan <files>        analyse local files and print JSON\n\
    calibrate <files>   print watermark detector metrics for local files";

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
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("Unknown command {other:?}.\n{USAGE}"),
    }
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
    let trustmark = detect::watermark::trustmark::report(image, &config.detect.watermark);
    let output = json!({
        "file": path,
        "mime": mime,
        "size": bytes.len(),
        "dwt_dct": dwt_dct,
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
