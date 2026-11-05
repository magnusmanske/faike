//! Web UI and JSON/AbuseFilter endpoints.

use crate::abuse_filter;
use crate::config::Config;
use crate::model::Detection;
use crate::status::Status;
use crate::store::{Filter, Store};
use crate::telemetry;
use crate::user_pages::{Exists, UserPages, safe_domain};
use crate::wiki::{DEFAULT_DOMAIN, Wiki};
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;
const MAX_QUERY_CHARS: usize = 200;
/// Query value for a view across all wikis.
const ALL_WIKIS: &str = "all";

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    store: Arc<Store>,
    status: Arc<Status>,
    user_pages: Arc<UserPages>,
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    wiki: Option<String>,
    min_score: Option<u16>,
    q: Option<String>,
    limit: Option<usize>,
}

impl Params {
    /// The filter for the selected wiki. `?wiki=all` selects every wiki;
    /// an absent or unknown value falls back to Commons.
    fn filter(&self, wikis: &[Wiki]) -> Filter {
        let text = self
            .q
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(MAX_QUERY_CHARS).collect::<String>());
        Filter {
            wiki: selected_wiki(self, wikis),
            min_score: self.min_score.unwrap_or(0).min(100) as u8,
            text,
            limit: self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        }
    }
}

/// The wiki a request selects, or `None` for all wikis. Unknown domains fall
/// back to Commons (or the first configured wiki if Commons was removed).
fn selected_wiki(params: &Params, wikis: &[Wiki]) -> Option<String> {
    match params.wiki.as_deref() {
        Some(ALL_WIKIS) => None,
        Some(domain) if wikis.iter().any(|wiki| wiki.domain == domain) => Some(domain.to_string()),
        _ => wikis
            .iter()
            .find(|wiki| wiki.domain == DEFAULT_DOMAIN)
            .or_else(|| wikis.first())
            .map(|wiki| wiki.domain.clone()),
    }
}

#[derive(Debug, Serialize)]
struct DetectionList {
    /// Selected wiki, or `None` for all wikis.
    wiki: Option<String>,
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
        user_pages: Arc::new(UserPages::new(&config)),
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
                        for wiki in store.wikis() {
                            status.set_cursor(&wiki.domain, wiki.cursor.as_ref());
                        }
                        status.clear_error();
                        telemetry::set_stored(store.count());
                        telemetry::set_wikis(store.wikis().len());
                    }
                    Err(error) => {
                        telemetry::record_store_error("reload");
                        status.set_error(format!("Store reload failed: {error:#}"));
                        eprintln!("faike: store reload failed: {error:#}");
                    }
                }
            }
        });
    }
    let address = state.config.listen_addr();
    let listener = tokio::net::TcpListener::bind(&address).await?;
    eprintln!("faike: listening on http://{address}");
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
        .route("/api/wikis", get(api_wikis))
        .route("/abuse-filters", get(abuse_filters_text))
        .route("/abuse-filters.json", get(abuse_filters_json))
        .route("/healthz", get(health))
        .with_state(state)
}

async fn index(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    telemetry::record_web_request("index");
    let wikis = state.store.wikis();
    let filter = params.filter(&wikis);
    let mut detections = state.store.recent(&filter);
    detections.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.score.cmp(&a.score)));
    let user_pages = state.user_pages.resolve(&detections, &wikis).await;
    let status = state.status.snapshot();
    let refresh = state.config.refresh_sec > 0
        && params.q.is_none()
        && params.min_score.is_none()
        && params.limit.is_none();

    let mut html = String::with_capacity(16 * 1024);
    html.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    html.push_str("<title>fAIke — AI upload watchdog</title>");
    if refresh {
        let _ = write!(
            html,
            "<meta http-equiv=\"refresh\" content=\"{}\">",
            state.config.refresh_sec
        );
    }
    html.push_str(STYLE);
    html.push_str("</head><body><header><h1>fAIke</h1>");
    html.push_str("<p>AI upload watchdog · detection is evidence, not proof.</p></header>");

    html.push_str("<nav class=\"wikis\">");
    let _ = write!(
        html,
        "<a class=\"{}\" href=\"/?wiki={}\">All wikis</a>",
        if filter.wiki.is_none() { "active" } else { "" },
        ALL_WIKIS
    );
    for wiki in &wikis {
        let active = filter.wiki.as_deref() == Some(wiki.domain.as_str());
        let _ = write!(
            html,
            "<a class=\"{} {}\" href=\"/?wiki={}\" title=\"{}\">{}</a>",
            if active { "active" } else { "" },
            if wiki.enabled { "" } else { "disabled" },
            query_encode(&wiki.domain),
            if wiki.enabled { "watching" } else { "disabled" },
            escape_html(&wiki.display_name)
        );
    }
    html.push_str("</nav>");

    let selected_status = filter
        .wiki
        .as_ref()
        .and_then(|domain| status.wikis.iter().find(|entry| &entry.wiki == domain));
    let selected_name = filter
        .wiki
        .as_ref()
        .and_then(|domain| wikis.iter().find(|wiki| &wiki.domain == domain))
        .map(|wiki| wiki.display_name.as_str())
        .unwrap_or("All wikis");

    html.push_str("<section class=\"status\">");
    let _ = write!(
        html,
        "<span>{} cursor <code>{}</code></span>",
        escape_html(selected_name),
        escape_html(
            selected_status
                .and_then(|entry| entry.cursor.as_ref())
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
    if let Some(error) = selected_status.and_then(|entry| entry.last_error.as_ref()) {
        let _ = write!(
            html,
            "<p class=\"error\">{}: {}</p>",
            escape_html(selected_name),
            escape_html(error)
        );
    }

    let wiki_param = filter.wiki.as_deref().unwrap_or(ALL_WIKIS);
    let _ = write!(
        html,
        "<form method=\"get\" action=\"/\" class=\"filters\">\
         <input type=\"hidden\" name=\"wiki\" value=\"{}\">\
         <label>Min score <input type=\"number\" name=\"min_score\" min=\"0\" max=\"100\" value=\"{}\"></label>\
         <label>Search <input type=\"search\" name=\"q\" maxlength=\"{}\" value=\"{}\" placeholder=\"title, user, tool, signal\"></label>\
         <label>Limit <input type=\"number\" name=\"limit\" min=\"1\" max=\"{}\" value=\"{}\"></label>\
         <button type=\"submit\">Filter</button>\
         <a href=\"/abuse-filters?wiki={}\">AbuseFilter export</a>\
         <a href=\"/api/detections?wiki={}\">JSON</a>\
         </form>",
        query_encode(wiki_param),
        filter.min_score,
        MAX_QUERY_CHARS,
        escape_html(params.q.as_deref().unwrap_or("")),
        MAX_LIMIT,
        filter.limit,
        query_encode(wiki_param),
        query_encode(wiki_param)
    );

    if detections.is_empty() {
        html.push_str("<p class=\"empty\">No detections match.</p>");
    } else {
        html.push_str(
            "<div class=\"table-wrap\"><table><thead><tr>\
            <th>Score</th><th>Wiki</th><th>File</th><th>Uploader</th><th>Time</th><th>Signals</th>\
            </tr></thead><tbody>",
        );
        for detection in &detections {
            html.push_str(&row(detection, &user_pages));
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</body></html>");
    html_response(html)
}

fn row(detection: &Detection, user_pages: &HashMap<(String, String), Exists>) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<tr><td><span class=\"score {}\">{}</span></td><td>",
        detection.level.as_str(),
        detection.score
    );
    if detection.wiki.is_empty() {
        out.push('—');
    } else {
        let _ = write!(
            out,
            "<a class=\"wiki\" href=\"/?wiki={}\">{}</a>",
            query_encode(&detection.wiki),
            escape_html(&detection.wiki)
        );
    }
    out.push_str("</td><td>");
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
        user_links(
            detection,
            user_pages
                .get(&(detection.wiki.clone(), detection.user.trim().to_string()))
                .copied()
        ),
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

/// A MediaWiki-style uploader link: `User:Foo (talk)`. A missing page gets the
/// `redlink` class, matching the wiki's own link colour. If existence is
/// unknown (lookup disabled or failed) the link stays neutral.
fn user_links(detection: &Detection, exists: Option<Exists>) -> String {
    let user = detection.user.trim();
    if user.is_empty() {
        return String::new();
    }
    if !safe_domain(&detection.wiki) {
        return escape_html(user);
    }
    let base = format!("https://{}/wiki/", detection.wiki);
    let encoded = query_encode(user);
    let exists = exists.unwrap_or_default();
    let user_class = if exists.user == Some(false) {
        " class=\"redlink\""
    } else {
        ""
    };
    let talk_class = if exists.talk == Some(false) {
        " class=\"redlink\""
    } else {
        ""
    };
    format!(
        "<a{user_class} href=\"{base}User:{encoded}\" target=\"_blank\" rel=\"noreferrer noopener\">{name}</a> \
         (<a{talk_class} href=\"{base}User_talk:{encoded}\" target=\"_blank\" rel=\"noreferrer noopener\">talk</a>)",
        name = escape_html(user)
    )
}

async fn api_detections(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    telemetry::record_web_request("api_detections");
    let wikis = state.store.wikis();
    let filter = params.filter(&wikis);
    let mut detections = state.store.recent(&filter);
    detections.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.score.cmp(&a.score)));
    json_response(&DetectionList {
        wiki: filter.wiki,
        count: detections.len(),
        detections,
    })
}

async fn api_wikis(State(state): State<AppState>) -> Response {
    telemetry::record_web_request("api_wikis");
    json_response(&state.store.wikis())
}

async fn api_status(State(state): State<AppState>) -> Response {
    telemetry::record_web_request("api_status");
    json_response(&state.status.snapshot())
}

async fn abuse_filters_text(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Response {
    telemetry::record_web_request("abuse_filters");
    let rules = current_rules(&state, &params);
    text_response(abuse_filter::text(&rules), "text/plain; charset=utf-8")
}

async fn abuse_filters_json(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Response {
    telemetry::record_web_request("abuse_filters");
    json_response(&current_rules(&state, &params))
}

fn current_rules(state: &AppState, params: &Params) -> Vec<abuse_filter::Rule> {
    let wikis = state.store.wikis();
    let detections = state.store.recent(&Filter {
        wiki: selected_wiki(params, &wikis),
        min_score: 0,
        text: None,
        limit: usize::MAX,
    });
    abuse_filter::rules(&detections, state.config.abuse_filter_min_score)
}

async fn health() -> Response {
    telemetry::record_web_request("health");
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

/// Percent-encode a query value. Wiki domains are ASCII, but the value comes
/// from the database, so do not rely on that.
fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
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
.redlink { color:var(--high); }
.status { display:flex; flex-wrap:wrap; gap:.5rem 1.2rem; padding:.6rem .8rem; border:1px solid var(--line); border-radius:.4rem; color:var(--muted); font-size:.88rem; }
.status code { color:var(--fg); }
.wikis { display:flex; flex-wrap:wrap; gap:.4rem; margin:.8rem 0 0; }
.wikis a { padding:.25rem .65rem; border:1px solid var(--line); border-radius:1rem; text-decoration:none; font-size:.85rem; }
.wikis a.active { background:var(--accent); border-color:var(--accent); color:var(--bg); }
.wikis a.disabled { opacity:.55; text-decoration:line-through; }
.wiki { font-size:.85rem; white-space:nowrap; }
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
                wikis_file: dir.path().join("wikis.json"),
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
            "commons.wikimedia.org".into(),
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
        let german = Detection::new(
            "def".into(),
            "de.wikipedia.org".into(),
            "File:German.png".into(),
            "2026-01-02T00:00:00Z".into(),
            "Beispiel".into(),
            "mit Midjourney".into(),
            String::new(),
            "https://de.wikipedia.org/wiki/Datei:German.png".into(),
            "image/png".into(),
            2048,
            "def".into(),
            vec![
                Signal::new(
                    Source::PngText,
                    Confidence::High,
                    "png.parameters.prompt",
                    "parameters",
                )
                .with_tool("Stable Diffusion"),
            ],
        );
        store.add(german).await.unwrap();
        AppState {
            config,
            store,
            status: Arc::new(Status::new()),
            user_pages: Arc::new(UserPages::disabled()),
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
    async fn index_links_uploaders_to_their_wiki() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let html = body(
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(html.contains("https://commons.wikimedia.org/wiki/User:Example"));
        assert!(html.contains("https://commons.wikimedia.org/wiki/User_talk:Example"));
        assert!(html.contains(">talk</a>"));
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
        assert!(!json.contains("German.png"), "Commons is the default view");
    }

    #[tokio::test]
    async fn wiki_views_are_filtered_and_linkable() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);

        let german = body(
            app.clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/detections?wiki=de.wikipedia.org")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(german.contains("German.png"));
        assert!(!german.contains("alert(1)"));

        let all = body(
            app.clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/detections?wiki=all")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(all.contains("German.png"));
        assert!(all.contains("alert(1)"));

        let html = body(
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/?wiki=de.wikipedia.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(html.contains("fAIke"));
        assert!(html.contains("German.png"));
        assert!(html.contains("href=\"/?wiki=de.wikipedia.org\""));
        assert!(html.contains("English Wikipedia"));
    }

    #[tokio::test]
    async fn api_lists_wikis() {
        let dir = TempDir::new().unwrap();
        let app = router(state(&dir).await);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/wikis")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body(response).await;
        assert!(json.contains("commons.wikimedia.org"));
        assert!(json.contains("en.wikipedia.org"));
        assert!(json.contains("de.wikipedia.org"));
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

    fn sample_detection(wiki: &str, user: &str) -> Detection {
        Detection::new(
            "id".into(),
            wiki.into(),
            "File:X.png".into(),
            "2026-01-01T00:00:00Z".into(),
            user.into(),
            String::new(),
            String::new(),
            String::new(),
            "image/png".into(),
            0,
            "id".into(),
            Vec::new(),
        )
    }

    #[test]
    fn user_links_target_the_wiki_and_mark_missing_pages() {
        let sample = sample_detection("commons.wikimedia.org", "Example User");
        let html = user_links(
            &sample,
            Some(Exists {
                user: Some(false),
                talk: Some(true),
            }),
        );
        assert!(html.contains("href=\"https://commons.wikimedia.org/wiki/User:Example%20User\""));
        assert!(
            html.contains("href=\"https://commons.wikimedia.org/wiki/User_talk:Example%20User\"")
        );
        assert!(html.contains(">Example User</a>"));
        assert!(html.contains(">talk</a>"));
        assert_eq!(
            html.matches("redlink").count(),
            1,
            "only the user page is missing"
        );
    }

    #[test]
    fn user_links_escape_names_and_reject_bad_domains() {
        let sample = sample_detection("commons.wikimedia.org", "<b>Mallory</b>");
        let html = user_links(&sample, None);
        assert!(html.contains("&lt;b&gt;Mallory&lt;/b&gt;"));
        assert!(!html.contains("<b>"));

        let sample = sample_detection("evil.example/\"", "Mallory");
        let html = user_links(&sample, None);
        assert!(!html.contains("href=\""));
        assert_eq!(html, "Mallory");
    }
}
