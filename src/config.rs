//! Configuration.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// MediaWiki Action API endpoint. Defaults to Wikimedia Commons.
    pub api_url: String,
    /// Contactable User-Agent, required for the watcher (Wikimedia policy).
    pub user_agent: String,
    /// Seconds between recent-changes polls.
    pub poll_interval_sec: u64,
    /// Uploads to analyse on the first run. `0` starts at the current time.
    pub backfill_events: usize,
    /// Recent-changes rows per request (API maximum for normal users is 500).
    pub max_batch: usize,
    /// Namespace to watch for uploads. Wikimedia Commons uses 6 (File).
    pub rc_namespace: i32,
    /// Skip files larger than this.
    pub max_download_bytes: u64,
    /// Concurrent file downloads.
    pub download_concurrency: usize,
    /// Only download these MIME prefixes. Metadata is still analysed for all.
    pub download_mime_prefixes: Vec<String>,
    /// Minimum score stored and shown.
    pub min_score: u8,
    /// Minimum score included in the exact-SHA1 AbuseFilter list.
    pub abuse_filter_min_score: u8,
    pub store: StoreConfig,
    /// Detections kept in memory for the web UI.
    pub max_records: usize,
    /// Web listen address. Use `0.0.0.0` on Toolforge; keep loopback locally.
    pub web_address: String,
    pub web_port: u16,
    /// Browser auto-refresh interval; `0` disables it.
    pub refresh_sec: u64,
    /// HTTP timeout for API and file requests.
    pub request_timeout_sec: u64,
    /// Cap on fetched wikitext, to keep pathological pages out of memory.
    pub max_wikitext_bytes: usize,
    pub detect: DetectConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: "https://commons.wikimedia.org/w/api.php".to_string(),
            user_agent: String::new(),
            poll_interval_sec: 15,
            backfill_events: 50,
            max_batch: 500,
            rc_namespace: 6,
            max_download_bytes: 64 * 1024 * 1024,
            download_concurrency: 2,
            download_mime_prefixes: vec!["image/".to_string()],
            min_score: 30,
            abuse_filter_min_score: 80,
            store: StoreConfig::default(),
            max_records: 2000,
            web_address: "127.0.0.1".to_string(),
            web_port: 8000,
            refresh_sec: 60,
            request_timeout_sec: 60,
            max_wikitext_bytes: 64 * 1024,
            detect: DetectConfig::default(),
        }
    }
}

/// Where detections are persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// Append-only JSONL, plus a JSON cursor file. One writer.
    #[default]
    File,
    /// The tool's ToolsDB database. Shared by the bot and the web service.
    Toolsdb,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StoreConfig {
    pub kind: StoreKind,
    /// JSONL store for the `file` backend.
    pub file: PathBuf,
    /// Recent-changes cursor for the `file` backend.
    pub cursor_file: PathBuf,
    /// ToolsDB database. Empty derives `<tool user>__faike_p`.
    pub database: String,
    /// Credentials file; `None` lets the `toolforge` crate search the usual
    /// locations (`TOOL_*` variables, `$TOOL_DATA_DIR`, `$HOME`).
    pub replica_my_cnf: Option<PathBuf>,
    /// Connections held per ToolsDB server.
    pub pool_max: usize,
    pub connect_timeout_sec: u64,
    /// Server-side query limit; `0` uses the server default.
    pub max_statement_time_sec: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            kind: StoreKind::File,
            file: PathBuf::from("detections.jsonl"),
            cursor_file: PathBuf::from("cursor.json"),
            database: String::new(),
            replica_my_cnf: None,
            pool_max: 3,
            connect_timeout_sec: 10,
            max_statement_time_sec: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DetectConfig {
    pub c2pa: bool,
    pub exif: bool,
    pub xmp: bool,
    pub png_text: bool,
    /// Raw-bytes fallback for malformed or unusual containers.
    pub container: bool,
    /// Commons `extmetadata`, file-page wikitext and upload comment.
    pub metadata: bool,
    /// Pixel-level watermark detection (expensive; independent of metadata).
    pub watermark: WatermarkConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatermarkConfig {
    pub enabled: bool,
    /// Stable Diffusion `invisible-watermark` DWT-DCT payloads.
    pub dwt_dct: bool,
    /// Adobe TrustMark neural watermark decoder.
    pub trustmark: bool,
    /// Directory containing `encoder_Q.onnx` and `decoder_Q.onnx`.
    pub models_dir: PathBuf,
    /// TrustMark model variant (`Q` or `P`).
    pub trustmark_variant: String,
    /// TrustMark error-correction schema (`Bch5`, `Bch4`, `Bch3`, `BchSuper`).
    pub trustmark_version: String,
    /// Minimum fraction of known DWT-DCT payload bits that must match.
    pub dwt_match_threshold: f64,
    /// Minimum bit-stability margin for a known DWT-DCT payload.
    pub dwt_margin_threshold: f64,
    /// Minimum bit-stability margin to report an unknown DWT-DCT-like mark.
    pub dwt_generic_margin_threshold: f64,
    /// Minimum fraction of coefficients near the encoder's quantization centers.
    pub dwt_lock_threshold: f64,
}

impl Default for WatermarkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dwt_dct: true,
            trustmark: true,
            models_dir: PathBuf::from("models"),
            trustmark_variant: "Q".to_string(),
            trustmark_version: "Bch5".to_string(),
            dwt_match_threshold: 0.85,
            dwt_margin_threshold: 0.35,
            dwt_generic_margin_threshold: 0.80,
            dwt_lock_threshold: 0.60,
        }
    }
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            c2pa: true,
            exif: true,
            xmp: true,
            png_text: true,
            container: true,
            metadata: true,
            watermark: WatermarkConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        let mut config: Self = serde_json::from_str(&raw)
            .with_context(|| format!("Invalid JSON in {}", path.display()))?;
        config.clamp();
        Ok(config)
    }

    /// Clamp values that would otherwise waste API quota or break the UI.
    fn clamp(&mut self) {
        self.poll_interval_sec = self.poll_interval_sec.max(1);
        self.max_batch = self.max_batch.clamp(1, 500);
        self.download_concurrency = self.download_concurrency.clamp(1, 8);
        self.min_score = self.min_score.min(100);
        self.abuse_filter_min_score = self.abuse_filter_min_score.min(100);
        self.max_records = self.max_records.max(1);
        self.store.pool_max = self.store.pool_max.clamp(1, 10);
        self.store.connect_timeout_sec = self.store.connect_timeout_sec.clamp(1, 120);
        self.store.max_statement_time_sec = self.store.max_statement_time_sec.min(600);
        self.max_wikitext_bytes = self.max_wikitext_bytes.clamp(1024, 4 * 1024 * 1024);
        self.request_timeout_sec = self.request_timeout_sec.clamp(5, 600);
        self.detect.watermark.dwt_match_threshold =
            self.detect.watermark.dwt_match_threshold.clamp(0.0, 1.0);
        self.detect.watermark.dwt_margin_threshold =
            self.detect.watermark.dwt_margin_threshold.clamp(0.0, 1.0);
        self.detect.watermark.dwt_generic_margin_threshold = self
            .detect
            .watermark
            .dwt_generic_margin_threshold
            .clamp(0.0, 1.0);
        self.detect.watermark.dwt_lock_threshold =
            self.detect.watermark.dwt_lock_threshold.clamp(0.0, 1.0);
    }

    /// Checks needed before starting the watcher. The web server itself can run
    /// without a User-Agent.
    pub fn validate_for_run(&self) -> Result<()> {
        if self.user_agent.trim().is_empty() {
            bail!(
                "Set `user_agent` in the config to a URL or e-mail address that identifies you; \
                 Wikimedia requires a contactable User-Agent"
            );
        }
        if !self.api_url.starts_with("http://") && !self.api_url.starts_with("https://") {
            bail!("`api_url` must be an http(s) URL");
        }
        if self.download_mime_prefixes.is_empty() {
            bail!(
                "`download_mime_prefixes` must not be empty; use [\"image/\"] to analyse images only"
            );
        }
        Ok(())
    }

    /// True if a file of this MIME type should be downloaded for byte-level
    /// analysis.
    pub fn should_download(&self, mime: &str, size: u64) -> bool {
        size > 0
            && size <= self.max_download_bytes
            && self
                .download_mime_prefixes
                .iter()
                .any(|prefix| mime.starts_with(prefix.as_str()))
    }

    pub fn listen_addr(&self) -> String {
        let address = std::env::var("AIMER_ADDRESS").unwrap_or_else(|_| self.web_address.clone());
        let port = std::env::var("AIMER_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(self.web_port);
        format!("{address}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert_eq!(config.api_url, "https://commons.wikimedia.org/w/api.php");
        assert_eq!(config.max_batch, 500);
        assert!(config.detect.c2pa);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = serde_json::from_str(r#"{"poll_interval_sec": 5}"#).unwrap();
        assert_eq!(config.poll_interval_sec, 5);
        assert_eq!(config.max_batch, 500);
        assert!(config.detect.exif);
    }

    #[test]
    fn values_are_clamped() {
        let mut config: Config = serde_json::from_str(
            r#"{"max_batch": 9999, "download_concurrency": 0, "min_score": 200}"#,
        )
        .unwrap();
        config.clamp();
        assert_eq!(config.max_batch, 500);
        assert_eq!(config.download_concurrency, 1);
        assert_eq!(config.min_score, 100);
    }

    #[test]
    fn store_section_defaults_and_clamps() {
        let config: Config = serde_json::from_str(r#"{"store":{"kind":"toolsdb"}}"#).unwrap();
        assert_eq!(config.store.kind, StoreKind::Toolsdb);
        assert_eq!(config.store.pool_max, 3);

        let mut config: Config = serde_json::from_str(
            r#"{"store":{"pool_max":0,"connect_timeout_sec":0,"max_statement_time_sec":9999}}"#,
        )
        .unwrap();
        config.clamp();
        assert_eq!(config.store.pool_max, 1);
        assert_eq!(config.store.connect_timeout_sec, 1);
        assert_eq!(config.store.max_statement_time_sec, 600);
    }

    #[test]
    fn run_requires_a_user_agent() {
        let config = Config::default();
        assert!(config.validate_for_run().is_err());
    }

    #[test]
    fn download_policy_respects_mime_and_size() {
        let config = Config::default();
        assert!(config.should_download("image/jpeg", 1024));
        assert!(!config.should_download("video/webm", 1024));
        assert!(!config.should_download("image/jpeg", config.max_download_bytes + 1));
    }
}
