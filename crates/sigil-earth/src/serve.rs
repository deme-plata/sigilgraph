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
            "routes": ["/v1/earth/latest (incl. .bio = K_bio, breathing, Lloyd ladder)", "/v1/earth/series?days=433", "/v1/earth/forecast", "/v1/earth/excitation", "/v1/earth/bio (3-year Mauna Loa CO₂ series + fit)",
                       "/v1/earth/provenance", "/v1/earth/tips", "/v1/earth/alerts?last=20", "/v1/earth/attest?last=20", "/v1/earth/health",
                       "/v1/earth/stream (text/event-stream: reading | alert | attest)", "/v1/earth/badge.svg (live SVG badge: K⊕ · regime · date)", "/v1/earth/search?q=… (the aether search: publications, readings by date/MJD/regime, attest rows, alerts)"],
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
/// The biosphere channel's 3-year daily series (Mauna Loa CO₂, fit, seasonal, residual, z).
/// The headline reading lives in `latest.bio`.
async fn bio(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "bio.json")
}
async fn provenance(State(st): State<Arc<App>>) -> Response {
    file(&st.dir, "provenance.json")
}
/// The fortolkning alone: one tip per metric (what · read · not · family · da), from the current feed.
async fn tips(State(st): State<Arc<App>>) -> Response {
    let Ok(text) = std::fs::read_to_string(st.dir.join("latest.json")) else { return not_found("latest.json") };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    json_value(StatusCode::OK, json!({"ok": true, "version": crate::VERSION, "count": v["tips"].as_object().map(|o| o.len()).unwrap_or(0), "tips": v["tips"]}))
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

#[derive(Deserialize)]
struct SearchQ {
    q: Option<String>,
    limit: Option<usize>,
}

/// `/v1/earth/search?q=…` — the aether search bar behind the 3D Earth (2026-09-17). Four sources,
/// all files this service already serves or that `sigil-earth publish` writes, none invented:
///   publication — flux-search over the publish index (`DEFAULT_SEARCH_INDEX`), one doc per
///                 attested reading pushed to the nodes' aether store; `node_copy` is the node's
///                 own copy of that reading (`/v1/aether/cat?name=…`, same origin on sigilgraph.org).
///   reading     — the EOP series by date (2026-09-14, or a 2026-09 prefix), MJD (61297), or
///                 regime word (stable / elevated / critical; `forecast` for predicted rows).
///   attest      — attest.jsonl rows by row number, date, tx or digest prefix.
///   alert       — alerts.jsonl rows containing the query.
/// Nothing here is ranked across kinds; the page groups them. Empty query → the hint only.
async fn search(State(st): State<Arc<App>>, Query(q): Query<SearchQ>) -> Response {
    let t0 = std::time::Instant::now();
    let query = q.q.unwrap_or_default().trim().to_string();
    let ql = query.to_lowercase();
    let limit = q.limit.unwrap_or(12).clamp(1, 50);
    let hint = "a date (2026-09-14 or 2026-09), an MJD (61297), a regime (stable / elevated / critical), 'forecast', 'attest', 'alert', a tx or digest prefix, or words from the fortolkning";
    if query.is_empty() {
        return json_value(StatusCode::OK, json!({"ok": true, "data": {"query": "", "hits": [], "hint": hint}}));
    }
    let mut hits: Vec<Value> = vec![];
    // 1. publications — the flux-search index sigil-earth publish maintains
    let idx = std::env::var("SIGIL_EARTH_SEARCH_INDEX").unwrap_or_else(|_| crate::publish::DEFAULT_SEARCH_INDEX.into());
    let mut index_docs = 0usize;
    if let Ok(mut eng) = flux_search::SearchEngine::load_from_path(&idx) {
        index_docs = eng.doc_count();
        let resp = eng.search(flux_search::SearchQuery { q: query.clone(), page: 1, per_page: limit, ..Default::default() });
        for r in resp.results {
            let blake3 = r.url.rsplit('/').next().unwrap_or("").to_string();
            let date = r.title.split_whitespace().find(|w| w.len() == 10 && w.as_bytes()[4] == b'-').unwrap_or("").to_string();
            hits.push(json!({"kind": "publication", "title": r.title, "snippet": r.snippet, "url": r.url, "blake3": blake3, "date": date,
                "score": r.score, "attest": "/v1/earth/attest?last=500", "bundle": format!("flux_aether_retrieve {{content_root: \"{blake3}\"}}")}));
        }
    }
    // 2. readings — the series by date / MJD / regime
    let is_date = ql.len() >= 7 && ql.as_bytes()[4] == b'-' && ql[..4].chars().all(|c| c.is_ascii_digit());
    let mjd_q: Option<f64> = if ql.len() == 5 && ql.chars().all(|c| c.is_ascii_digit()) { ql.parse().ok() } else { None };
    let regime_q = ["stable", "elevated", "critical"].iter().find(|r| ql.contains(*r)).copied();
    let want_forecast = ql.contains("forecast") || ql.contains("predict");
    if is_date || mjd_q.is_some() || regime_q.is_some() || want_forecast {
        if let Ok(text) = std::fs::read_to_string(st.dir.join("series.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                let mut rows: Vec<&Value> = v["rows"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
                if !(want_forecast && !is_date && mjd_q.is_none() && regime_q.is_none()) { rows.reverse(); } // most recent first; the forecast reads forward from today
                let mut n = 0usize;
                for r in rows {
                    let date = r["date"].as_str().unwrap_or("");
                    let mjd = r["mjd"].as_f64().unwrap_or(f64::NAN);
                    let predicted = r["flag"].as_str() == Some("P");
                    let k = r["k_resid"].as_f64();
                    let regime = k.map(crate::regime);
                    let m = (is_date && date.starts_with(&ql))
                        || mjd_q.map(|m| (m - mjd).abs() < 0.5).unwrap_or(false)
                        || (regime_q.is_some() && !predicted && regime == regime_q)
                        || (want_forecast && predicted && !is_date);
                    if !m { continue; }
                    hits.push(json!({"kind": "reading", "date": date, "mjd": mjd, "predicted": predicted, "k_resid": k, "k_raw": r["k_raw"], "regime": regime,
                        "lod_ms": r["lod"], "xp": r["xp"], "yp": r["yp"], "ut1utc": r["ut1utc"],
                        "title": format!("{date} · MJD {mjd:.0}{}{}", k.map(|k| format!(" · K⊕ {k:.2}σ {}", crate::regime(k))).unwrap_or_default(), if predicted { " · IERS prediction" } else { "" })}));
                    n += 1;
                    if n >= limit { break; }
                }
            }
        }
    }
    // 3. attest rows — by n, date, tx / digest prefix, or the word attest
    let attest_rows = tail_jsonl(&st.dir.join("attest.jsonl"), 500);
    let want_attest = ql.contains("attest") || ql.contains("anchor");
    let mut n = 0usize;
    for r in attest_rows.iter().rev() {
        let s = r.to_string().to_lowercase();
        let row_n = r["n"].as_u64().unwrap_or(0);
        let m = want_attest || (ql.len() >= 6 && s.contains(&ql)) || (is_date && r["date"].as_str().map(|d| d.starts_with(&ql)).unwrap_or(false))
            || ql.strip_prefix("attest").and_then(|x| x.trim().parse::<u64>().ok()) == Some(row_n);
        if !m { continue; }
        hits.push(json!({"kind": "attest", "n": row_n, "date": r["date"], "ts": r["ts"], "blake3": r["blake3"], "tx": r["anchor"]["tx_hash"], "executed": r["anchor"]["executed"], "wallet": r["anchor"]["wallet"],
            "title": format!("attest row {row_n} · {} · {}", r["date"].as_str().unwrap_or("?"), r["blake3"].as_str().map(|b| &b[..12]).unwrap_or("?"))}));
        n += 1;
        if n >= limit { break; }
    }
    // 4. alerts
    let alert_rows = tail_jsonl(&st.dir.join("alerts.jsonl"), 500);
    let want_alert = ql.contains("alert");
    let mut n = 0usize;
    for r in alert_rows.iter().rev() {
        let s = r.to_string().to_lowercase();
        if !(want_alert || (ql.len() >= 4 && s.contains(&ql))) { continue; }
        hits.push(json!({"kind": "alert", "row": r, "title": format!("alert · {} · {}", r["date"].as_str().or(r["ts"].as_str()).unwrap_or("?"), r["kind"].as_str().or(r["metric"].as_str()).unwrap_or("?"))}));
        n += 1;
        if n >= limit { break; }
    }
    // the publish index and attest.jsonl carry re-publications of one reading: one line per (kind, title)
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(format!("{}|{}", h["kind"], h["title"])));
    json_value(StatusCode::OK, json!({"ok": true, "data": {"query": query, "hits": hits, "count": hits.len(), "took_ms": t0.elapsed().as_millis() as u64,
        "index_docs": index_docs, "index": idx, "hint": hint,
        "sources": ["flux-search index (sigil-earth publish)", "series.json", "attest.jsonl", "alerts.jsonl"]}}))
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

/// A live SVG badge of today's K⊕ — embeddable anywhere (README, board, a post). Colour = regime,
/// text = "K⊕ 2.81σ · elevated · 2026-09-13". Built from latest.json on every request; the browser
/// may cache it for ten minutes (the feed itself refreshes four times a day).
async fn badge(State(st): State<Arc<App>>) -> Response {
    let text = std::fs::read_to_string(st.dir.join("latest.json")).unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let k = v["today"]["k_resid"].as_f64();
    let regime = v["today"]["regime"].as_str().unwrap_or("offline");
    let date = v["today"]["date"].as_str().unwrap_or("—");
    let ladder = &v["ladder"]["k_resid"];
    let (p90, p99) = (ladder["p90"].as_f64(), ladder["p99"].as_f64());
    (StatusCode::OK, [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=600")],
     badge_svg(k, regime, date, p90, p99)).into_response()
}

/// Pure function so it can be tested without a server: the badge geometry is a shields.io-style pill.
pub fn badge_svg(k: Option<f64>, regime: &str, date: &str, p90: Option<f64>, p99: Option<f64>) -> String {
    let colour = match regime { "stable" => "#2ea44f", "elevated" => "#e0a030", "critical" => "#d0342c", _ => "#6b6b7b" };
    let value = match k { Some(k) => format!("{k:.2}σ · {regime} · {date}"), None => "offline".to_string() };
    let ladder = match (p90, p99) { (Some(a), Some(b)) => format!("p90 {a:.2} · p99 {b:.2}"), _ => String::new() };
    let label = "K⊕ Earth";
    let (lw, vw) = (7.2 * label.chars().count() as f64 + 14.0, 7.2 * value.chars().count() as f64 + 14.0);
    let w = lw + vw;
    format!(concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"20\" role=\"img\" aria-label=\"{label}: {value}\">",
        "<title>{label}: {value}{ladder_t}</title>",
        "<linearGradient id=\"s\" x2=\"0\" y2=\"100%\"><stop offset=\"0\" stop-color=\"#bbb\" stop-opacity=\".1\"/><stop offset=\"1\" stop-opacity=\".1\"/></linearGradient>",
        "<clipPath id=\"r\"><rect width=\"{w:.0}\" height=\"20\" rx=\"3\" fill=\"#fff\"/></clipPath>",
        "<g clip-path=\"url(#r)\"><rect width=\"{lw:.0}\" height=\"20\" fill=\"#555\"/><rect x=\"{lw:.0}\" width=\"{vw:.0}\" height=\"20\" fill=\"{colour}\"/><rect width=\"{w:.0}\" height=\"20\" fill=\"url(#s)\"/></g>",
        "<g fill=\"#fff\" text-anchor=\"middle\" font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" font-size=\"11\">",
        "<text x=\"{lx:.1}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{label}</text><text x=\"{lx:.1}\" y=\"14\">{label}</text>",
        "<text x=\"{vx:.1}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{value}</text><text x=\"{vx:.1}\" y=\"14\">{value}</text>",
        "</g></svg>"),
        w = w, lw = lw, vw = vw, colour = colour, label = label, value = value,
        ladder_t = if ladder.is_empty() { String::new() } else { format!(" ({ladder})") },
        lx = lw / 2.0, vx = lw + vw / 2.0)
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
        .route("/v1/earth/bio", get(bio))
        .route("/v1/earth/provenance", get(provenance))
        .route("/v1/earth/tips", get(tips))
        .route("/v1/earth/alerts", get(alerts))
        .route("/v1/earth/attest", get(attest))
        .route("/v1/earth/health", get(health))
        .route("/v1/earth/stream", get(stream))
        .route("/v1/earth/badge.svg", get(badge))
        .route("/v1/earth/search", get(search))
        .fallback(fallback)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("sigil-earth serve: listening on {bind}");
    axum::serve(listener, router.into_make_service()).await?;
    Ok(())
}

#[cfg(test)]
mod badge_tests {
    #[test]
    fn badge_reports_regime_colour_and_value_and_survives_missing_data() {
        let s = super::badge_svg(Some(2.8089), "elevated", "2026-09-13", Some(2.45), Some(3.13));
        assert!(s.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(s.contains("2.81σ · elevated · 2026-09-13"));
        assert!(s.contains("#e0a030")); // elevated = amber
        assert!(s.contains("p90 2.45 · p99 3.13"));
        let c = super::badge_svg(Some(3.5), "critical", "d", None, None);
        assert!(c.contains("#d0342c") && !c.contains("p90"));
        let o = super::badge_svg(None, "offline", "—", None, None);
        assert!(o.contains(">offline<") && o.contains("#6b6b7b"));
    }
}
