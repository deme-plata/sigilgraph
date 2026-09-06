//! The Kristensen K-gauge, as a card on the Node tab.
//!
//! ## What it measures, and what it deliberately does not
//!
//! Not "is the chain fast". K is the **coordination pressure** in one round: energetic
//! disagreement (ΔH) × the round's duration (τ) × the plurality of who may propose (Δs).
//!
//! ```text
//! K  = 2π·√(ΔH·Δs) / τ           the engineering gauge (units: s^-1)
//! K* = 2π·√(ΔH·τ·Δs / ħ), ħ:=1   dimensionless; K*≈1 is the Margolus–Levitin boundary
//! ```
//!
//! 🪤 **Δs is proposer ENTROPY, never block-rate deviation.** The live node-side gauge
//! (`q-api-server/src/k_parameter_gauge.rs`) substitutes a block-rate deviation for Δs, and
//! that substitution makes the reading *blind to churn* — a chain can shed every peer it has
//! while producing blocks on cadence and the gauge will not move. Block-rate deviation IS
//! shown here, labelled `blk-dev`, and is never fed into K.
//!
//! ## Provenance discipline
//!
//! Every channel says where it came from. ΔH is a **lower bound**: it carries the mining
//! reject ratio and peer churn, but not P2P byte asymmetry, because `sigil-api` exposes no
//! byte counters. A channel with no data source reports that rather than contributing a
//! zero that would read as "healthy".
//!
//! The arithmetic is deliberately identical to the browser chip in
//! `gui/sigil-wallet-tron-embedded.html` and to the `flux_sigil_kgauge` MCP tool, so the
//! three surfaces cannot disagree about the same chain.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::heroes::card_block;
use crate::{C_CYAN, C_DIM, C_GOLD, C_GREEN, C_NEON_CYAN, C_RED, C_VBRIGHT};

/// One complete reading. Every field is measured or explicitly a bound — nothing is asserted.
#[derive(Clone, Debug, Default)]
pub(crate) struct KReading {
    pub k: f64,
    pub k_star: f64,
    /// Energetic disagreement — reject ratio + peer churn. A LOWER BOUND (no byte counters).
    pub delta_h: f64,
    /// Proposer entropy in bits. The true Δs.
    pub delta_s_bits: f64,
    /// Window length actually elapsed, seconds.
    pub tau: f64,
    /// Observer coverage Ω = 1 − e^(−peers/n_total).
    pub omega: f64,
    /// Commitment Λ — window-limited, so it says more about the sample than the chain.
    pub lambda: f64,
    pub reject_ratio: f64,
    pub churn: f64,
    /// Block-rate deviation vs the configured target. DIAGNOSTIC ONLY — never used as Δs.
    pub blk_rate_dev: f64,
    pub observed_bps: f64,
    pub distinct_producers: usize,
    pub blocks_in_window: usize,
    pub tip: u64,
    pub peers: u64,
    /// Which term dominates — the one worth acting on.
    pub dominant: String,
}

impl KReading {
    /// K < 5 stable · 5–10 approaching · ≥ 10 critical. Thresholds from flux-kgauge v4.
    pub(crate) fn regime(&self) -> &'static str {
        if !self.k.is_finite() {
            "unknown"
        } else if self.k >= 10.0 {
            "critical"
        } else if self.k >= 5.0 {
            "approaching"
        } else {
            "stable"
        }
    }
}

/// The latest reading, plus whether a sample is in flight. `None` until the first completes.
static GAUGE: OnceLock<Mutex<(Option<KReading>, bool)>> = OnceLock::new();

fn cell() -> &'static Mutex<(Option<KReading>, bool)> {
    GAUGE.get_or_init(|| Mutex::new((None, false)))
}

pub(crate) fn latest() -> (Option<KReading>, bool) {
    cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Turn the configured status URL into the origin the `/v1/*` endpoints hang off.
///
/// `cfg.api` points at a STATUS document (`https://sigilgraph.org/api/v1/status`,
/// `http://127.0.0.1:18181/v1/status`, …) but the gauge needs three sibling endpoints, so
/// the path has to come off. Trimming a known suffix — rather than assuming a shape — keeps
/// a custom `--api` working instead of silently sampling the wrong origin.
pub(crate) fn api_base_of(api: &str) -> String {
    let mut b = api.trim_end_matches('/');
    for suffix in ["/api/v1/status", "/v1/status", "/api/v1", "/v1", "/status"] {
        if let Some(t) = b.strip_suffix(suffix) {
            b = t;
            break;
        }
    }
    b.trim_end_matches('/').to_string()
}

// ── sampling ────────────────────────────────────────────────────────────────────────────

/// Pull one `f64` out of a flat JSON object without a serde model — the node's shapes drift
/// between versions and a hard model would turn a new field into a blank card.
fn num(body: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let i = body.find(&pat)? + pat.len();
    let rest = &body[i..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == '+'))
        .unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// Count how many blocks each producer proposed in the window.
///
/// `/v1/dagknight/recent` renders a producer as a byte array, so the key is the first few
/// bytes of that array verbatim — enough to separate proposers, and it needs no knowledge of
/// how the node happens to encode an identity this week.
fn producer_counts(body: &str) -> HashMap<String, u64> {
    let mut counts = HashMap::new();
    for seg in body.split("\"producer\":").skip(1) {
        let t = seg.trim_start();
        // Take the VALUE and stop at its delimiter. A fixed-width prefix reads past the
        // end of a short array into whatever field follows, which makes two identical
        // proposers look distinct — and proposer plurality is exactly what Δs measures,
        // so that error inflates entropy and the whole gauge with it.
        let key = if let Some(rest) = t.strip_prefix('[') {
            rest.find(']').map(|i| rest[..i].to_string())
        } else if let Some(rest) = t.strip_prefix('"') {
            rest.find('"').map(|i| rest[..i].to_string())
        } else {
            let end = t.find(|c: char| c == ',' || c == '}').unwrap_or(t.len());
            Some(t[..end].trim().to_string())
        };
        if let Some(k) = key.filter(|k| !k.is_empty()) {
            *counts.entry(k).or_insert(0u64) += 1;
        }
    }
    counts
}

/// Shannon entropy in bits. Zero for an empty or single-outcome distribution — which is the
/// honest answer for a one-producer chain, not a defect.
fn shannon_bits(counts: &HashMap<String, u64>) -> f64 {
    let n: u64 = counts.values().sum();
    if n == 0 {
        return 0.0;
    }
    counts
        .values()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n as f64;
            -p * p.log2()
        })
        .sum()
}

/// The configured target block rate the deviation channel is measured against. Diagnostic
/// only — see the module docs for why it must never become Δs.
const TARGET_BPS: f64 = 0.83;

struct Sample {
    height: f64,
    shares_accepted: f64,
    rejects_total: f64,
    peers: f64,
    recent_body: String,
}

/// One shared blocking client. Rebuilding it per request costs ~25 ms because the rustls
/// builder reloads the bundled webpki root store every time (see `serve.rs`), and this
/// samples three endpoints twice per reading.
fn client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(4))
            .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION"), " kgauge"))
            .build()
            .unwrap_or_default()
    })
}

fn get(url: String) -> Option<String> {
    client().get(url).send().ok()?.text().ok()
}

fn take_sample(api_base: &str) -> Option<Sample> {
    let miners = get(format!("{api_base}/v1/mining/miners"))?;
    let topo = get(format!("{api_base}/v1/network/topology"))?;
    let recent = get(format!("{api_base}/v1/dagknight/recent")).unwrap_or_default();
    // rejects is [[reason, count], ...] — sum the counts, whatever the reasons happen to be.
    let rejects_total = miners
        .split("\"rejects\":")
        .nth(1)
        .map(|s| {
            let end = s.find(']').map(|i| i + 1).unwrap_or(0);
            s[..end]
                .split(',')
                .filter_map(|p| p.trim().trim_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok())
                .sum::<f64>()
        })
        .unwrap_or(0.0);
    Some(Sample {
        height: num(&miners, "height").unwrap_or(0.0),
        shares_accepted: num(&miners, "shares_accepted").unwrap_or(0.0),
        rejects_total,
        peers: num(&topo, "peer_count").unwrap_or(0.0),
        recent_body: recent,
    })
}

/// Take two samples τ apart and turn them into a reading.
///
/// τ is the WALL CLOCK between them, not the nominal sleep — a stalled node makes the
/// interval longer than requested and K must reflect the round that actually happened.
fn measure(api_base: &str, window: Duration) -> Option<KReading> {
    let t0 = Instant::now();
    let a = take_sample(api_base)?;
    std::thread::sleep(window);
    let b = take_sample(api_base)?;
    let tau = t0.elapsed().as_secs_f64().max(0.001);

    let counts = producer_counts(&b.recent_body);
    let delta_s_bits = shannon_bits(&counts);
    let distinct_producers = counts.len();
    let blocks_in_window = counts.values().sum::<u64>() as usize;

    let observed_bps = ((b.height - a.height).max(0.0)) / tau;
    let blk_rate_dev = ((observed_bps - TARGET_BPS).abs()) / TARGET_BPS;

    let shares_delta = (b.shares_accepted - a.shares_accepted).max(0.0);
    let rejects = b.rejects_total;
    let reject_ratio = if shares_delta + rejects > 0.0 { rejects / (shares_delta + rejects) } else { 0.0 };
    let churn = if a.peers > 0.0 { (b.peers - a.peers).abs() / a.peers } else { 0.0 };
    // No byte counters on sigil-api, so traffic asymmetry contributes NOTHING rather than a
    // zero pretending to be a measurement. ΔH is therefore a lower bound, and says so.
    let delta_h = reject_ratio + churn;

    let k = 2.0 * std::f64::consts::PI * (delta_h * delta_s_bits).sqrt() / tau;
    let k_star = 2.0 * std::f64::consts::PI * (delta_h * tau * delta_s_bits).sqrt();

    let n_total = (b.peers + 2.0).max(8.0);
    let omega = 1.0 - (-b.peers / n_total).exp();
    let d_commit = (b.height - a.height).max(0.0);
    let lambda = 1.0 - (-d_commit / (18.0 * 100.0)).exp();

    let dominant = if delta_h == 0.0 && delta_s_bits == 0.0 {
        "none (quiet chain)".to_string()
    } else if delta_h >= delta_s_bits {
        "energetic disagreement (ΔH)".to_string()
    } else {
        "proposer plurality (Δs)".to_string()
    };

    Some(KReading {
        k,
        k_star,
        delta_h,
        delta_s_bits,
        tau,
        omega,
        lambda,
        reject_ratio,
        churn,
        blk_rate_dev,
        observed_bps,
        distinct_producers,
        blocks_in_window,
        tip: b.height as u64,
        peers: b.peers as u64,
        dominant,
    })
}

/// Start the background sampler. Idempotent — a second call is a no-op, so wiring it into
/// more than one entry point cannot produce two threads fighting over the same cell.
pub(crate) fn spawn(api_base: String) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("kgauge".into())
        .spawn(move || loop {
            cell().lock().unwrap_or_else(|e| e.into_inner()).1 = true;
            // 6 s matches the browser chip, so the two surfaces sample the same shape of
            // window and their numbers stay comparable.
            let r = measure(&api_base, Duration::from_secs(6));
            {
                let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
                g.1 = false;
                if r.is_some() {
                    g.0 = r;
                }
            }
            std::thread::sleep(Duration::from_secs(90));
        })
        .ok();
}

// ── the card ────────────────────────────────────────────────────────────────────────────

fn dim<T: Into<String>>(s: T) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(C_DIM))
}

/// The Node tab's K-gauge card: the gauge, every input that produced it, and the provenance
/// of each — so a reading can never be mistaken for more evidence than it is.
pub(crate) fn render_kgauge_card(_app: &crate::App) -> Paragraph<'static> {
    let (reading, busy) = latest();
    let Some(r) = reading else {
        let msg = if busy { "measuring — two samples 6 s apart…" } else { "no reading yet" };
        return Paragraph::new(vec![
            Line::from(vec![dim("K "), Span::styled("—", Style::default().fg(C_DIM))]),
            Line::from(dim(msg)),
            Line::from(dim("K = 2π·√(ΔH·Δs)/τ")),
        ])
        .block(card_block(" Κ  KRISTENSEN K-GAUGE", C_VBRIGHT));
    };

    let regime = r.regime();
    let col = match regime {
        "critical" => C_RED,
        "approaching" => C_GOLD,
        "unknown" => C_DIM,
        _ => C_GREEN,
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("K ", Style::default().fg(C_DIM)),
            Span::styled(format!("{:.4}", r.k), Style::default().fg(col).add_modifier(Modifier::BOLD)),
            dim("  K* "),
            Span::styled(format!("{:.3}", r.k_star), Style::default().fg(C_NEON_CYAN)),
            dim(" (≈1 = ML bound)  "),
            Span::styled(
                regime.to_uppercase(),
                Style::default().fg(col).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            dim("ΔH "),
            Span::styled(format!("{:.4}", r.delta_h), Style::default().fg(C_CYAN)),
            // Named a bound in place. A number that is only a floor must never be printed
            // as though it were the measurement.
            dim(" (lower bound)  rej "),
            Span::styled(format!("{:.3}", r.reject_ratio), Style::default().fg(C_DIM)),
            dim("  churn "),
            Span::styled(format!("{:.3}", r.churn), Style::default().fg(C_DIM)),
        ]),
        Line::from(vec![
            dim("Δs "),
            Span::styled(format!("{:.3} bits", r.delta_s_bits), Style::default().fg(C_CYAN)),
            dim("  proposer entropy · "),
            Span::styled(format!("{}", r.distinct_producers), Style::default().fg(C_GOLD)),
            dim(format!(" producer(s) / {} blk", r.blocks_in_window)),
        ]),
        Line::from(vec![
            dim("τ  "),
            Span::styled(format!("{:.1} s", r.tau), Style::default().fg(C_CYAN)),
            dim("  Ω "),
            Span::styled(format!("{:.3}", r.omega), Style::default().fg(C_CYAN)),
            dim(format!(" ({} peers)  Λ ", r.peers)),
            Span::styled(format!("{:.3}", r.lambda), Style::default().fg(C_CYAN)),
            dim(" (window-limited)"),
        ]),
        Line::from(vec![
            dim("rate "),
            Span::styled(format!("{:.2} blk/s", r.observed_bps), Style::default().fg(C_GOLD)),
            dim("  blk-dev "),
            Span::styled(format!("{:.2}", r.blk_rate_dev), Style::default().fg(C_DIM)),
            // The whole point of showing it: it is visible AND excluded.
            dim(" — diagnostic, NOT Δs"),
        ]),
        Line::from(vec![
            dim("dominant "),
            Span::styled(r.dominant.clone(), Style::default().fg(C_VBRIGHT)),
        ]),
        Line::from(vec![
            dim("unmeasured: P2P byte asymmetry (no counters) · tip "),
            Span::styled(format!("{}", r.tip), Style::default().fg(C_DIM)),
        ]),
    ];
    Paragraph::new(lines).block(card_block(" Κ  KRISTENSEN K-GAUGE", C_VBRIGHT))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-producer chain has zero proposer entropy — that is the correct answer, and it
    /// is what makes SIGIL read K*≈0 today.
    #[test]
    fn single_producer_has_zero_entropy() {
        let mut c = HashMap::new();
        c.insert("epsilon".to_string(), 200u64);
        assert_eq!(shannon_bits(&c), 0.0);
    }

    /// Two producers splitting a window evenly is exactly one bit.
    #[test]
    fn even_two_producer_split_is_one_bit() {
        let mut c = HashMap::new();
        c.insert("a".to_string(), 100u64);
        c.insert("b".to_string(), 100u64);
        assert!((shannon_bits(&c) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_window_is_zero_not_nan() {
        assert_eq!(shannon_bits(&HashMap::new()), 0.0);
    }

    /// The regime thresholds are the flux-kgauge v4 ones and are load-bearing for the colour.
    #[test]
    fn regime_thresholds() {
        let mk = |k: f64| KReading { k, ..Default::default() };
        assert_eq!(mk(0.0).regime(), "stable");
        assert_eq!(mk(4.999).regime(), "stable");
        assert_eq!(mk(5.0).regime(), "approaching");
        assert_eq!(mk(9.999).regime(), "approaching");
        assert_eq!(mk(10.0).regime(), "critical");
        assert_eq!(mk(f64::NAN).regime(), "unknown");
    }

    /// The base must survive every URL shape a user can legitimately configure.
    #[test]
    fn api_base_strips_whatever_status_path_was_configured() {
        assert_eq!(api_base_of("https://sigilgraph.org/api/v1/status"), "https://sigilgraph.org");
        assert_eq!(api_base_of("http://127.0.0.1:18181/v1/status"), "http://127.0.0.1:18181");
        assert_eq!(api_base_of("http://127.0.0.1:18181/v1/status/"), "http://127.0.0.1:18181");
        assert_eq!(api_base_of("http://127.0.0.1:18181"), "http://127.0.0.1:18181");
        // A bare origin with a trailing slash must not become an empty string.
        assert_eq!(api_base_of("http://box:8080/"), "http://box:8080");
    }

    #[test]
    fn num_reads_ints_floats_and_negatives() {
        let b = r#"{"height":4141675,"net_hps":245400347.0,"my_hps":0.0,"drift":-1.5}"#;
        assert_eq!(num(b, "height"), Some(4141675.0));
        assert_eq!(num(b, "net_hps"), Some(245400347.0));
        assert_eq!(num(b, "drift"), Some(-1.5));
        assert_eq!(num(b, "absent"), None);
    }

    /// Producer keys must separate DIFFERENT proposers and merge identical ones, whatever
    /// encoding the node uses for an identity.
    #[test]
    fn producer_counts_separate_distinct_proposers() {
        let body = r#"{"blocks":[{"producer":[1,2,3],"h":1},{"producer":[1,2,3],"h":2},{"producer":[9,9,9],"h":3}]}"#;
        let c = producer_counts(body);
        assert_eq!(c.len(), 2, "two distinct producers");
        assert_eq!(c.values().sum::<u64>(), 3, "three blocks accounted for");
        // The bug this pins: a fixed-width prefix ran past the short array into `"h":N`,
        // so the two blocks from the SAME producer got different keys and the chain
        // looked more plural than it is.
        assert_eq!(*c.get("1,2,3").expect("array read to its closing bracket"), 2);
        // A string-encoded identity must work the same way.
        let s = r#"{"blocks":[{"producer":"epsilon","h":1},{"producer":"epsilon","h":2}]}"#;
        let cs = producer_counts(s);
        assert_eq!(cs.len(), 1);
        assert_eq!(*cs.get("epsilon").unwrap(), 2);
    }
}
