//! Metrics and health pushes for both processes.
//!
//! Uses `wikimisc::telemetry`: the token comes from `$DASHBOARD_TOKEN`, and
//! without one the service runs normally. Metric names stay small and their
//! labels bounded (`wiki` is a domain from the `wikis` table; `operation` and
//! `route` are fixed sets).

#[cfg(feature = "telemetry")]
mod enabled {
    use wikimisc::metrics::{counter, gauge, histogram};

    /// Telemetry for the bot; handles SIGTERM and sends a final push.
    pub fn start_bot() {
        wikimisc::logging::init();
        wikimisc::telemetry::install_bot(env!("CARGO_PKG_VERSION"), None);
    }

    /// Telemetry for the web service. Our own graceful shutdown is not
    /// `wikimisc::server::serve`, so telemetry handles the signals and sends
    /// the final push itself.
    pub fn start_web() {
        wikimisc::logging::init();
        wikimisc::telemetry::install("web", env!("CARGO_PKG_VERSION"), None, true);
    }

    pub fn record_poll(wiki: &str, uploads: usize) {
        counter!("faike_polls_total", "wiki" => wiki.to_string()).increment(1);
        counter!("faike_uploads_total", "wiki" => wiki.to_string()).increment(uploads as u64);
    }

    pub fn record_analysis(wiki: &str, seconds: f64) {
        counter!("faike_files_analyzed_total", "wiki" => wiki.to_string()).increment(1);
        histogram!("faike_analysis_seconds", "wiki" => wiki.to_string()).record(seconds);
    }

    pub fn record_detection(wiki: &str, score: u8) {
        counter!("faike_detections_total", "wiki" => wiki.to_string()).increment(1);
        histogram!("faike_detection_score").record(f64::from(score));
    }

    pub fn record_download_failure() {
        counter!("faike_download_failures_total").increment(1);
    }

    pub fn record_store_error(operation: &'static str) {
        counter!("faike_store_errors_total", "operation" => operation).increment(1);
    }

    pub fn record_web_request(route: &'static str) {
        counter!("faike_web_requests_total", "route" => route).increment(1);
    }

    pub fn set_stored(stored: usize) {
        gauge!("faike_stored_detections").set(stored as f64);
    }

    pub fn set_wikis(count: usize) {
        gauge!("faike_wikis").set(count as f64);
    }
}

#[cfg(not(feature = "telemetry"))]
mod disabled {
    pub fn start_bot() {}
    pub fn start_web() {}
    pub fn record_poll(_wiki: &str, _uploads: usize) {}
    pub fn record_analysis(_wiki: &str, _seconds: f64) {}
    pub fn record_detection(_wiki: &str, _score: u8) {}
    pub fn record_download_failure() {}
    pub fn record_store_error(_operation: &'static str) {}
    pub fn record_web_request(_route: &'static str) {}
    pub fn set_stored(_stored: usize) {}
    pub fn set_wikis(_count: usize) {}
}

#[cfg(not(feature = "telemetry"))]
pub use disabled::*;
#[cfg(feature = "telemetry")]
pub use enabled::*;

#[cfg(test)]
mod tests {
    #[test]
    fn recording_without_a_dashboard_is_a_noop() {
        super::record_poll("commons.wikimedia.org", 3);
        super::record_analysis("commons.wikimedia.org", 0.5);
        super::record_detection("commons.wikimedia.org", 90);
        super::record_download_failure();
        super::record_store_error("reload");
        super::record_web_request("index");
        super::set_stored(1);
        super::set_wikis(3);
    }
}
