//! The Kristensen K-gauge, as a card on the Node tab: **K_fix** and the **witness W**.
//!
//! ## What it measures
//!
//! ```text
//! K_fix = 2π·√(ΔH_c · τ_d · w_S)    ∈ [0, 2π]   stable < 1 · elevated < 3 · critical ≥ 3
//! ΔH_c  = 0.20·D_tip + 0.35·D_state + 0.20·D_fin + 0.25·D_conf   (measured channels only)
//! τ_d   = (1 + min(d, 512)/512) / 2  d = how many BLOCKS the disagreement has lasted
//! w_S   = 1 − H_norm/4               proposer entropy as a bounded weight, never a factor
//! W     = min(clock, certificate, independence, channels)   could a disagreement be SEEN?
//! ```
//!
//! ΔH_c is disagreement between NODES: blocks that had to merge several tips, peers whose
//! committed state root differs from ours at the same height, peers' distance from us in
//! settled height, and red blocks plus semantic rejects. A channel the node cannot measure is
//! reported as missing; it never contributes a zero that would read as "healthy". W is not a
//! disagreement at all: it says whether one could have been observed. A K_fix that would read
//! "stable" while W is low reads "blind", because then the reading is about the instrument,
//! not the chain. An observed disagreement is always shown at its own severity.
//!
//! ## Why the card of 2026-09-06 was replaced (2026-10-26)
//!
//! It computed K = 2π·√(ΔH·Δs)/τ. Measured on live g3 that day: (1) with one block-maker Δs = 0
//! and the product read 0 "STABLE" whatever happened, the flaw the 2026-09-14 battle test
//! convicted (one-key fork 0, the same fork with two keys 5.88); (2) its ΔH was mining-share
//! rejects, the LIFETIME total ÷ shares accepted in the last 6 s (36 / (3 + 36) = 0.923), and
//! only the first reject kind was parsed; (3) it never read the finality certificate, so a
//! committee of one looked like consensus; (4) −p·log₂p for p = 1 printed "−0.0000".
//!
//! The arithmetic is the same as `fluxc-mcp/src/handlers/sigil_kgauge.rs` (`consensus_gauge`,
//! `k_fix_of`, `witness_of`) and the wallet chip on sigilgraph.org; the tests below pin it.
//! One deliberate difference: the certificate's age is (tip − certificate height) ÷ block rate,
//! not this machine's clock minus the node's, because sigil-top usually reads a node on another
//! machine and a user's clock can be minutes off. On the producer's own host the two agree
//! (2026-10-26: 0 blocks behind, 0.09 s old). Left out on purpose: the Ω confidence band
//! (Ω = 1 − e^(−p/(p+2)) saturates at 0.632, so its upper bound reads ≈ 3 on a chain whose every
//! channel agrees) and Λ (it only counts the blocks inside the sampling window). Rejects, churn
//! and block rate stay visible as DIAGNOSTICS and never enter ΔH_c.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::Value;

use crate::heroes::card_block;
use crate::{C_CYAN, C_DIM, C_GOLD, C_GREEN, C_RED, C_VBRIGHT};

/// `sigil_dagknight::BraidConfig::default().final_depth`: the persistence scale of τ_d.
pub(crate) const FINAL_DEPTH: f64 = 512.0;
pub(crate) const W_TIP: f64 = 0.20;
pub(crate) const W_STATE: f64 = 0.35;
pub(crate) const W_FINALITY: f64 = 0.20;
pub(crate) const W_CONFLICT: f64 = 0.25;
/// A peer more than this many finality depths away is catching up, not disagreeing.
const SYNCING_DEPTHS: f64 = 2.0;
/// w_S = 1 − H_norm / W_S_DIV ∈ [¾, 1]. /4, not /2: with /2 a persistent two-party state split
/// read 2.97 "elevated"; a full state divergence must stay critical whatever the key count.
const W_S_DIV: f64 = 4.0;
/// Certificate freshness ramp in units of the finality time τ = 512 / block rate: fresh within
/// 2τ (never less than 120 s), worthless from 20τ, linear between.
const CERT_FRESH_TAUS: f64 = 2.0;
const CERT_BLIND_TAUS: f64 = 20.0;
const CERT_FRESH_MIN_SECS: f64 = 120.0;
/// τ for the certificate ramp when no block arrived in the window (the clock sense is 0 then).
const TAU0_SECS: f64 = 100.0;
/// Below this W the reading carries no information about agreement.
pub(crate) const WITNESS_BLIND_BELOW: f64 = 0.34;
/// Sampling window (6 s matches the browser chip) and the pause between readings.
const WINDOW: Duration = Duration::from_secs(6);
const PAUSE: Duration = Duration::from_secs(90);

// ── the pure gauge: no network, the tests drive it ─────────────────────────────────────

/// What a mining-reject kind says about CONSENSUS. A late or repeated share is network timing
/// ("noise"); a proof that does not verify is a competing claim about state ("semantic").
pub(crate) fn reject_class(kind: &str) -> &'static str {
    match kind {
        "stale_height" | "duplicate" | "already_known" | "late" | "no_tip" => "noise",
        "verify_mismatch" | "non_canonical_wallet" | "invalid_signature" | "bad_proof"
        | "invalid_state" | "state_root_mismatch" | "conflicting_spend" | "wrong_parent"
        | "consensus_rule" => "semantic",
        _ => "unclassified",
    }
}

/// Shannon entropy in bits of a producer distribution. Zero for one producer, which is the
/// honest answer for a one-producer chain.
pub(crate) fn shannon_bits(counts: &[u64]) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let h: f64 = counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum();
    // −1·log₂1 is −0.0 (IEEE negative zero) and a float sum keeps it: the old card printed
    // "−0.0000" for exactly this. Adding +0.0 turns −0.0 into +0.0 and changes nothing else.
    h.max(0.0) + 0.0
}

pub(crate) fn tau_d_of(persist_blocks: f64) -> f64 {
    (1.0 + persist_blocks.clamp(0.0, FINAL_DEPTH) / FINAL_DEPTH) / 2.0
}

pub(crate) fn w_s_of(h_norm: f64) -> f64 {
    1.0 - h_norm.clamp(0.0, 1.0) / W_S_DIV
}

pub(crate) fn k_fix_of(delta_h_c: f64, persist_blocks: f64, h_norm: f64) -> f64 {
    2.0 * std::f64::consts::PI * (delta_h_c.clamp(0.0, 1.0) * tau_d_of(persist_blocks) * w_s_of(h_norm)).sqrt()
}

pub(crate) fn regime_of(k: f64) -> &'static str {
    if !k.is_finite() {
        "unknown"
    } else if k < 1.0 {
        "stable"
    } else if k < 3.0 {
        "elevated"
    } else {
        "critical"
    }
}

/// Certificate freshness ∈ [0, 1] against the chain's own finality time. `None` when the node
/// serves no certificate: unknown is not health, and the witness treats it as 0.
pub(crate) fn cert_freshness(cert_age_secs: Option<f64>, tau_finality_secs: f64) -> Option<f64> {
    let age = cert_age_secs?;
    let tau = if tau_finality_secs.is_finite() && tau_finality_secs > 0.0 { tau_finality_secs } else { TAU0_SECS };
    let fresh = (CERT_FRESH_TAUS * tau).max(CERT_FRESH_MIN_SECS);
    let blind = (CERT_BLIND_TAUS * tau).max(fresh * 2.0);
    Some(if age <= fresh { 1.0 } else if age >= blind { 0.0 } else { 1.0 - (age - fresh) / (blind - fresh) })
}

/// The finality certificate as `/v1/finality/certificate` serves it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Cert {
    pub height: u64,
    pub certified_at_ms: u64,
    pub committee_size: usize,
    /// DISTINCT signers on the certificate. Not failure domains: two signers can be one host.
    pub voters: usize,
    pub bft: bool,
    pub gate: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Witness {
    pub w: f64,
    pub weakest: &'static str,
    pub clock: f64,
    pub certificate: Option<f64>,
    pub independence: f64,
    pub channels: f64,
    pub cert_lag_blocks: Option<u64>,
    pub voters: usize,
    pub committee: usize,
    pub bft: bool,
    pub gate: String,
    /// One plain sentence: what W says about this reading.
    pub verdict: String,
}

impl Witness {
    pub(crate) fn blind(&self) -> bool {
        self.w < WITNESS_BLIND_BELOW
    }
}

/// W = the MINIMUM of four senses, never the product: a chain is only as witnessed as its
/// weakest sense, and the minimum names the sense that failed.
pub(crate) fn witness_of(
    clock_alive: bool,
    cert: Option<&Cert>,
    cert_age_secs: Option<f64>,
    cert_lag_blocks: Option<u64>,
    tau_finality_secs: f64,
    missing_weight: f64,
) -> Witness {
    let clock = if clock_alive { 1.0 } else { 0.0 };
    let certificate = if cert.is_some() { cert_freshness(cert_age_secs, tau_finality_secs) } else { None };
    let channels = (1.0 - missing_weight.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let (voters, committee, bft, gate) = match cert {
        Some(c) => (c.voters, c.committee_size, c.bft, c.gate.clone()),
        None => (0, 0, false, "absent".to_string()),
    };
    // A "solo" gate is the writer attesting to itself, not a quorum: capped hard, not scaled.
    let solo = !bft || gate.eq_ignore_ascii_case("solo") || voters <= 1;
    let independence = if solo { 0.0 } else { (voters as f64 / committee.max(voters).max(1) as f64).clamp(0.0, 1.0) };
    let senses = [
        ("clock", clock),
        ("certificate", certificate.unwrap_or(0.0)),
        ("independence", independence),
        ("channels", channels),
    ];
    let (weakest, w) = senses.iter().fold(("clock", f64::INFINITY), |acc, &(n, v)| if v < acc.1 { (n, v) } else { acc });
    let verdict = if !clock_alive {
        "clock stopped: no new block, nothing was seen".to_string()
    } else if cert.is_none() {
        "no finality certificate served: unknown".to_string()
    } else if certificate == Some(0.0) {
        "certificate frozen: nobody is voting".to_string()
    } else if solo {
        "one writer attesting to its own chain".to_string()
    } else if channels < WITNESS_BLIND_BELOW {
        "most of ΔH_c cannot be measured here".to_string()
    } else if w < WITNESS_BLIND_BELOW {
        format!("weakest sense: {weakest} at {w:.2}")
    } else {
        "witnessed: low K_fix = evidence of agreement".to_string()
    };
    Witness {
        w,
        weakest,
        clock,
        certificate,
        independence,
        channels,
        cert_lag_blocks,
        voters,
        committee,
        bft,
        gate,
        verdict,
    }
}

/// The reflex arc: a stable K_fix on a blind gauge reads "blind"; anything worse keeps its
/// own severity, because an OBSERVED disagreement is evidence whatever W says.
pub(crate) fn regime_witnessed(k: f64, wit: &Witness) -> &'static str {
    let r = regime_of(k);
    if r == "stable" && wit.blind() { "blind" } else { r }
}

/// Everything the gauge needs, already measured.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    pub blocks: usize,
    pub merging_blocks: usize,
    pub red_blocks: usize,
    pub accepted_delta: u64,
    /// Rejects that happened INSIDE the window, by kind (delta of two lifetime counters).
    pub rejects_delta: HashMap<String, u64>,
    pub peer_heights: Vec<u64>,
    /// One entry per peer whose state root at ITS height could be compared with ours.
    pub state_root_verdicts: Vec<bool>,
    /// Our SETTLED height (the spine), compared with the peers' settled heights.
    pub local_height: u64,
    pub producer_counts: Vec<u64>,
    /// Mining-tip advance during the window: the clock, and the block rate.
    pub blocks_added: u64,
    pub window_secs: f64,
    pub peers_start: u64,
    pub peers: u64,
    /// The mining tip at the end of the window.
    pub tip: u64,
    pub cert: Option<Cert>,
    /// Wall-clock age of the certificate; used only when no block arrived to measure it by.
    pub cert_wall_age_secs: Option<f64>,
}

#[derive(Clone, Debug)]
pub(crate) struct Channel {
    pub name: &'static str,
    pub weight: f64,
    /// `None` = the node could not measure it. Never read as 0.
    pub value: Option<f64>,
    /// The counts behind the value, e.g. 0 of 9 peers.
    pub evidence: String,
}

/// One complete reading. Every field is measured, derived from measurements, or says it is
/// missing; nothing is asserted.
#[derive(Clone, Debug)]
pub(crate) struct KReading {
    pub k_fix: f64,
    /// stable · elevated · critical · blind (after the witness).
    pub regime: &'static str,
    /// What K_fix alone would say.
    pub regime_unwitnessed: &'static str,
    pub delta_h_c: f64,
    pub missing_weight: f64,
    pub channels: [Channel; 4],
    /// Peers whose state root differs from ours, of those the node could compare.
    pub state_differ: usize,
    pub state_compared: usize,
    pub persist_blocks: f64,
    pub tau_d: f64,
    pub w_s: f64,
    pub h_norm: f64,
    pub n_eff: f64,
    pub distinct_producers: usize,
    pub blocks: usize,
    pub witness: Witness,
    pub converged_peers: usize,
    pub syncing_peers: usize,
    pub rejects_noise: u64,
    pub rejects_semantic: u64,
    pub rejects_unclassified: u64,
    pub churn: f64,
    pub peers: u64,
    pub block_rate_bps: f64,
    pub tip: u64,
}

pub(crate) fn gauge(i: &Inputs) -> KReading {
    let n = i.blocks.max(1) as f64;

    // Rejects by kind: only semantic ones are disagreement; the rest are diagnostics.
    let (mut noise, mut semantic, mut unclassified) = (0u64, 0u64, 0u64);
    for (kind, &v) in &i.rejects_delta {
        match reject_class(kind) {
            "noise" => noise += v,
            "semantic" => semantic += v,
            _ => unclassified += v,
        }
    }
    let denom = i.accepted_delta + noise + semantic + unclassified;
    let semantic_rate = if denom > 0 { semantic as f64 / denom as f64 } else { 0.0 };

    // The four disagreement channels.
    let local = i.local_height as f64;
    let (converged, syncing): (Vec<u64>, Vec<u64>) =
        i.peer_heights.iter().copied().partition(|&h| (h as f64 - local).abs() <= SYNCING_DEPTHS * FINAL_DEPTH);
    let d_fin = if converged.is_empty() {
        None
    } else {
        Some(converged.iter().map(|&h| ((h as f64 - local).abs() / FINAL_DEPTH).min(1.0)).sum::<f64>() / converged.len() as f64)
    };
    let differ = i.state_root_verdicts.iter().filter(|&&ok| !ok).count();
    let d_state = if i.state_root_verdicts.is_empty() { None } else { Some(differ as f64 / i.state_root_verdicts.len() as f64) };
    let channels = [
        Channel { name: "tip", weight: W_TIP, value: Some(i.merging_blocks as f64 / n), evidence: format!("{} of {} blocks merged tips", i.merging_blocks, i.blocks) },
        Channel { name: "state", weight: W_STATE, value: d_state, evidence: format!("{differ} of {} peers differ", i.state_root_verdicts.len()) },
        Channel { name: "fin", weight: W_FINALITY, value: d_fin, evidence: format!("{} peers, {} syncing", converged.len(), syncing.len()) },
        Channel {
            name: "conf",
            weight: W_CONFLICT,
            value: Some((i.red_blocks as f64 / n + semantic_rate).min(1.0)),
            evidence: format!("{} red, {semantic} semantic", i.red_blocks),
        },
    ];
    let delta_h_c = channels.iter().filter_map(|c| c.value.map(|v| c.weight * v)).sum::<f64>() + 0.0;
    let missing_weight = channels.iter().filter(|c| c.value.is_none()).map(|c| c.weight).sum::<f64>() + 0.0;

    // Proposer structure: a weight, never a factor that can zero the gauge.
    let entropy = shannon_bits(&i.producer_counts);
    let distinct = i.producer_counts.iter().filter(|&&c| c > 0).count();
    let h_norm = if distinct >= 2 { (entropy / (distinct as f64).log2()).clamp(0.0, 1.0) } else { 0.0 };

    // Persistence in blocks: mean spine gap to converged peers + merging + red blocks.
    let gap = if converged.is_empty() { 0.0 } else { converged.iter().map(|&h| (h as f64 - local).abs()).sum::<f64>() / converged.len() as f64 };
    let persist_blocks = (gap + i.merging_blocks as f64 + i.red_blocks as f64).min(FINAL_DEPTH);
    let k_fix = k_fix_of(delta_h_c, persist_blocks, h_norm);

    // The witness. Certificate age by blocks, so a skewed client clock cannot age it.
    let block_rate_bps = if i.window_secs > 0.0 { i.blocks_added as f64 / i.window_secs } else { 0.0 };
    let tau_finality_secs = if block_rate_bps > 0.0 { FINAL_DEPTH / block_rate_bps } else { TAU0_SECS };
    let cert_lag_blocks = i.cert.as_ref().map(|c| i.tip.saturating_sub(c.height));
    let cert_age_secs = if block_rate_bps > 0.0 { cert_lag_blocks.map(|l| l as f64 / block_rate_bps) } else { i.cert_wall_age_secs };
    let witness = witness_of(i.blocks_added > 0, i.cert.as_ref(), cert_age_secs, cert_lag_blocks, tau_finality_secs, missing_weight);

    KReading {
        k_fix,
        regime: regime_witnessed(k_fix, &witness),
        regime_unwitnessed: regime_of(k_fix),
        delta_h_c,
        missing_weight,
        channels,
        state_differ: differ,
        state_compared: i.state_root_verdicts.len(),
        persist_blocks,
        tau_d: tau_d_of(persist_blocks),
        w_s: w_s_of(h_norm),
        h_norm,
        n_eff: 2f64.powf(entropy),
        distinct_producers: distinct,
        blocks: i.blocks,
        witness,
        converged_peers: converged.len(),
        syncing_peers: syncing.len(),
        rejects_noise: noise,
        rejects_semantic: semantic,
        rejects_unclassified: unclassified,
        churn: if i.peers_start > 0 { (i.peers as f64 - i.peers_start as f64).abs() / i.peers_start as f64 } else { 0.0 },
        peers: i.peers,
        block_rate_bps,
        tip: i.tip,
    }
}

// ── reading the node's JSON ──────────────────────────────────────────────────────────────

/// `{"data": {...}}` or the object itself: the node has served both shapes.
fn unwrap_data(v: &Value) -> &Value {
    v.get("data").unwrap_or(v)
}

/// The cumulative counters one sample reads from `/v1/mining/miners` + `/v1/network/topology`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Counters {
    /// The mining tip (DAG frontier).
    pub height: u64,
    pub shares_accepted: u64,
    /// LIFETIME rejects by kind. Only the difference of two samples is a rate.
    pub rejects_by_kind: HashMap<String, u64>,
    pub peers: u64,
    pub peer_heights: Vec<u64>,
    /// The node's own settled height (`local_view.height`), the spine peers publish too.
    pub settled_height: Option<u64>,
    pub state_root_verdicts: Vec<bool>,
}

pub(crate) fn parse_miners(v: &Value, c: &mut Counters) {
    let m = unwrap_data(v);
    c.height = m.get("height").and_then(Value::as_u64).unwrap_or(0);
    c.shares_accepted = m.get("shares_accepted").and_then(Value::as_u64).unwrap_or(0);
    // `rejects` is [[kind, count], …]. Every kind is summed under its own name.
    if let Some(arr) = m.get("rejects").and_then(Value::as_array) {
        for e in arr {
            let kind = e.get(0).and_then(Value::as_str).unwrap_or("unknown").to_string();
            *c.rejects_by_kind.entry(kind).or_insert(0) += e.get(1).and_then(Value::as_u64).unwrap_or(0);
        }
    }
}

pub(crate) fn parse_topology(v: &Value, c: &mut Counters) {
    let t = unwrap_data(v);
    c.peers = t.get("peer_count").and_then(Value::as_u64).unwrap_or(0);
    c.peer_heights = t
        .get("peer_heights")
        .and_then(Value::as_object)
        .map(|o| o.iter().filter(|(k, _)| k.as_str() != "last").filter_map(|(_, v)| v.as_u64()).filter(|&h| h > 0).collect())
        .unwrap_or_default();
    c.settled_height = t.get("local_view").and_then(|v| v.get("height")).and_then(Value::as_u64);
    // A peer whose view the node could not compare (`null`) is unknown, not a disagreement.
    c.state_root_verdicts = t
        .get("peer_views")
        .and_then(Value::as_object)
        .map(|o| o.values().filter_map(|pv| pv.get("state_root_matches_local").and_then(Value::as_bool)).collect())
        .unwrap_or_default();
}

pub(crate) fn parse_cert(v: &Value) -> Option<Cert> {
    let c = v.get("certificate").unwrap_or_else(|| unwrap_data(v));
    let voters = c
        .get("votes")
        .and_then(Value::as_array)
        .map(|a| {
            let mut ids: Vec<&str> = a.iter().filter_map(|v| v.get("validator_id").and_then(Value::as_str)).collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.is_empty() { a.len() } else { ids.len() }
        })
        .unwrap_or(0);
    Some(Cert {
        height: c.get("height").and_then(Value::as_u64)?,
        certified_at_ms: c.get("certified_at_ms").and_then(Value::as_u64).unwrap_or(0),
        committee_size: c.get("committee_size").and_then(Value::as_u64).unwrap_or(0) as usize,
        voters,
        bft: c.get("bft").and_then(Value::as_bool).unwrap_or(false),
        gate: c.get("gate").and_then(Value::as_str).unwrap_or("unknown").to_string(),
    })
}

/// One block of `/v1/dagknight/recent`, reduced to what the gauge reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlockFact {
    pub height: u64,
    /// The producer exactly as served (a byte array or a string), as an opaque key.
    pub producer: String,
    pub merging: bool,
    pub red: bool,
}

pub(crate) fn parse_recent(v: &Value) -> Vec<BlockFact> {
    unwrap_data(v)
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| BlockFact {
                    height: b.get("height").and_then(Value::as_u64).unwrap_or(0),
                    producer: b.get("producer").map(Value::to_string).unwrap_or_default(),
                    merging: b.get("merge_parents").and_then(Value::as_array).is_some_and(|m| !m.is_empty()),
                    red: b.get("is_blue").and_then(Value::as_bool) == Some(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rejects that happened between two samples, by kind. Both samples are LIFETIME counters, so
/// only their difference says anything about the window.
pub(crate) fn rejects_delta(a: &HashMap<String, u64>, b: &HashMap<String, u64>) -> HashMap<String, u64> {
    b.iter()
        .filter_map(|(k, &vb)| {
            let d = vb.saturating_sub(a.get(k).copied().unwrap_or(0));
            (d > 0).then(|| (k.clone(), d))
        })
        .collect()
}

/// Turns the blocks of the window and two counter samples into the gauge's inputs.
pub(crate) fn inputs_from(a: &Counters, b: &Counters, recent: &[BlockFact], window_secs: f64, cert: Option<Cert>, now_ms: u64) -> Inputs {
    // Blocks produced INSIDE the window; on a quiet window, the most recent ones.
    let in_window: Vec<&BlockFact> = recent.iter().filter(|bk| bk.height > a.height).collect();
    let window: Vec<&BlockFact> = if in_window.len() >= 2 { in_window } else { recent.iter().collect() };
    let mut by_producer: HashMap<&str, u64> = HashMap::new();
    for bk in &window {
        *by_producer.entry(bk.producer.as_str()).or_insert(0) += 1;
    }
    let cert_wall_age_secs =
        cert.as_ref().filter(|c| c.certified_at_ms > 0).map(|c| now_ms.saturating_sub(c.certified_at_ms) as f64 / 1000.0);
    Inputs {
        blocks: window.len(),
        merging_blocks: window.iter().filter(|bk| bk.merging).count(),
        red_blocks: window.iter().filter(|bk| bk.red).count(),
        accepted_delta: b.shares_accepted.saturating_sub(a.shares_accepted),
        rejects_delta: rejects_delta(&a.rejects_by_kind, &b.rejects_by_kind),
        peer_heights: b.peer_heights.clone(),
        state_root_verdicts: b.state_root_verdicts.clone(),
        local_height: b.settled_height.unwrap_or(b.height),
        producer_counts: by_producer.into_values().collect(),
        blocks_added: b.height.saturating_sub(a.height),
        window_secs,
        peers_start: a.peers,
        peers: b.peers,
        tip: b.height,
        cert,
        cert_wall_age_secs,
    }
}

// ── sampling the node ───────────────────────────────────────────────────────────────────

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
/// `http://127.0.0.1:18181/v1/status`, …) but the gauge needs sibling endpoints, so the path
/// has to come off. Trimming a known suffix, rather than assuming a shape, keeps a custom
/// `--api` working instead of silently sampling the wrong origin.
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

/// Pull one `f64` out of a flat JSON object without a serde model (used by the mining card).
pub(crate) fn num(body: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let i = body.find(&pat)? + pat.len();
    let rest = &body[i..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == '+'))
        .unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// One shared blocking client. Rebuilding it per request costs ~25 ms because the rustls
/// builder reloads the bundled webpki root store every time (see `serve.rs`).
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

fn fetch(base: &str, path: &str) -> Option<Value> {
    let body = client().get(format!("{base}{path}")).send().ok()?.text().ok()?;
    serde_json::from_str(&body).ok()
}

fn counters(base: &str) -> Option<Counters> {
    let mut c = Counters::default();
    parse_miners(&fetch(base, "/v1/mining/miners")?, &mut c);
    parse_topology(&fetch(base, "/v1/network/topology")?, &mut c);
    Some(c)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Two counter samples `window` apart, then the certificate and the recent blocks.
/// The window is the WALL CLOCK between the samples, not the nominal sleep.
pub(crate) fn measure(api_base: &str, window: Duration) -> Option<KReading> {
    let t0 = Instant::now();
    let a = counters(api_base)?;
    std::thread::sleep(window);
    let b = counters(api_base)?;
    let secs = t0.elapsed().as_secs_f64().max(0.001);
    let cert = fetch(api_base, "/v1/finality/certificate").and_then(|v| parse_cert(&v));
    let recent = fetch(api_base, "/v1/dagknight/recent").map(|v| parse_recent(&v)).unwrap_or_default();
    Some(gauge(&inputs_from(&a, &b, &recent, secs, cert, now_ms())))
}

/// Start the background sampler. Idempotent: a second call is a no-op, so wiring it into more
/// than one entry point cannot produce two threads fighting over the same cell.
pub(crate) fn spawn(api_base: String) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("kgauge".into())
        .spawn(move || loop {
            cell().lock().unwrap_or_else(|e| e.into_inner()).1 = true;
            let r = measure(&api_base, WINDOW);
            {
                let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
                g.1 = false;
                if r.is_some() {
                    g.0 = r;
                }
            }
            std::thread::sleep(PAUSE);
        })
        .ok();
}

// ── the card ────────────────────────────────────────────────────────────────────────────

fn dim<T: Into<String>>(s: T) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(C_DIM))
}

fn val<T: Into<String>>(s: T) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(C_CYAN))
}

fn regime_colour(regime: &str) -> ratatui::style::Color {
    match regime {
        "critical" => C_RED,
        "elevated" | "blind" => C_GOLD,
        "stable" => C_GREEN,
        _ => C_DIM,
    }
}

fn channel_value(c: &Channel) -> String {
    c.value.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into())
}

/// The card's seven lines. Kept to ≤ 52 columns so they fit the half-width left column
/// without being cut (the old card's last line was).
pub(crate) fn card_lines(r: &KReading) -> Vec<Line<'static>> {
    let col = regime_colour(r.regime);
    let wit = &r.witness;
    let [tip, state, fin, conf] = &r.channels;
    let headline_note = if r.regime == "blind" {
        format!("  W {:.2}: {}", wit.w, wit.weakest)
    } else {
        format!("  W {:.2}", wit.w)
    };
    let cert = match wit.cert_lag_blocks {
        Some(l) if l >= 10_000 => format!("cert +{}k blk", l / 1000),
        Some(l) => format!("cert +{l} blk"),
        None => "no cert".to_string(),
    };
    vec![
        Line::from(vec![
            dim("K_fix "),
            Span::styled(format!("{:.2}", r.k_fix), Style::default().fg(col).add_modifier(Modifier::BOLD)),
            dim("  "),
            Span::styled(r.regime.to_uppercase(), Style::default().fg(col).add_modifier(Modifier::BOLD)),
            dim(headline_note),
        ]),
        Line::from(vec![
            dim("ΔH_c "),
            val(format!("{:.3}", r.delta_h_c)),
            dim(" tip "),
            val(channel_value(tip)),
            dim(" state "),
            val(match state.value {
                Some(_) => format!("{}/{}", r.state_differ, r.state_compared),
                None => "—".into(),
            }),
            dim(" fin "),
            val(channel_value(fin)),
            dim(" conf "),
            val(channel_value(conf)),
        ]),
        Line::from(vec![
            dim("τ_d "),
            val(format!("{:.2}", r.tau_d)),
            dim(format!(" ({:.0} blk)", r.persist_blocks)),
            dim("  w_S "),
            val(format!("{:.2}", r.w_s)),
            dim(format!(" (N_eff {:.2} of {})", r.n_eff, r.distinct_producers)),
        ]),
        Line::from(vec![
            dim("W "),
            val(format!("{:.2}", wit.w)),
            dim(format!(
                "  {cert} · {}/{} signer · {} · bft {}",
                wit.voters,
                wit.committee,
                wit.gate,
                if wit.bft { "yes" } else { "no" }
            )),
        ]),
        Line::from(Span::styled(wit.verdict.clone(), Style::default().fg(C_VBRIGHT))),
        Line::from(dim(format!(
            "diag: rej Δ{} · churn {:.2} · {} peers · {:.1} blk/s",
            r.rejects_noise + r.rejects_semantic + r.rejects_unclassified,
            r.churn,
            r.peers,
            r.block_rate_bps
        ))),
        Line::from(dim(format!("unmeasured: P2P bytes · tip {}", crate::tabs_ui::group(r.tip)))),
    ]
}

/// The Node tab's K-gauge card: the gauge, its inputs, and what it can and cannot see.
pub(crate) fn render_kgauge_card(_app: &crate::App) -> Paragraph<'static> {
    let (reading, busy) = latest();
    let lines = match reading {
        Some(r) => card_lines(&r),
        None => vec![
            Line::from(vec![dim("K_fix "), dim("—")]),
            Line::from(dim(if busy { "measuring: two samples 6 s apart…" } else { "no reading yet" })),
            Line::from(dim("K_fix = 2π·√(ΔH_c·τ_d·w_S)")),
            Line::from(dim("W = min(clock, cert, independence, channels)")),
        ],
    };
    Paragraph::new(lines).block(card_block(" Κ  KRISTENSEN K-GAUGE", C_VBRIGHT))
}

// ── `sigil-top kgauge`: one reading, in full, in the terminal ──────────────────────────

/// Every input with its weight and the counts behind it, so the reading can be checked by hand.
pub(crate) fn report_text(r: &KReading) -> String {
    let w = &r.witness;
    let mut s = String::new();
    s.push_str(&format!(
        "\n  K_fix {:.3}   regime {}   (K_fix alone: {})\n",
        r.k_fix,
        r.regime.to_uppercase(),
        r.regime_unwitnessed
    ));
    s.push_str(&format!("    = 2π·√(ΔH_c {:.4} · τ_d {:.3} · w_S {:.3})\n\n", r.delta_h_c, r.tau_d, r.w_s));
    s.push_str(&format!(
        "  ΔH_c {:.4}   how far the nodes' views are apart (unmeasured weight {:.0} %)\n",
        r.delta_h_c,
        r.missing_weight * 100.0
    ));
    for c in &r.channels {
        s.push_str(&format!("    {:<6} {:>5} × {:.2}   {}\n", c.name, channel_value(c), c.weight, c.evidence));
    }
    s.push_str(&format!(
        "\n  τ_d {:.3}   the disagreement has lasted ~{:.0} of {} blocks\n",
        r.tau_d, r.persist_blocks, FINAL_DEPTH as u64
    ));
    s.push_str(&format!(
        "  w_S {:.3}   N_eff {:.2} of {} producer(s) over {} blocks (H_norm {:.3})\n\n",
        r.w_s, r.n_eff, r.distinct_producers, r.blocks, r.h_norm
    ));
    s.push_str(&format!("  W {:.2}   could a disagreement be seen? weakest sense: {}\n", w.w, w.weakest));
    s.push_str(&format!(
        "    clock {:.2} · certificate {} · independence {:.2} · channels {:.2}\n",
        w.clock,
        w.certificate.map(|c| format!("{c:.2}")).unwrap_or_else(|| "none".into()),
        w.independence,
        w.channels
    ));
    s.push_str(&format!(
        "    {} signer(s) of a committee of {}, gate {}, bft {}, certificate {}\n",
        w.voters,
        w.committee,
        w.gate,
        if w.bft { "yes" } else { "no" },
        w.cert_lag_blocks.map(|l| format!("{l} blocks behind the tip")).unwrap_or_else(|| "absent".into())
    ));
    s.push_str(&format!("    → {}\n\n", w.verdict));
    s.push_str(&format!(
        "  diagnostics, never in ΔH_c: rejects in window {} noise · {} semantic · {} other, churn {:.3}, {} peers ({} converged, {} syncing), {:.2} blk/s, tip {}\n",
        r.rejects_noise,
        r.rejects_semantic,
        r.rejects_unclassified,
        r.churn,
        r.peers,
        r.converged_peers,
        r.syncing_peers,
        r.block_rate_bps,
        crate::tabs_ui::group(r.tip)
    ));
    s.push_str("  unmeasured: P2P byte asymmetry (sigil-api has no byte counters)\n\n");
    s
}

/// `sigil-top kgauge [URL]`: reads the node's /v1 routes twice, 6 s apart, prints one reading
/// and exits. Starts no node and no screen.
pub(crate) fn run_cli(args: &[String]) {
    let api = args.first().map(String::as_str).unwrap_or(crate::help::DEFAULT_API);
    let base = api_base_of(api);
    println!("  measuring {base}: two samples {} s apart…", WINDOW.as_secs());
    match measure(&base, WINDOW) {
        Some(r) => {
            print!("{}", report_text(&r));
            // The same seven lines the Node-tab card shows, as plain text.
            println!("  the Node-tab card shows:");
            for l in card_lines(&r) {
                let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                println!("    │ {t}");
            }
            println!();
        }
        None => {
            eprintln!("  ✗ {base} did not answer /v1/mining/miners and /v1/network/topology");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    /// One producer has zero entropy, and it must be +0.0: the old card printed "−0.0000".
    #[test]
    fn single_producer_entropy_is_positive_zero() {
        let h = shannon_bits(&[200]);
        assert_eq!(h, 0.0);
        assert!(h.is_sign_positive(), "−0.0 leaked into the entropy");
        assert!(k_fix_of(0.0, 0.0, h).is_sign_positive());
    }

    #[test]
    fn even_two_producer_split_is_one_bit() {
        assert!(close(shannon_bits(&[100, 100]), 1.0));
        assert_eq!(shannon_bits(&[]), 0.0);
    }

    /// K_fix's shape, from the explainer and the battle test.
    #[test]
    fn k_fix_values_from_the_battle_test() {
        // Nobody disagrees: 0, whatever the persistence and the key count.
        for h in [0.0, 0.5, 1.0] {
            assert_eq!(k_fix_of(0.0, 300.0, h), 0.0);
        }
        // A full state split (tip + state + conflict), just started, ONE key: critical at once.
        let split = W_TIP + W_STATE + W_CONFLICT;
        assert!(close(k_fix_of(split, 0.0, 0.0), 3.974));
        assert_eq!(regime_of(k_fix_of(split, 0.0, 0.0)), "critical");
        // Rotating keys lowers the same reading by at most 1 − √¾ = 13.4 % and never raises it.
        let lever = 1.0 - k_fix_of(1.0, 0.0, 1.0) / k_fix_of(1.0, 0.0, 0.0);
        assert!(close(lever, 1.0 - 0.75f64.sqrt()));
        // Top of the scale: total disagreement that outlived finality, one key.
        assert!(close(k_fix_of(1.0, FINAL_DEPTH, 0.0), 2.0 * std::f64::consts::PI));
        // One node in five differs on balances for 100 blocks, four producers sharing evenly.
        assert!(close(k_fix_of(W_STATE * 0.2, 100.0, 1.0), 1.113));
        assert_eq!(regime_of(k_fix_of(W_STATE * 0.2, 100.0, 1.0)), "elevated");
    }

    #[test]
    fn regime_ladder_is_one_and_three() {
        assert_eq!(regime_of(0.999), "stable");
        assert_eq!(regime_of(1.0), "elevated");
        assert_eq!(regime_of(2.999), "elevated");
        assert_eq!(regime_of(3.0), "critical");
        assert_eq!(regime_of(f64::NAN), "unknown");
    }

    fn solo_cert(height: u64) -> Cert {
        Cert { height, certified_at_ms: 1, committee_size: 1, voters: 1, bft: false, gate: "solo".into() }
    }

    /// The g3 shape of 2026-10-26: a committee of one, solo gate, certificate at the tip.
    #[test]
    fn a_solo_committee_is_blind_not_stable() {
        let w = witness_of(true, Some(&solo_cert(100)), Some(0.0), Some(0), 64.0, 0.0);
        assert_eq!(w.w, 0.0);
        assert_eq!(w.weakest, "independence");
        assert!(w.blind());
        assert_eq!(regime_witnessed(0.76, &w), "blind");
        // An observed disagreement keeps its own severity, whatever W says.
        assert_eq!(regime_witnessed(3.5, &w), "critical");
        assert_eq!(regime_witnessed(1.5, &w), "elevated");
    }

    #[test]
    fn a_stopped_clock_and_a_missing_certificate_are_named() {
        let w = witness_of(false, Some(&solo_cert(100)), Some(0.0), Some(0), 64.0, 0.0);
        assert_eq!((w.w, w.weakest), (0.0, "clock"));
        assert!(w.verdict.starts_with("clock stopped"));
        let w = witness_of(true, None, None, None, 64.0, 0.0);
        assert_eq!(w.certificate, None);
        assert_eq!(w.weakest, "certificate");
        assert!(w.verdict.starts_with("no finality certificate"));
    }

    /// Three of four distinct BFT signers, a fresh certificate, every channel measured.
    #[test]
    fn a_real_committee_is_witnessed() {
        let cert = Cert { height: 100, certified_at_ms: 1, committee_size: 4, voters: 3, bft: true, gate: "grandpa".into() };
        let w = witness_of(true, Some(&cert), Some(5.0), Some(2), 64.0, 0.0);
        assert!(close(w.w, 0.75));
        assert!(!w.blind());
        assert_eq!(regime_witnessed(0.2, &w), "stable");
        assert!(w.verdict.starts_with("witnessed"));
    }

    #[test]
    fn certificate_freshness_ramp() {
        // τ = 64 s: fresh up to max(2τ, 120 s) = 128 s, worthless from 20τ = 1,280 s.
        assert_eq!(cert_freshness(Some(128.0), 64.0), Some(1.0));
        assert_eq!(cert_freshness(Some(1280.0), 64.0), Some(0.0));
        assert!(close(cert_freshness(Some(704.0), 64.0).unwrap(), 0.5));
        assert_eq!(cert_freshness(None, 64.0), None);
    }

    #[test]
    fn rejects_are_classified_noise_or_semantic() {
        assert_eq!(reject_class("stale_height"), "noise");
        assert_eq!(reject_class("duplicate"), "noise");
        assert_eq!(reject_class("verify_mismatch"), "semantic");
        assert_eq!(reject_class("something_new"), "unclassified");
    }

    /// The node's real shape, with two reject kinds: both are read (the old parser stopped at
    /// the first `]` and saw only the first kind).
    #[test]
    fn every_reject_kind_is_parsed() {
        let v: Value = serde_json::from_str(
            r#"{"height":4607241,"shares_accepted":118174,"rejects":[["stale_height",36],["verify_mismatch",2]]}"#,
        )
        .unwrap();
        let mut c = Counters::default();
        parse_miners(&v, &mut c);
        assert_eq!(c.height, 4_607_241);
        assert_eq!(c.rejects_by_kind.get("stale_height"), Some(&36));
        assert_eq!(c.rejects_by_kind.get("verify_mismatch"), Some(&2));
    }

    /// The 0.923 of 2026-10-26: 36 lifetime stale_height rejects in BOTH samples and 3 shares
    /// accepted in between. Rejects in the window: none. Disagreement from it: none.
    #[test]
    fn lifetime_rejects_are_not_a_rate() {
        let mut a = Counters { height: 100, shares_accepted: 118_172, ..Default::default() };
        a.rejects_by_kind.insert("stale_height".into(), 36);
        let mut b = a.clone();
        b.height = 150;
        b.shares_accepted = 118_175;
        assert!(rejects_delta(&a.rejects_by_kind, &b.rejects_by_kind).is_empty());
        let i = inputs_from(&a, &b, &[], 6.0, None, 0);
        let r = gauge(&i);
        assert_eq!(r.rejects_noise + r.rejects_semantic + r.rejects_unclassified, 0);
        assert_eq!(r.channels[3].value, Some(0.0), "conflict channel must not see lifetime rejects");
        // A stale_height reject INSIDE the window is a diagnostic, never disagreement.
        b.rejects_by_kind.insert("stale_height".into(), 40);
        let r = gauge(&inputs_from(&a, &b, &[], 6.0, None, 0));
        assert_eq!(r.rejects_noise, 4);
        assert_eq!(r.channels[3].value, Some(0.0));
    }

    #[test]
    fn topology_cert_and_recent_parse_from_the_node_shape() {
        let topo: Value = serde_json::from_str(
            r#"{"peer_count":3,"local_view":{"height":1000},
                "peer_heights":{"a":1010,"b":990,"c":187852,"last":5},
                "peer_views":{"a":{"state_root_matches_local":true},"b":{"state_root_matches_local":false},"c":{"state_root_matches_local":null}}}"#,
        )
        .unwrap();
        let mut c = Counters::default();
        parse_topology(&topo, &mut c);
        assert_eq!(c.peers, 3);
        assert_eq!(c.settled_height, Some(1000));
        let mut hs = c.peer_heights.clone();
        hs.sort_unstable();
        assert_eq!(hs, vec![990, 1010, 187_852], "\"last\" is not a peer");
        let mut vs = c.state_root_verdicts.clone();
        vs.sort_unstable();
        assert_eq!(vs, vec![false, true], "an uncompared view (null) is unknown, not a verdict");

        let cert: Value = serde_json::from_str(
            r#"{"height":4617542,"certified_at_ms":1792996671973,"quorum":1,"committee_size":1,"bft":false,"gate":"solo",
                "votes":[{"validator_id":"6d51","signature":"x"},{"validator_id":"6d51","signature":"y"}]}"#,
        )
        .unwrap();
        let cert = parse_cert(&cert).unwrap();
        assert_eq!((cert.height, cert.voters, cert.committee_size, cert.bft), (4_617_542, 1, 1, false));
        assert_eq!(cert.gate, "solo");

        let recent: Value = serde_json::from_str(
            r#"{"blocks":[{"height":5,"producer":[1,2,3],"merge_parents":[],"is_blue":true},
                          {"height":6,"producer":[1,2,3],"merge_parents":["ab"],"is_blue":false},
                          {"height":7,"producer":"epsilon","merge_parents":[],"is_blue":true}]}"#,
        )
        .unwrap();
        let blocks = parse_recent(&recent);
        assert_eq!(blocks.len(), 3);
        assert!(blocks[1].merging && blocks[1].red);
        assert_eq!(blocks[0].producer, blocks[1].producer);
        assert_ne!(blocks[0].producer, blocks[2].producer);
    }

    /// The reading the MCP tool took on live g3 at 2026-10-26 ~07:30 CET, rebuilt from its
    /// inputs: 9 converged peers (state roots all agree, mean spine gap 67 blocks), one still
    /// syncing, a straight chain, 199 of 200 blocks from one producer, a solo certificate at the
    /// tip. K_fix ≈ 0.76, stable on its own, BLIND once the witness speaks.
    #[test]
    fn the_live_g3_reading_of_2026_10_26() {
        let local = 4_607_841u64;
        let mut peer_heights: Vec<u64> = vec![local + 67; 9];
        peer_heights.push(187_852);
        let i = Inputs {
            blocks: 200,
            producer_counts: vec![199, 1],
            peer_heights,
            state_root_verdicts: vec![true; 9],
            local_height: local,
            blocks_added: 240,
            window_secs: 30.0,
            peers_start: 10,
            peers: 10,
            tip: 4_607_855,
            cert: Some(solo_cert(4_607_855)),
            ..Default::default()
        };
        let r = gauge(&i);
        assert!(close(r.delta_h_c, W_FINALITY * 67.0 / FINAL_DEPTH));
        assert!(close(r.k_fix, 0.760), "k_fix {}", r.k_fix);
        assert_eq!(r.regime_unwitnessed, "stable");
        assert_eq!(r.regime, "blind");
        assert_eq!((r.converged_peers, r.syncing_peers), (9, 1));
        assert_eq!(r.witness.cert_lag_blocks, Some(0));
        assert!(close(r.n_eff, 1.032));
    }

    /// A one-producer FORK must read critical. This is the case the old product form read 0.
    #[test]
    fn a_one_key_fork_reads_critical_not_stable() {
        let i = Inputs {
            blocks: 200,
            merging_blocks: 200,
            red_blocks: 0,
            producer_counts: vec![200],
            peer_heights: vec![1000; 4],
            state_root_verdicts: vec![false; 4],
            local_height: 1000,
            blocks_added: 50,
            window_secs: 6.0,
            peers: 4,
            peers_start: 4,
            tip: 1000,
            cert: Some(solo_cert(1000)),
            ..Default::default()
        };
        let r = gauge(&i);
        assert_eq!(r.h_norm, 0.0);
        assert!(r.k_fix >= 3.0, "k_fix {}", r.k_fix);
        assert_eq!(r.regime, "critical", "a blind witness must not hide an observed fork");
    }

    /// The card must fit the half-width column (the old card's last line was cut off).
    #[test]
    fn card_lines_fit_the_column() {
        let local = 4_607_841u64;
        let i = Inputs {
            blocks: 200,
            producer_counts: vec![199, 1],
            peer_heights: vec![local + 67; 9],
            state_root_verdicts: vec![true; 9],
            local_height: local,
            blocks_added: 240,
            window_secs: 30.0,
            peers: 10,
            peers_start: 10,
            tip: 4_607_855,
            cert: Some(solo_cert(4_607_855)),
            ..Default::default()
        };
        let lines = card_lines(&gauge(&i));
        assert_eq!(lines.len(), 7, "the card slot holds 7 lines inside its border");
        for l in &lines {
            assert!(l.width() <= 52, "{} columns: {:?}", l.width(), l);
        }
        let text: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
        assert!(text.contains("BLIND") && text.contains("4,607,855") && !text.contains("-0.00"));
        assert!(report_text(&gauge(&i)).contains("one writer attesting to its own chain"));
    }

    #[test]
    fn api_base_strips_whatever_status_path_was_configured() {
        assert_eq!(api_base_of("https://sigilgraph.org/api/v1/status"), "https://sigilgraph.org");
        assert_eq!(api_base_of("http://127.0.0.1:18181/v1/status"), "http://127.0.0.1:18181");
        assert_eq!(api_base_of("http://127.0.0.1:18181/v1/status/"), "http://127.0.0.1:18181");
        assert_eq!(api_base_of("http://127.0.0.1:18181"), "http://127.0.0.1:18181");
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
}
