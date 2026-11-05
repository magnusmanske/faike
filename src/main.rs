mod abuse_filter;
mod config;
mod detect;
mod mediawiki;
mod model;
mod patterns;
mod status;
mod store;
mod watcher;
mod web;

use anyhow::{Context, Result, bail};
use config::Config;
use detect::{Input, Metadata};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const USAGE: &str = "Usage: aimer <run|watch|serve|scan> [config.json | file]\n\
    run [config]    watch uploads and serve the web UI\n\
    watch [config]  watch uploads only (bot process)\n\
    serve [config]  serve the web UI from an existing detection store\n\
    scan <files>    analyse local files and print JSON";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("aimer: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    match command {
        "run" => {
            let config = Arc::new(load_config(&args)?);
            config.validate_for_run()?;
            let store = Arc::new(store::Store::open(&config).await?);
            let status = Arc::new(status::Status::new());
            let watcher_config = config.clone();
            let watcher_store = store.clone();
            let watcher_status = status.clone();
            tokio::spawn(async move {
                watcher::run(watcher_config, watcher_store, watcher_status).await;
            });
            web::run(config, store, status).await
        }
        "watch" => {
            let config = Arc::new(load_config(&args)?);
            config.validate_for_run()?;
            let store = Arc::new(store::Store::open(&config).await?);
            let status = Arc::new(status::Status::new());
            tokio::select! {
                _ = watcher::run(config, store, status) => {},
                _ = web::shutdown_signal() => {},
            }
            Ok(())
        }
        "serve" => {
            let config = Arc::new(load_config(&args)?);
            let store = Arc::new(store::Store::open(&config).await?);
            let status = Arc::new(status::Status::new());
            if let Some(cursor) = store.load_cursor().await {
                status.set_cursor(Some(&cursor));
            }
            web::run(config, store, status).await
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
        .or_else(|| std::env::var("AIMER_CONFIG").ok())
        .unwrap_or_else(|| "config.json".to_string());
    Config::load(path)
}

async fn scan(path: &str) -> Result<()> {
    let path_ref = Path::new(path);
    let bytes =
        std::fs::read(path_ref).with_context(|| format!("Cannot read {}", path_ref.display()))?;
    let mime = mime_for(path_ref, &bytes);
    let metadata = Metadata::default();
    let input = Input {
        bytes: &bytes,
        mime: &mime,
        metadata: &metadata,
    };
    let signals = detect::detect(&input, &config::DetectConfig::default());
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
