//! `/v1/earth/*` — JSON read straight from the files the ingest writes, plus a Server-Sent-Events
//! stream that emits `reading` when latest.json changes and `alert` / `attest` when those logs
//! grow. Same shape as sigil-api's `/v1/events`; sits behind fluxc-serve's `FLUX_PROXY` rule.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct Ev {
    pub kind: String,
    pub data: String,
}

pub struct App {
    pub dir: PathBuf,
    pub tx: broadcast::Sender<Ev>,
    pub started: f64,
}

const JSON_HEADERS: [(header::HeaderName, &str); 2] =
    [(header::CONTENT_TYPE, "application/json; charset=utf-8"), (header::CACHE_CONTROL, "no-store")];

fn json_bytes(status: StatusCode, body: Vec<u8>) -> Response {
    (status, JSON_HEADERS, body).into_response()
}

fn json_value(status: StatusCode, v: Value) -> Response {
    json_bytes(status, v.to_string().into_bytes())
}

fn not_found(what: &str) -> Response {
    json_value(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("{what} not available yet")}))
}

fn file(dir: &Path, name: &str) -> Response {
    match std::fs::read(dir.join(name)) {
        Ok(b) => json_bytes(StatusCode::OK, b),
        Err(_) => not_found(name),
    }
}

fn tail_jsonl(path: &Path, last: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let rows: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    rows.into_iter().rev().take(last).collect::<Vec<_>>().into_iter().rev().collect()
}

#[derive(Deserialize)]
struct Days {
    days: Option<f64>,
}
#[derive(Deserialize)]
struct Last {
    last: Option<usize>,
}

async fn index(State(st): State<Arc<App>>) -> Response {
    json_value(
        StatusCode::OK,
        json!({
            "ok": true, "service": crate::VERSION, "data_dir": st.dir.display().to_string(),
            "routes": ["/v1/earth/latest", "/v1/earth/series?days=433", "/v1/earth/forecast", "/v1/earth/excitation",
                       "/v1/earth/provenance", "/v1/earth/alerts?last=20", "/v1/earth/attest?last=20", "/v1/earth/health",
                       "/v1/earth/stream (text/event-stream: reading | alert | attest)"],
            "pages": ["https://sigilgraph.org/datacenter.html", "https://sigilgraph.org/kristensen-earth.html", "https://sigilgraph.org/kristensen-board.html"]
        }),
    )
}

async fn latest(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "latest.json")
}
async fn forecast(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "forecast.json")
}
async fn excitation(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "excitation.json")
}
async fn provenance(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "provenance.json")
}

async fn series(State(st): State<Arc<App>>, Query(q): Query<Days>) -> Response {
    let days = q.days.unwrap_or(433.0).clamp(1.0, 1200.0);
    let Ok(text) = std::fs::read_to_string(st.dir.join("series.json")) else { return not_found("series.json") };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return not_found("series.json") };
    let last = v["meta"]["last_observed_mjd"].as_f64().unwrap_or(0.0);
    let returned = match v["rows"].as_array_mut() {
        Some(rows) => {
            rows.retain(|r| r["mjd"].as_f64().map(|m| m >= last - days).unwrap_or(false));
            rows.len()
        }
        None => 0,
    };
    v["meta"]["days"] = json!(days);
    v["meta"]["returned"] = json!(returned);
    json_value(StatusCode::OK, v)
}

async fn alerts(State(st): State<Arc<App>>, Query(q): Query<Last>) -> Response {
    let rows = tail_jsonl(&st.dir.join("alerts.jsonl"), q.last.unwrap_or(20).clamp(1, 500));
    json_value(StatusCode::OK, json!({"ok": true, "count": rows.len(), "rows": rows}))
}

async fn attest(State(st): State<Arc<App>>, Query(q): Query<Last>) -> Response {
    let rows = tail_jsonl(&st.dir.join("attest.jsonl"), q.last.unwrap_or(20).clamp(1, 500));
    let v = crate::attest::verify(&st.dir.join("attest.jsonl"), Some(&st.dir.join("latest.json")));
    json_value(StatusCode::OK, json!({"ok": true, "count": rows.len(), "verification": v, "rows": rows}))
}

fn mtime(p: &Path) -> Option<f64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

async fn health(State(st): State<Arc<App>>) -> Response {
    let f = |n: &str| json!({"mtime": mtime(&st.dir.join(n)).map(crate::time::iso_utc), "bytes": std::fs::metadata(st.dir.join(n)).map(|m| m.len()).ok()});
    json_value(
        StatusCode::OK,
        json!({
            "ok": true, "service": crate::VERSION, "uptime_s": (crate::time::now_unix() - st.started).round(),
            "subscribers": st.tx.receiver_count(),
            "files": {"latest.json": f("latest.json"), "series.json": f("series.json"), "forecast.json": f("forecast.json"),
                      "excitation.json": f("excitation.json"), "alerts.jsonl": f("alerts.jsonl"), "attest.jsonl": f("attest.jsonl")}
        }),
    )
}

async fn stream(State(st): State<Arc<App>>) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let first = std::fs::read_to_string(st.dir.join("latest.json")).unwrap_or_else(|_| "{}".into());
    let rx = st.tx.subscribe();
    let head = futures_util::stream::once(async move { Ok(Event::default().event("reading").data(first)) });
    let live = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(ev) => Some(Ok(Event::default().event(ev.kind).data(ev.data))),
            Err(_) => None,
        }
    });
    Sse::new(head.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
}

async fn fallback() -> Response {
    json_value(StatusCode::NOT_FOUND, json!({"ok": false, "error": "no such earth route", "index": "/v1/earth"}))
}

/// Watch the data files and publish changes to every subscriber.
async fn watcher(app: Arc<App>) {
    let mut iv = tokio::time::interval(Duration::from_secs(2));
    let stat = |n: &str| -> (Option<f64>, u64) {
        let p = app.dir.join(n);
        (mtime(&p), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0))
    };
    let mut last = stat("latest.json");
    let mut alerts = stat("alerts.jsonl").1;
    let mut attest = stat("attest.jsonl").1;
    loop {
        iv.tick().await;
        let now = stat("latest.json");
        if now != last {
            last = now;
            if let Ok(text) = std::fs::read_to_string(app.dir.join("latest.json")) {
                let _ = app.tx.send(Ev { kind: "reading".into(), data: text });
            }
        }
        let a = stat("alerts.jsonl").1;
        if a > alerts {
            for row in tail_jsonl(&app.dir.join("alerts.jsonl"), 1) {
                let _ = app.tx.send(Ev { kind: "alert".into(), data: row.to_string() });
            }
        }
        alerts = a;
        let t = stat("attest.jsonl").1;
        if t > attest {
            for row in tail_jsonl(&app.dir.join("attest.jsonl"), 1) {
                let _ = app.tx.send(Ev { kind: "attest".into(), data: row.to_string() });
            }
        }
        attest = t;
    }
}

pub async fn run(bind: &str, dir: PathBuf) -> anyhow::Result<()> {
    let (tx, _) = broadcast::channel::<Ev>(256);
    let app = Arc::new(App { dir, tx, started: crate::time::now_unix() });
    tokio::spawn(watcher(app.clone()));
    let router = Router::new()
        .route("/v1/earth", get(index))
        .route("/v1/earth/", get(index))
        .route("/v1/earth/latest", get(latest))
        .route("/v1/earth/series", get(series))
        .route("/v1/earth/forecast", get(forecast))
        .route("/v1/earth/excitation", get(excitation))
        .route("/v1/earth/provenance", get(provenance))
        .route("/v1/earth/alerts", get(alerts))
        .route("/v1/earth/attest", get(attest))
        .route("/v1/earth/health", get(health))
        .route("/v1/earth/stream", get(stream))
        .fallback(fallback)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("sigil-earth serve: listening on {bind}");
    axum::serve(listener, router.into_make_service()).await?;
    Ok(())
}
