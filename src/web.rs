//! Web UI and JSON/AbuseFilter endpoints.

use crate::abuse_filter;
use crate::config::Config;
use crate::model::Detection;
use crate::status::Status;
use crate::store::{Filter, Store};
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;
const MAX_QUERY_CHARS: usize = 200;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    store: Arc<Store>,
    status: Arc<Status>,
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    min_score: Option<u16>,
    q: Option<String>,
    limit: Option<usize>,
}

impl Params {
    fn filter(&self) -> Filter {
        let text = self
            .q
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(MAX_QUERY_CHARS).collect::<String>());
        Filter {
            min_score: self.min_score.unwrap_or(0).min(100) as u8,
            text,
            limit: self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        }
    }
}

#[derive(Debug, Serialize)]
struct DetectionList {
    count: usize,
    detections: Vec<Detection>,
}

pub async fn run(
    config: Arc<Config>,
    store: Arc<Store>,
    status: Arc<Status>,
    live_reload: bool,
) -> Result<()> {
    status.set_stored(store.count());
    let state = AppState {
        config,
        store,
        status,
    };
    if live_reload {
        let store = state.store.clone();
        let status = state.status.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                match store.reload().await {
                    Ok(()) => {
                        status.set_stored(store.count());
                        if let Some(cursor) = store.load_cursor().await {
                            status.set_cursor(Some(&cursor));
                        }
                        status.clear_error();
                    }
                    Err(error) => {
                        status.set_error(format!("Store reload failed: {error:#}"));
                        eprintln!("aimer: store reload failed: {error:#}");
                    }
                }
            }
        });
    }
    let address = state.config.listen_addr();
    let listener = tokio::net::TcpListener::bind(&address).await?;
    eprintln!("aimer: listening on http://{address}");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/detections", get(api_detections))
        .route("/api/status", get(api_status))
        .route("/abuse-filters", get(abuse_filters_text))
        .route("/abuse-filters.json", get(abuse_filters_json))
        .route("/healthz", get(health))
        .with_state(state)
}

async fn index(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    let filter = params.filter();
    let mut detections = state.store.recent(&filter);
    detections.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.score.cmp(&a.score)));
    let status = state.status.snapshot();
    let refresh = state.config.refresh_sec > 0
        && params.q.is_none()
        && params.min_score.is_none()
        && params.limit.is_none();

    let mut html = String::with_capacity(16 * 1024);
    html.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    html.push_str("<title>aimer — AI upload watchdog</title>");
    if refresh {
        let _ = write!(
            html,
            "<meta http-equiv=\"refresh\" content=\"{}\">",
            state.config.refresh_sec
        );
    }
    html.push_str(STYLE);
    html.push_str("</head><body><header><h1>aimer</h1>");
    html.push_str("<p>AI upload watchdog · detection is evidence, not proof.</p></header>");

    html.push_str("<section class=\"status\">");
    let _ = write!(
        html,
        "<span>Cursor <code>{}</code></span>",
        escape_html(
            status
                .cursor
                .as_ref()
                .map(|cursor| cursor.timestamp.as_str())
                .unwrap_or("—")
        )
    );
    let _ = write!(
        html,
        "<span>{} polls</span><span>{} uploads</span><span>{} analysed</span>\
         <span>{} detections</span><span>{} in memory</span><span>up {}</span>",
        status.polls,
        status.uploads_seen,
        status.files_analyzed,
        status.detections,
        status.stored,
        duration(status.uptime_seconds)
    );
    html.push_str("</section>");
    if let Some(error) = &status.last_error {
        let _ = write!(
            html,
            "<p class=\"error\">Last error: {}</p>",
            escape_html(error)
        );
    }

    let _ = write!(
        html,
        "<form method=\"get\" action=\"/\" class=\"filters\">\
         <label>Min score <input type=\"number\" name=\"min_score\" min=\"0\" max=\"100\" value=\"{}\"></label>\
         <label>Search <input type=\"search\" name=\"q\" maxlength=\"{}\" value=\"{}\" placeholder=\"title, user, tool, signal\"></label>\
         <label>Limit <input type=\"number\" name=\"limit\" min=\"1\" max=\"{}\" value=\"{}\"></label>\
         <button type=\"submit\">Filter</button>\
         <a href=\"/abuse-filters\">AbuseFilter export</a>\
         <a href=\"/api/detections\">JSON</a>\
         </form>",
        filter.min_score,
        MAX_QUERY_CHARS,
        escape_html(params.q.as_deref().unwrap_or("")),
        MAX_LIMIT,
        filter.limit
    );

    if detections.is_empty() {
        html.push_str("<p class=\"empty\">No detections match.</p>");
    } else {
        html.push_str(
            "<div class=\"table-wrap\"><table><thead><tr>\
            <th>Score</th><th>File</th><th>Uploader</th><th>Time</th><th>Signals</th>\
            </tr></thead><tbody>",
        );
        for detection in &detections {
            html.push_str(&row(detection));
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</body></html>");
    html_response(html)
}

fn row(detection: &Detection) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<tr><td><span class=\"score {}\">{}</span></td><td>",
        detection.level.as_str(),
        detection.score
    );
    match safe_url(&detection.description_url) {
        Some(url) => {
            let _ = write!(
                out,
                "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer noopener\">{}</a>",
                escape_html(url),
                escape_html(&detection.title)
            );
        }
        None => {
            let _ = write!(out, "{}", escape_html(&detection.title));
        }
    }
    if !detection.mime.is_empty() || detection.size > 0 {
        let _ = write!(
            out,
            "<div class=\"meta\">{} · {}</div>",
            escape_html(&detection.mime),
            size(detection.size)
        );
    }
    let comment = text_preview(&detection.comment, 180);
    if !comment.is_empty() {
        let title = text_preview(&detection.comment, 300);
        let _ = write!(
            out,
            "<div class=\"comment\" title=\"{}\">{}</div>",
            escape_html(&title),
            escape_html(&comment)
        );
    }
    let _ = write!(
        out,
        "</td><td>{}</td><td class=\"time\">{}</td><td class=\"signals\">",
        escape_html(&detection.user),
        escape_html(&detection.timestamp)
    );
    for signal in &detection.signals {
        let label = signal
            .tool
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| signal.code.clone());
        let _ = write!(
            out,
            "<span class=\"sig {}\" title=\"{}: {}\">{} · {}</span>",
            signal.confidence.as_str(),
            escape_html(signal.source.as_str()),
            escape_html(&signal.detail),
            escape_html(signal.source.as_str()),
            escape_html(&label)
        );
    }
    out.push_str("</td></tr>");
    out
}

async fn api_detections(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    let filter = params.filter();
    let mut detections = state.store.recent(&filter);
    detections.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.score.cmp(&a.score)));
    json_response(&DetectionList {
        count: detections.len(),
        detections,
    })
}

async fn api_status(State(state): State<AppState>) -> Response {
    json_response(&state.status.snapshot())
}

async fn abuse_filters_text(State(state): State<AppState>) -> Response {
    let rules = current_rules(&state);
    text_response(abuse_filter::text(&rules), "text/plain; charset=utf-8")
}

async fn abuse_filters_json(State(state): State<AppState>) -> Response {
    json_response(&current_rules(&state))
}

fn current_rules(state: &AppState) -> Vec<abuse_filter::Rule> {
    let detections = state.store.recent(&Filter {
        min_score: 0,
        text: None,
        limit: usize::MAX,
    });
    abuse_filter::rules(&detections, state.config.abuse_filter_min_score)
}

async fn health() -> Response {
    text_response("ok\n".to_string(), "text/plain; charset=utf-8")
}

fn html_response(body: String) -> Response {
    let mut headers = security_headers();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    (headers, body).into_response()
}

fn text_response(body: String, content_type: &'static str) -> Response {
    let mut headers = security_headers();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    (headers, body).into_response()
}

fn json_response<T: Serialize>(value: &T) -> Response {
    let mut headers = security_headers();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    match serde_json::to_vec(value) {
        Ok(body) => (headers, Body::from(body)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JSON serialization failed: {error}"),
        )
            .into_response(),
    }
}

fn security_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers
}

fn safe_url(url: &str) -> Option<&str> {
    let url = url.trim();
    (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn text_preview(value: &str, max: usize) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::patterns::truncate(&collapsed, max)
}

fn size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

const STYLE: &str = r#"<style>
:root { color-scheme: light dark; --fg:#1b1b1b; --muted:#666; --bg:#fff; --line:#ddd; --accent:#0b5cad; --high:#b3261e; --medium:#9a6700; --low:#5f6368; }
@media (prefers-color-scheme: dark) { :root { --fg:#e8e8e8; --muted:#a0a0a0; --bg:#121212; --line:#333; --accent:#8ab4f8; --high:#f2b8b5; --medium:#fdd663; --low:#bdc1c6; } }
* { box-sizing: border-box; }
body { margin:0 auto; padding:1.25rem; max-width:1400px; background:var(--bg); color:var(--fg); font:15px/1.45 system-ui,-apple-system,Segoe UI,sans-serif; }
header h1 { margin:0; font-size:1.6rem; letter-spacing:-.02em; }
header p { margin:.2rem 0 1rem; color:var(--muted); }
a { color:var(--accent); }
.status { display:flex; flex-wrap:wrap; gap:.5rem 1.2rem; padding:.6rem .8rem; border:1px solid var(--line); border-radius:.4rem; color:var(--muted); font-size:.88rem; }
.status code { color:var(--fg); }
.error { padding:.6rem .8rem; border-left:3px solid var(--high); background:color-mix(in srgb, var(--high) 10%, transparent); }
.filters { display:flex; flex-wrap:wrap; gap:.6rem 1rem; align-items:end; margin:1rem 0; }
.filters label { display:flex; flex-direction:column; gap:.2rem; font-size:.85rem; color:var(--muted); }
.filters input { padding:.3rem .4rem; border:1px solid var(--line); border-radius:.3rem; background:var(--bg); color:var(--fg); }
.filters input[type=number] { width:7rem; }
.filters input[type=search] { width:18rem; }
.filters button { padding:.35rem .8rem; border:1px solid var(--line); border-radius:.3rem; background:var(--bg); color:var(--fg); cursor:pointer; }
.filters a { font-size:.9rem; }
.table-wrap { overflow-x:auto; border:1px solid var(--line); border-radius:.4rem; }
table { border-collapse:collapse; width:100%; min-width:900px; }
th, td { text-align:left; vertical-align:top; padding:.55rem .6rem; border-bottom:1px solid var(--line); }
th { font-size:.8rem; text-transform:uppercase; letter-spacing:.04em; color:var(--muted); font-weight:600; }
tr:last-child td { border-bottom:none; }
.score { display:inline-block; min-width:2.6rem; text-align:center; padding:.15rem .4rem; border-radius:1rem; font-weight:700; font-variant-numeric:tabular-nums; border:1px solid currentColor; }
.score.high { color:var(--high); } .score.medium { color:var(--medium); } .score.low { color:var(--low); }
.meta, .comment { color:var(--muted); font-size:.82rem; margin-top:.2rem; }
.comment { max-width:34rem; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.time { white-space:nowrap; color:var(--muted); font-size:.85rem; }
.signals { max-width:30rem; }
.sig { display:inline-block; margin:.1rem .25rem .1rem 0; padding:.1rem .4rem; border:1px solid var(--line); border-radius:.25rem; font-size:.8rem; }
.sig.high { border-color:var(--high); } .sig.medium { border-color:var(--medium); }
.empty { padding:2rem 0; color:var(--muted); }
@media (max-width:700px) { body { padding:.7rem; } .filters input[type=search] { width:100%; } }
</style>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, Signal, Source};
    use axum::body::to_bytes;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn config(dir: &TempDir) -> Config {
        Config {
            store: crate::config::StoreConfig {
                file: dir.path().join("detections.jsonl"),
                cursor_file: dir.path().join("cursor.json"),
                ..crate::config::StoreConfig::default()
            },
            ..Config::default()
        }
    }

    async fn state(dir: &TempDir) -> AppState {
        let config = Arc::new(config(dir));
        let store = Arc::new(Store::open(&config).await.unwrap());
        let detection = Detection::new(
            "abc".into(),
            "File:<script>alert(1)</script>.png".into(),
            "2026-01-01T00:00:00Z".into(),
            "Example".into(),
            "made with Midjourney".into(),
            String::new(),
            "https://commons.wikimedia.org/wiki/File:Example.png".into(),
            "image/png".into(),
            1024,
            "abc".into(),
            vec![
                Signal::new(
                    Source::C2pa,
                    Confidence::High,
                    "c2pa.digital_source_type",
                    "trained",
                )
                .with_tool("Midjourney"),
            ],
        );
        store.add(detection).await.unwrap();
        AppState {
            config,
            store,
            status: Arc::new(Status::new()),
        }
    }

    async fn body(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn index_escapes_detection_fields() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(response).await;
        assert!(html.contains("File:&lt;script&gt;alert(1)&lt;/script&gt;.png"));
        assert!(!html.contains("<script>alert(1)</script>"));
    }

    #[tokio::test]
    async fn api_lists_detections() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/detections?min_score=50")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body(response).await;
        assert!(json.contains("\"count\":1"));
        assert!(json.contains("Midjourney"));
    }

    #[tokio::test]
    async fn abuse_filter_export_contains_rule() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/abuse-filters")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = body(response).await;
        assert!(text.contains("added_lines irlike"));
        assert!(text.contains("equals_to_any"));
    }

    #[tokio::test]
    async fn health_is_ok() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "ok\n");
    }
}
