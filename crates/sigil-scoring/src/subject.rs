// Subject-generic SAP + X-Algo tables.
//
// Ports the math of flux-p2p::sap and flux-p2p::x_algo but parametrises the
// subject identity over K instead of hardcoding PeerId. The same dimensions
// are reusable for any scorable entity: validators, pools, wallets, LP IDs.
//
// Component update math is preserved verbatim:
//   - SAP: weighted sum of 5 dims, EMA smoothing on total, top/worst-N
//   - X-Algo: weighted sum of 5 dims, recompute on dim updates, top-N

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── SAP (5-dim) ───────────────────────────────────────────────────────────

/// SAP component breakdown. Each field is on [0, 1].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct SapComponents {
    pub contribution: f64,
    pub latency: f64,
    pub stake: f64,
    pub accuracy: f64,
    pub uptime: f64,
}

/// SAP component weights. Defaults: 0.30 / 0.25 / 0.20 / 0.15 / 0.10.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SapWeights {
    pub contribution: f64,
    pub latency: f64,
    pub stake: f64,
    pub accuracy: f64,
    pub uptime: f64,
}

impl Default for SapWeights {
    fn default() -> Self {
        SapWeights {
            contribution: 0.30,
            latency: 0.25,
            stake: 0.20,
            accuracy: 0.15,
            uptime: 0.10,
        }
    }
}

/// Per-subject SAP score record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubjectScore<K> {
    pub subject: K,
    pub total: f64,
    pub components: SapComponents,
    pub updated_at_ms: u64,
    pub rounds_participated: u64,
}

/// Generic SAP score table. K is the subject identity (validator id, pool id,
/// wallet id, …).
#[derive(Clone, Debug)]
pub struct SubjectScoreTable<K: Hash + Eq + Clone> {
    scores: HashMap<K, SubjectScore<K>>,
    weights: SapWeights,
    ema_alpha: f64,
    total_rounds: u64,
    total_stake: u64,
}

impl<K: Hash + Eq + Clone> Default for SubjectScoreTable<K> {
    fn default() -> Self { Self::new() }
}

impl<K: Hash + Eq + Clone> SubjectScoreTable<K> {
    pub fn new() -> Self {
        SubjectScoreTable {
            scores: HashMap::new(),
            weights: SapWeights::default(),
            ema_alpha: 0.3,
            total_rounds: 0,
            total_stake: 1,
        }
    }

    pub fn with_weights(weights: SapWeights) -> Self {
        let mut t = Self::new();
        t.weights = weights;
        t
    }

    pub fn len(&self) -> usize { self.scores.len() }
    pub fn is_empty(&self) -> bool { self.scores.is_empty() }

    /// Get the total composite SAP score for a subject.
    pub fn get(&self, subject: &K) -> Option<f64> {
        self.scores.get(subject).map(|s| s.total)
    }

    /// Get the full record including component breakdown.
    pub fn get_full(&self, subject: &K) -> Option<&SubjectScore<K>> {
        self.scores.get(subject)
    }

    /// Update (or insert) a subject's components. New total is EMA-smoothed
    /// with the previous total (70% new + 30% old at the default alpha=0.3).
    pub fn update(&mut self, subject: K, components: SapComponents) {
        let old_total = self.scores.get(&subject).map(|s| s.total).unwrap_or(0.0);
        let total = self.weighted_sum(&components);
        let smoothed = total * (1.0 - self.ema_alpha) + old_total * self.ema_alpha;
        let rounds = self.scores.get(&subject)
            .map(|s| s.rounds_participated)
            .unwrap_or(0);
        self.scores.insert(subject.clone(), SubjectScore {
            subject,
            total: smoothed.clamp(0.0, 1.0),
            components,
            updated_at_ms: now_ms(),
            rounds_participated: rounds,
        });
    }

    /// Adaptive smoothing parameter; 0 = no smoothing (use the new value as-is),
    /// 1 = no update (stuck at the old value).
    pub fn set_ema_alpha(&mut self, alpha: f64) {
        self.ema_alpha = alpha.clamp(0.0, 1.0);
    }

    /// Increment per-subject round counter.
    pub fn record_participation(&mut self, subject: &K) {
        if let Some(score) = self.scores.get_mut(subject) {
            score.rounds_participated += 1;
        }
        self.total_rounds += 1;
    }

    /// Exponential-decay latency score from p50 (matches flux-p2p semantics).
    /// e^(-p50_ms / 100) — 100ms gives ~0.37, 50ms gives ~0.61, 0ms gives 1.0.
    pub fn update_latency(&mut self, subject: &K, p50_ms: f64) {
        if let Some(score) = self.scores.get_mut(subject) {
            score.components.latency = (-p50_ms / 100.0).exp().clamp(0.0, 1.0);
        }
        self.recompute_total(subject);
    }

    /// Stake normalized as `stake / max(stake_across_all)`. Reaches 1.0 for
    /// the largest holder, decays linearly for smaller ones.
    pub fn update_stake(&mut self, subject: &K, stake_units: u64) {
        let max_stake = self.scores.values()
            .map(|s| (s.components.stake * self.total_stake as f64) as u64)
            .max()
            .unwrap_or(stake_units)
            .max(stake_units);
        self.total_stake = self.total_stake.max(1);
        if let Some(score) = self.scores.get_mut(subject) {
            score.components.stake = if stake_units == 0 {
                0.0
            } else {
                (stake_units as f64 / max_stake.max(1) as f64).clamp(0.0, 1.0)
            };
        }
        self.recompute_total(subject);
    }

    /// Force-zero a subject's accuracy (equivocation / known-bad). The total
    /// recomputes to reflect the drop.
    pub fn mark_equivocation(&mut self, subject: &K) {
        if let Some(score) = self.scores.get_mut(subject) {
            score.components.accuracy = 0.0;
        }
        self.recompute_total(subject);
    }

    /// Top N subjects by total. Returns owned clones (subjects need not be
    /// sortable themselves — we sort by f64 total).
    pub fn top(&self, n: usize) -> Vec<SubjectScore<K>> {
        let mut sorted: Vec<&SubjectScore<K>> = self.scores.values().collect();
        sorted.sort_by(|a, b| b.total.partial_cmp(&a.total).unwrap_or(std::cmp::Ordering::Equal));
        sorted.into_iter().take(n).cloned().collect()
    }

    /// Bottom N subjects by total — eviction / deprioritization helper.
    pub fn worst(&self, n: usize) -> Vec<SubjectScore<K>> {
        let mut sorted: Vec<&SubjectScore<K>> = self.scores.values().collect();
        sorted.sort_by(|a, b| a.total.partial_cmp(&b.total).unwrap_or(std::cmp::Ordering::Equal));
        sorted.into_iter().take(n).cloned().collect()
    }

    fn weighted_sum(&self, c: &SapComponents) -> f64 {
        let w = &self.weights;
        c.contribution * w.contribution
            + c.latency * w.latency
            + c.stake * w.stake
            + c.accuracy * w.accuracy
            + c.uptime * w.uptime
    }

    fn recompute_total(&mut self, subject: &K) {
        let components_opt = self.scores.get(subject).map(|s| s.components);
        if let Some(c) = components_opt {
            let total = self.weighted_sum(&c).clamp(0.0, 1.0);
            if let Some(score) = self.scores.get_mut(subject) {
                score.total = total;
            }
        }
    }
}

// ─── X-Algo (5-dim) ────────────────────────────────────────────────────────

/// Cross-algorithm dimension breakdown. Each field on [0, 1].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct XAlgoDimensions {
    pub temporal_trust: f64,
    pub consensus_align: f64,
    pub tx_quality: f64,
    pub topology_rank: f64,
    pub econ_efficiency: f64,
}

/// X-Algo dimension weights. Defaults: 0.30 / 0.25 / 0.20 / 0.15 / 0.10.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct XAlgoWeights {
    pub temporal: f64,
    pub consensus: f64,
    pub tx_quality: f64,
    pub topology: f64,
    pub econ: f64,
}

impl Default for XAlgoWeights {
    fn default() -> Self {
        XAlgoWeights {
            temporal: 0.30,
            consensus: 0.25,
            tx_quality: 0.20,
            topology: 0.15,
            econ: 0.10,
        }
    }
}

/// Per-subject X-Algo score record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XAlgoScore<K> {
    pub subject: K,
    pub total: f64,
    pub dimensions: XAlgoDimensions,
    pub sap_correlation: f64,
    pub computed_at_ms: u64,
}

/// Generic X-Algo table with round history for temporal-trust decay.
#[derive(Clone, Debug)]
pub struct XAlgoTable<K: Hash + Eq + Clone> {
    scores: HashMap<K, XAlgoScore<K>>,
    weights: XAlgoWeights,
    history_window: usize,
    /// (round_number, was_correct, tx_quality_at_time) per subject.
    history: HashMap<K, Vec<(u64, bool, f64)>>,
}

impl<K: Hash + Eq + Clone> Default for XAlgoTable<K> {
    fn default() -> Self { Self::new() }
}

impl<K: Hash + Eq + Clone> XAlgoTable<K> {
    pub fn new() -> Self {
        XAlgoTable {
            scores: HashMap::new(),
            weights: XAlgoWeights::default(),
            history_window: 200,
            history: HashMap::new(),
        }
    }

    pub fn with_window(history_window: usize) -> Self {
        XAlgoTable {
            scores: HashMap::new(),
            weights: XAlgoWeights::default(),
            history_window: history_window.max(50),
            history: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize { self.scores.len() }

    pub fn get(&self, subject: &K) -> Option<XAlgoScore<K>> {
        self.scores.get(subject).cloned()
    }

    /// Record a round outcome. `correct` = agreed with supermajority (for
    /// validators) or "operation was valid" (for pools/wallets/LP); `tx_quality`
    /// is a non-spam / well-formed fraction on [0,1].
    pub fn record_round(&mut self, subject: K, round: u64, correct: bool, tx_quality: f64) {
        let hist = self.history.entry(subject.clone()).or_default();
        hist.push((round, correct, tx_quality));
        if hist.len() > self.history_window * 2 {
            let cutoff = round.saturating_sub(self.history_window as u64);
            hist.retain(|(r, _, _)| *r >= cutoff);
        }
        self.recompute(&subject);
    }

    /// Bulk-update topology rank for many subjects at once (e.g. after a
    /// PageRank pass over the validator or pool-route graph).
    pub fn update_topology(&mut self, ranks: &HashMap<K, f64>) {
        for (subject, rank) in ranks {
            let rank = rank.clamp(0.0, 1.0);
            if let Some(score) = self.scores.get_mut(subject) {
                score.dimensions.topology_rank = rank;
            } else {
                self.scores.insert(subject.clone(), XAlgoScore {
                    subject: subject.clone(),
                    total: rank,
                    dimensions: XAlgoDimensions { topology_rank: rank, ..Default::default() },
                    sap_correlation: 0.0,
                    computed_at_ms: now_ms(),
                });
            }
        }
        let subjects: Vec<K> = self.scores.keys().cloned().collect();
        for s in &subjects { self.recompute_total(s); }
    }

    /// Economic efficiency = value_produced / cost_spent, clamped to [0,1].
    /// For validators: rewards earned vs gas/QUG spent. For pools: fee
    /// accrual vs IL. For LP: net realized vs deployed.
    pub fn update_econ(&mut self, subject: &K, value_produced: u64, cost_spent: u64) {
        if let Some(score) = self.scores.get_mut(subject) {
            score.dimensions.econ_efficiency = if cost_spent > 0 {
                (value_produced as f64 / cost_spent as f64).clamp(0.0, 1.0)
            } else if value_produced > 0 {
                1.0
            } else {
                0.0
            };
        }
        self.recompute_total(subject);
    }

    /// Top N by total.
    pub fn top(&self, n: usize) -> Vec<XAlgoScore<K>> {
        let mut sorted: Vec<&XAlgoScore<K>> = self.scores.values().collect();
        sorted.sort_by(|a, b| b.total.partial_cmp(&a.total).unwrap_or(std::cmp::Ordering::Equal));
        sorted.into_iter().take(n).cloned().collect()
    }

    fn recompute(&mut self, subject: &K) {
        let hist = self.history.get(subject).cloned().unwrap_or_default();
        if hist.is_empty() {
            return;
        }
        // Exponential-decay temporal trust: recent rounds weighted more.
        let total_w: f64 = (0..hist.len()).map(|i| (i as f64 / hist.len() as f64).exp()).sum();
        let mut temporal = 0.0;
        for (i, (_, ok, _)) in hist.iter().enumerate() {
            let w = (i as f64 / hist.len() as f64).exp();
            if *ok { temporal += w; }
        }
        temporal /= total_w.max(1e-9);

        let consensus = hist.iter().filter(|(_, ok, _)| *ok).count() as f64 / hist.len() as f64;
        let tx_quality = hist.iter().map(|(_, _, q)| *q).sum::<f64>() / hist.len() as f64;

        let entry = self.scores.entry(subject.clone()).or_insert(XAlgoScore {
            subject: subject.clone(),
            total: 0.0,
            dimensions: XAlgoDimensions::default(),
            sap_correlation: 0.0,
            computed_at_ms: now_ms(),
        });
        entry.dimensions.temporal_trust = temporal.clamp(0.0, 1.0);
        entry.dimensions.consensus_align = consensus.clamp(0.0, 1.0);
        entry.dimensions.tx_quality = tx_quality.clamp(0.0, 1.0);
        entry.computed_at_ms = now_ms();
        self.recompute_total(subject);
    }

    fn recompute_total(&mut self, subject: &K) {
        let w = self.weights;
        if let Some(score) = self.scores.get_mut(subject) {
            let d = score.dimensions;
            score.total = (d.temporal_trust * w.temporal
                + d.consensus_align * w.consensus
                + d.tx_quality * w.tx_quality
                + d.topology_rank * w.topology
                + d.econ_efficiency * w.econ).clamp(0.0, 1.0);
        }
    }

    /// Correlate X-Algo with an external SAP table over the same key type.
    /// Produces a simple multiplicative correlation field per subject.
    pub fn correlate_with_sap(&mut self, sap: &SubjectScoreTable<K>) {
        for (k, score) in self.scores.iter_mut() {
            if let Some(s) = sap.get(k) {
                score.sap_correlation = (s * score.total).clamp(0.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type PoolId = [u8; 8];

    #[test]
    fn sap_basic_pool_scoring() {
        let mut table: SubjectScoreTable<PoolId> = SubjectScoreTable::new();
        let pool: PoolId = *b"swap0001";
        table.update(pool, SapComponents {
            contribution: 0.9,
            latency: 0.8,
            stake: 0.7,
            accuracy: 1.0,
            uptime: 0.95,
        });
        // weighted = 0.9*0.30 + 0.8*0.25 + 0.7*0.20 + 1.0*0.15 + 0.95*0.10 = 0.855
        // EMA new entry: 0.855 * (1 - 0.3) + 0 * 0.3 = 0.5985
        let score = table.get(&pool).unwrap();
        assert!((score - 0.5985).abs() < 0.01, "expected ~0.5985 got {score}");
    }

    #[test]
    fn sap_top_returns_n_sorted() {
        let mut table: SubjectScoreTable<u32> = SubjectScoreTable::new();
        for i in 0u32..5 {
            table.update(i, SapComponents { contribution: i as f64 / 5.0, ..Default::default() });
        }
        let top = table.top(3);
        assert_eq!(top.len(), 3);
        assert!(top[0].total >= top[1].total);
        assert!(top[1].total >= top[2].total);
    }

    #[test]
    fn sap_mark_equivocation_zeros_accuracy() {
        let mut table: SubjectScoreTable<&'static str> = SubjectScoreTable::new();
        table.update("byzantine", SapComponents {
            accuracy: 1.0, ..Default::default()
        });
        let before = table.get(&"byzantine").unwrap();
        assert!(before > 0.0);
        table.mark_equivocation(&"byzantine");
        let after = table.get(&"byzantine").unwrap();
        assert!(after < before);
    }

    #[test]
    fn xalgo_records_history_and_scores() {
        let mut x: XAlgoTable<u32> = XAlgoTable::new();
        for round in 0u64..10 {
            x.record_round(42, round, true, 0.9);
        }
        let score = x.get(&42).unwrap();
        // All correct + high tx_quality → high temporal + consensus + quality
        assert!(score.dimensions.temporal_trust > 0.9);
        assert!(score.dimensions.consensus_align > 0.9);
        assert!(score.dimensions.tx_quality > 0.85);
    }

    #[test]
    fn xalgo_history_window_caps() {
        let mut x: XAlgoTable<u32> = XAlgoTable::with_window(50);
        for r in 0u64..200 {
            x.record_round(1, r, true, 1.0);
        }
        // history pruned to at most 2*window
        let hist = x.history.get(&1).unwrap();
        assert!(hist.len() <= 100, "history not pruned: {}", hist.len());
    }

    #[test]
    fn xalgo_econ_update() {
        let mut x: XAlgoTable<u32> = XAlgoTable::new();
        x.record_round(7, 0, true, 1.0); // bootstrap the entry
        x.update_econ(&7, 100, 50);
        let s = x.get(&7).unwrap();
        // value=100, cost=50 → 2.0 raw, clamped to 1.0
        assert!((s.dimensions.econ_efficiency - 1.0).abs() < 1e-9);
    }

    #[test]
    fn xalgo_correlate_with_sap() {
        let mut sap: SubjectScoreTable<u32> = SubjectScoreTable::new();
        sap.update(1, SapComponents { contribution: 1.0, ..Default::default() });
        let mut x: XAlgoTable<u32> = XAlgoTable::new();
        x.record_round(1, 0, true, 1.0);
        x.correlate_with_sap(&sap);
        let s = x.get(&1).unwrap();
        assert!(s.sap_correlation > 0.0);
    }
}
