//! Push delivery of settlement — the *delivery* half of instant finality.
//!
//! The chain settles a payment the moment the committee certifies the block that
//! carries it (Phase 3, 2026-09-08: certificate one block behind the tip, measured).
//! Until this module existed every client learned that by *polling*
//! `/v1/transactions/:hash` every 2 s, so "instant" finality was delivered on a
//! 2-second sampling clock plus whatever the proxy added. This module gives the node
//! three ways to *push* the answer instead:
//!
//! | surface | shape | who it is for |
//! |---|---|---|
//! | `GET /v1/events` | Server-Sent Events, `kind` = tx / certificate / tip | anything that can hold a stream open (sigil-top, agents, a browser tab) |
//! | `GET /v1/transactions/:hash/wait` | long-poll: answers the instant the tx goes terminal, else on timeout | the browser + phone behind `sigilgraph.org`, where one short request is the robust shape |
//! | `POST /v1/webhooks` | HMAC-SHA256-signed POST to a URL when a tx settles / a certificate lands | merchants and agents that want the confirmation delivered to *them* |
//!
//! All three read one [`EventBus`]: a `tokio::sync::broadcast` channel the
//! [`crate::shielded::ShieldedBridge`] publishes into as a tx moves
//! `accepted → in_block → applied | rejected`, and the finality gate publishes
//! into on every certificate. Nothing here touches consensus: the bus is fed
//! from the same call sites that already record the outcome, after the fact.
//!
//! ## Honesty of a delivered "applied"
//!
//! `applied` is fired from the same `remember()` that `/v1/transactions/:hash` reads —
//! i.e. after `dag_drain_apply` settled the block and retired the tx. On a gated node
//! the settled line IS the certified line, so `applied` means "inside a block the
//! committee signed". A client that wants to verify rather than trust still fetches
//! `/v1/finality/certificate` and checks the votes — the `certificate` event carries
//! the same fields for exactly that purpose.
//!
//! ## What a webhook is and is not
//!
//! A webhook registration lives in RAM, bounded ([`WEBHOOK_CAP`]) and time-limited
//! ([`WEBHOOK_TTL`]): it is a *delivery request for the settlement of a payment that
//! is happening now*, not a durable subscription. Delivery is at-most-once per event
//! with one retry; a receiver that wants certainty keeps `/wait` as its fallback.
//! Targets on loopback / private / link-local ranges are refused unless
//! `SIGIL_WEBHOOK_ALLOW_PRIVATE=1` — this node lives inside a WireGuard mesh and
//! must not be turnable into a request proxy against it (same guard the K-gauge
//! webhook has).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One thing that happened that a client may be waiting for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A transaction changed status. `status` is the same vocabulary
    /// `/v1/transactions/:hash` uses: `accepted` (queued at the door), `in_block`
    /// (inside a candidate on the frontier, awaiting the certificate), `applied`
    /// (settled), `rejected` (evicted; `reason` says why).
    Tx {
        tx_hash: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        ts_ms: u64,
    },
    /// The finality gate raised the settlement line to `height` on a committee
    /// certificate. Same fields as `/v1/finality/certificate` minus the votes; fetch
    /// that route to verify the signatures.
    Certificate {
        height: u64,
        spine_block_hash: String,
        votes: usize,
        committee_size: usize,
        quorum: usize,
        ts_ms: u64,
    },
    /// The producer published a new frontier tip.
    Tip { height: u64, ts_ms: u64 },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Tx { .. } => "tx",
            Event::Certificate { .. } => "certificate",
            Event::Tip { .. } => "tip",
        }
    }
    pub fn tx_hash(&self) -> Option<&str> {
        match self {
            Event::Tx { tx_hash, .. } => Some(tx_hash.as_str()),
            _ => None,
        }
    }
    /// True for the statuses after which a tx will never change again.
    pub fn is_terminal_tx(&self) -> bool {
        matches!(self, Event::Tx { status, .. } if status == "applied" || status == "rejected")
    }
}

/// A sequenced event as delivered to subscribers. `seq` is monotonic per node
/// process and doubles as the SSE `id`, so a reconnecting client can tell what it
/// missed (the bus itself does not replay — `Last-Event-ID` is informational).
#[derive(Clone, Debug, Serialize)]
pub struct Sequenced {
    pub seq: u64,
    #[serde(flatten)]
    pub event: Event,
}

/// How many events a slow subscriber may fall behind before it is told so
/// (`RecvError::Lagged`) rather than stalling the publisher. Publishing never
/// blocks on a reader.
pub const BUS_CAPACITY: usize = 4096;

pub struct EventBus {
    tx: broadcast::Sender<Arc<Sequenced>>,
    seq: AtomicU64,
    published: AtomicU64,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self { tx, seq: AtomicU64::new(0), published: AtomicU64::new(0) }
    }

    /// Publish one event. Never blocks, never fails: with no subscriber the event is
    /// simply dropped (the bus is delivery, not a log). Returns the sequence number.
    pub fn publish(&self, event: Event) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.published.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Arc::new(Sequenced { seq, event }));
        seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Sequenced>> {
        self.tx.subscribe()
    }

    /// Live subscriber count — `/v1/events` readers plus the webhook dispatcher.
    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    pub fn tx_event(&self, hash: &[u8; 32], status: &str, reason: Option<String>) -> u64 {
        self.publish(Event::Tx {
            tx_hash: hex::encode(hash),
            status: status.to_string(),
            reason,
            ts_ms: now_ms(),
        })
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ── webhooks ─────────────────────────────────────────────────────────────────

/// Most registrations held at once. Oldest expire first when full.
pub const WEBHOOK_CAP: usize = 256;
/// How long a registration lives. A shielded tx is dropped by the bridge after
/// 40 min at the latest, so an hour covers every payment's whole lifetime.
pub const WEBHOOK_TTL: Duration = Duration::from_secs(3600);
/// Per-delivery HTTP timeout, and the pause before the single retry.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const RETRY_AFTER: Duration = Duration::from_secs(2);
/// Header carrying the HMAC-SHA256 of the exact body bytes, keyed by the
/// registration's secret: `X-Sigil-Signature: sha256=<hex>`.
pub const SIGNATURE_HEADER: &str = "X-Sigil-Signature";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebhookRequest {
    /// Where to POST. `http://` or `https://`; not a private address unless the
    /// node allows it.
    pub url: String,
    /// HMAC-SHA256 key. If omitted the node generates one and returns it ONCE.
    #[serde(default)]
    pub secret: Option<String>,
    /// Only deliver events about this transaction (64-hex). Omit for every tx.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Which kinds to deliver: any of `tx`, `certificate`, `tip`. Default `["tx"]`.
    /// `tip` fires several times a second on a live producer — ask for it knowingly.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Stop after the first terminal tx event (default true when `tx_hash` is set).
    #[serde(default)]
    pub once: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebhookRecord {
    pub id: String,
    pub url: String,
    pub tx_hash: Option<String>,
    pub kinds: Vec<String>,
    pub once: bool,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub delivered: u64,
    pub failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

struct Registration {
    record: WebhookRecord,
    secret: Vec<u8>,
    created: Instant,
    /// Set once a `once` registration has delivered its terminal event; the entry
    /// is then dropped on the next sweep.
    done: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WebhookError {
    BadUrl(&'static str),
    PrivateTarget,
    BadTxHash,
    BadKind(String),
    Full,
}

impl WebhookError {
    pub fn message(&self) -> String {
        match self {
            Self::BadUrl(why) => format!("url: {why}"),
            Self::PrivateTarget => {
                "url points at a loopback / private / link-local address; this node does not \
                 deliver there (set SIGIL_WEBHOOK_ALLOW_PRIVATE=1 on the node to allow it)"
                    .into()
            }
            Self::BadTxHash => "tx_hash must be 64-hex".into(),
            Self::BadKind(k) => format!("unknown event kind {k:?} (tx, certificate, tip)"),
            Self::Full => format!("too many webhooks registered (cap {WEBHOOK_CAP}); try again later"),
        }
    }
}

/// The in-RAM registry plus the dispatcher's view of it.
#[derive(Default)]
pub struct WebhookRegistry {
    inner: Mutex<HashMap<String, Registration>>,
    allow_private: bool,
}

impl WebhookRegistry {
    pub fn new() -> Self {
        Self::from_env()
    }

    pub fn from_env() -> Self {
        let allow_private = std::env::var("SIGIL_WEBHOOK_ALLOW_PRIVATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self::with_policy(allow_private)
    }

    pub fn with_policy(allow_private: bool) -> Self {
        Self { inner: Mutex::new(HashMap::new()), allow_private }
    }

    /// Validate and store a registration. Returns the record and the secret (the
    /// only time the secret is ever returned when the node generated it).
    pub fn register(&self, req: WebhookRequest) -> Result<(WebhookRecord, String), WebhookError> {
        let host = url_host(&req.url)?;
        if !self.allow_private && host_is_private(&host) {
            return Err(WebhookError::PrivateTarget);
        }
        let tx_hash = match req.tx_hash.as_deref() {
            None => None,
            Some(h) => {
                let h = h.strip_prefix("0x").unwrap_or(h);
                if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(WebhookError::BadTxHash);
                }
                Some(h.to_ascii_lowercase())
            }
        };
        let kinds = match req.kinds {
            None => vec!["tx".to_string()],
            Some(ks) => {
                for k in &ks {
                    if !matches!(k.as_str(), "tx" | "certificate" | "tip") {
                        return Err(WebhookError::BadKind(k.clone()));
                    }
                }
                if ks.is_empty() { vec!["tx".to_string()] } else { ks }
            }
        };
        let once = req.once.unwrap_or(tx_hash.is_some());
        let secret = match req.secret {
            Some(s) if !s.is_empty() => s,
            _ => random_hex(32),
        };
        let mut g = self.inner.lock().unwrap();
        Self::sweep_locked(&mut g);
        if g.len() >= WEBHOOK_CAP {
            return Err(WebhookError::Full);
        }
        let id = format!("wh_{}", random_hex(8));
        let now = now_ms();
        let record = WebhookRecord {
            id: id.clone(),
            url: req.url,
            tx_hash,
            kinds,
            once,
            created_ms: now,
            expires_ms: now + WEBHOOK_TTL.as_millis() as u64,
            delivered: 0,
            failed: 0,
            last_status: None,
            last_error: None,
        };
        g.insert(
            id,
            Registration {
                record: record.clone(),
                secret: secret.as_bytes().to_vec(),
                created: Instant::now(),
                done: false,
            },
        );
        Ok((record, secret))
    }

    pub fn get(&self, id: &str) -> Option<WebhookRecord> {
        self.inner.lock().unwrap().get(id).map(|r| r.record.clone())
    }

    pub fn remove(&self, id: &str) -> bool {
        self.inner.lock().unwrap().remove(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn sweep_locked(g: &mut HashMap<String, Registration>) {
        g.retain(|_, r| !r.done && r.created.elapsed() < WEBHOOK_TTL);
    }

    /// Every registration that wants `ev`, as `(id, url, secret)`. Marks `once`
    /// registrations done when `ev` is their terminal tx event.
    fn targets_for(&self, ev: &Event) -> Vec<(String, String, Vec<u8>)> {
        let mut g = self.inner.lock().unwrap();
        Self::sweep_locked(&mut g);
        let kind = ev.kind();
        let mut out = Vec::new();
        for (id, r) in g.iter_mut() {
            if !r.record.kinds.iter().any(|k| k == kind) {
                continue;
            }
            if let Some(want) = r.record.tx_hash.as_deref() {
                match ev.tx_hash() {
                    Some(h) if h == want => {}
                    _ => continue,
                }
            }
            out.push((id.clone(), r.record.url.clone(), r.secret.clone()));
            if r.record.once && ev.is_terminal_tx() {
                r.done = true;
            }
        }
        out
    }

    fn note_result(&self, id: &str, status: Option<u16>, err: Option<String>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.get_mut(id) {
            match (status, &err) {
                (Some(s), None) if (200..300).contains(&s) => r.record.delivered += 1,
                _ => r.record.failed += 1,
            }
            r.record.last_status = status;
            r.record.last_error = err;
        }
    }
}

/// `sha256=<hex>` over `body` keyed by `secret` — what goes in [`SIGNATURE_HEADER`].
pub fn sign_body(secret: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Verify a received `X-Sigil-Signature` against the body — the receiver's half of
/// [`sign_body`], kept here so the two cannot drift.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Some(hex_sig) = header.strip_prefix("sha256=") else { return false };
    let Ok(sig) = hex::decode(hex_sig) else { return false };
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// The body a receiver gets: the sequenced event plus which registration it was for.
#[derive(Serialize)]
struct Delivery<'a> {
    webhook_id: &'a str,
    #[serde(flatten)]
    event: &'a Sequenced,
    node_ts_ms: u64,
}

/// One blocking POST with the signed body. Returns the HTTP status or an error string.
fn post_once(url: &str, body: &[u8], signature: &str, webhook_id: &str) -> Result<u16, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(DELIVERY_TIMEOUT)
        .redirects(0)
        .build();
    let resp = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set(SIGNATURE_HEADER, signature)
        .set("X-Sigil-Webhook-Id", webhook_id)
        .set("User-Agent", "sigil-node-webhooks/1")
        .send_bytes(body);
    match resp {
        Ok(r) => Ok(r.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(e) => Err(e.to_string()),
    }
}

/// Run the dispatcher: subscribe to the bus and deliver every matching event to every
/// registration, on blocking threads so the node's runtime never waits on a receiver.
/// Returns when the bus is dropped.
pub async fn run_dispatcher(bus: Arc<EventBus>, registry: Arc<WebhookRegistry>) {
    let mut rx = bus.subscribe();
    loop {
        let item = match rx.recv().await {
            Ok(it) => it,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("⚠ webhook dispatcher lagged {n} events (receivers slower than the chain)");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let targets = registry.targets_for(&item.event);
        if targets.is_empty() {
            continue;
        }
        for (id, url, secret) in targets {
            let registry = registry.clone();
            let item = item.clone();
            tokio::task::spawn_blocking(move || {
                let body = serde_json::to_vec(&Delivery {
                    webhook_id: &id,
                    event: &item,
                    node_ts_ms: now_ms(),
                })
                .unwrap_or_default();
                let sig = sign_body(&secret, &body);
                let mut result = post_once(&url, &body, &sig, &id);
                if !matches!(result, Ok(s) if (200..300).contains(&s)) {
                    std::thread::sleep(RETRY_AFTER);
                    result = post_once(&url, &body, &sig, &id);
                }
                match result {
                    Ok(status) => registry.note_result(&id, Some(status), None),
                    Err(e) => registry.note_result(&id, None, Some(e)),
                }
            });
        }
    }
}

// ── URL policy ───────────────────────────────────────────────────────────────

/// Host part of an http(s) URL, without a full URL parser dependency.
fn url_host(url: &str) -> Result<String, WebhookError> {
    let rest = if let Some(r) = url.strip_prefix("https://") {
        r
    } else if let Some(r) = url.strip_prefix("http://") {
        r
    } else {
        return Err(WebhookError::BadUrl("must start with http:// or https://"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err(WebhookError::BadUrl("missing host"));
    }
    if url.len() > 2048 {
        return Err(WebhookError::BadUrl("longer than 2048 bytes"));
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("").to_string()
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() {
        return Err(WebhookError::BadUrl("missing host"));
    }
    Ok(host.to_ascii_lowercase())
}

/// Loopback, private (RFC 1918 / ULA), link-local, unspecified, or a name that
/// resolves only to such addresses. A name that fails to resolve is treated as
/// private too — refusing is the safe default for a node inside a mesh.
fn host_is_private(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_is_private(ip);
    }
    use std::net::ToSocketAddrs;
    match (host, 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let mut any = false;
            for a in addrs {
                any = true;
                if ip_is_private(a.ip()) {
                    return true;
                }
            }
            !any
        }
        Err(_) => true,
    }
}

fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_private(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn random_hex(n_bytes: usize) -> String {
    // The node already depends on blake3; hashing a fresh nanosecond timestamp,
    // the process id and a counter is not a CSPRNG but these are opaque ids and
    // fallback secrets, and the caller may always supply its own secret.
    static CTR: AtomicU64 = AtomicU64::new(0);
    let mut h = blake3::Hasher::new();
    h.update(&now_ms().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(&CTR.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    h.update(
        &SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    let out = h.finalize();
    hex::encode(&out.as_bytes()[..n_bytes.min(32)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bus_delivers_in_order_with_monotonic_seq() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let h = [7u8; 32];
        bus.tx_event(&h, "accepted", None);
        bus.tx_event(&h, "in_block", None);
        bus.tx_event(&h, "applied", None);
        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        let c = rx.recv().await.unwrap();
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert!(!a.event.is_terminal_tx());
        assert!(c.event.is_terminal_tx());
        assert_eq!(c.event.tx_hash(), Some(hex::encode(h).as_str()));
    }

    #[test]
    fn publishing_without_subscribers_is_a_no_op() {
        let bus = EventBus::new();
        assert_eq!(bus.publish(Event::Tip { height: 1, ts_ms: 0 }), 1);
        assert_eq!(bus.published(), 1);
        assert_eq!(bus.subscribers(), 0);
    }

    #[test]
    fn event_json_is_tagged_by_kind() {
        let v = serde_json::to_value(Event::Tx {
            tx_hash: "ab".into(),
            status: "applied".into(),
            reason: None,
            ts_ms: 5,
        })
        .unwrap();
        assert_eq!(v["kind"], "tx");
        assert_eq!(v["status"], "applied");
        assert!(v.get("reason").is_none());
        let s = Sequenced { seq: 9, event: Event::Tip { height: 3, ts_ms: 1 } };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["seq"], 9);
        assert_eq!(v["kind"], "tip");
        assert_eq!(v["height"], 3);
    }

    #[test]
    fn signature_round_trips_and_rejects_tamper() {
        let sig = sign_body(b"k", b"{\"a\":1}");
        assert!(sig.starts_with("sha256="));
        assert!(verify_signature(b"k", b"{\"a\":1}", &sig));
        assert!(!verify_signature(b"k", b"{\"a\":2}", &sig));
        assert!(!verify_signature(b"other", b"{\"a\":1}", &sig));
        assert!(!verify_signature(b"k", b"{\"a\":1}", "sha256=zz"));
        assert!(!verify_signature(b"k", b"{\"a\":1}", "md5=00"));
    }

    #[test]
    fn private_targets_are_refused_by_default_and_allowed_by_policy() {
        let strict = WebhookRegistry::with_policy(false);
        for url in [
            "http://127.0.0.1:8084/hook",
            "http://localhost/hook",
            "http://10.77.0.5:18181/x",
            "http://192.168.1.2/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/",
        ] {
            let e = strict
                .register(WebhookRequest { url: url.into(), secret: None, tx_hash: None, kinds: None, once: None })
                .unwrap_err();
            assert_eq!(e, WebhookError::PrivateTarget, "{url}");
        }
        let open = WebhookRegistry::with_policy(true);
        let (rec, secret) = open
            .register(WebhookRequest {
                url: "http://127.0.0.1:1/hook".into(),
                secret: None,
                tx_hash: None,
                kinds: None,
                once: None,
            })
            .unwrap();
        assert!(rec.id.starts_with("wh_"));
        assert_eq!(secret.len(), 64);
        assert_eq!(rec.kinds, vec!["tx"]);
        assert!(!rec.once);
    }

    #[test]
    fn url_and_field_validation() {
        let r = WebhookRegistry::with_policy(true);
        let mk = |url: &str| WebhookRequest { url: url.into(), secret: None, tx_hash: None, kinds: None, once: None };
        assert!(matches!(r.register(mk("ftp://x/")).unwrap_err(), WebhookError::BadUrl(_)));
        assert!(matches!(r.register(mk("http:///nohost")).unwrap_err(), WebhookError::BadUrl(_)));
        let mut bad_tx = mk("http://127.0.0.1:1/");
        bad_tx.tx_hash = Some("abc".into());
        assert_eq!(r.register(bad_tx).unwrap_err(), WebhookError::BadTxHash);
        let mut bad_kind = mk("http://127.0.0.1:1/");
        bad_kind.kinds = Some(vec!["block".into()]);
        assert!(matches!(r.register(bad_kind).unwrap_err(), WebhookError::BadKind(_)));
        let mut once = mk("http://127.0.0.1:1/");
        once.tx_hash = Some("0x".to_string() + &"A".repeat(64));
        let (rec, _) = r.register(once).unwrap();
        assert!(rec.once, "a per-tx registration defaults to once");
        assert_eq!(rec.tx_hash.as_deref(), Some("a".repeat(64).as_str()));
        assert!(r.get(&rec.id).is_some());
        assert!(r.remove(&rec.id));
        assert!(r.get(&rec.id).is_none());
    }

    #[test]
    fn targets_filter_by_kind_and_hash_and_retire_once() {
        let r = WebhookRegistry::with_policy(true);
        let h = "b".repeat(64);
        let (per_tx, _) = r
            .register(WebhookRequest {
                url: "http://127.0.0.1:1/a".into(),
                secret: Some("s".into()),
                tx_hash: Some(h.clone()),
                kinds: None,
                once: None,
            })
            .unwrap();
        let (all_certs, _) = r
            .register(WebhookRequest {
                url: "http://127.0.0.1:1/b".into(),
                secret: Some("s".into()),
                tx_hash: None,
                kinds: Some(vec!["certificate".into()]),
                once: Some(false),
            })
            .unwrap();
        let other = Event::Tx { tx_hash: "c".repeat(64), status: "applied".into(), reason: None, ts_ms: 0 };
        assert!(r.targets_for(&other).is_empty(), "wrong hash, no cert subscribers for tx");
        let pending = Event::Tx { tx_hash: h.clone(), status: "in_block".into(), reason: None, ts_ms: 0 };
        let t = r.targets_for(&pending);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, per_tx.id);
        assert!(r.get(&per_tx.id).is_some(), "not terminal yet");
        let applied = Event::Tx { tx_hash: h.clone(), status: "applied".into(), reason: None, ts_ms: 0 };
        assert_eq!(r.targets_for(&applied).len(), 1);
        assert!(r.targets_for(&applied).is_empty(), "once → retired after the terminal event");
        let cert = Event::Certificate {
            height: 1,
            spine_block_hash: "00".into(),
            votes: 2,
            committee_size: 2,
            quorum: 2,
            ts_ms: 0,
        };
        let t = r.targets_for(&cert);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, all_certs.id);
    }

    /// The whole delivery path against a real listener: register (private allowed),
    /// publish, receive the signed POST, verify the HMAC over the exact bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatcher_delivers_a_signed_post_that_verifies() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let got = Arc::new(Mutex::new(None::<(String, Vec<u8>)>));
        let got2 = got.clone();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = s.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let clen: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    while buf.len() < pos + 4 + clen {
                        let n = s.read(&mut tmp).unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let sig = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("x-sigil-signature:").map(|_| l.split_once(':').unwrap().1.trim().to_string()))
                        .unwrap_or_default();
                    *got2.lock().unwrap() = Some((sig, buf[pos + 4..pos + 4 + clen].to_vec()));
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    break;
                }
            }
        });
        let bus = Arc::new(EventBus::new());
        let reg = Arc::new(WebhookRegistry::with_policy(true));
        let h = "d".repeat(64);
        let (rec, secret) = reg
            .register(WebhookRequest {
                url: format!("http://127.0.0.1:{port}/hook"),
                secret: Some("shared-secret".into()),
                tx_hash: Some(h.clone()),
                kinds: None,
                once: None,
            })
            .unwrap();
        let disp = tokio::spawn(run_dispatcher(bus.clone(), reg.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(Event::Tx { tx_hash: h.clone(), status: "applied".into(), reason: None, ts_ms: 1 });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if got.lock().unwrap().is_some() { break; }
            assert!(Instant::now() < deadline, "no delivery within 5 s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (sig, body) = got.lock().unwrap().clone().unwrap();
        assert!(verify_signature(secret.as_bytes(), &body, &sig), "signature must verify over the exact body");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["kind"], "tx");
        assert_eq!(v["status"], "applied");
        assert_eq!(v["tx_hash"], h);
        assert_eq!(v["webhook_id"], rec.id);
        assert_eq!(v["seq"], 1);
        // bookkeeping lands after the POST returns
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let r = reg.get(&rec.id);
            if r.as_ref().map(|r| r.delivered).unwrap_or(1) == 1 { break; }
            if r.is_none() { break; } // once → swept already, also fine
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(bus);
        disp.abort();
    }
}
