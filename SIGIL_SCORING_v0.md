# SIGIL Scoring v0 — SAP + X-Algo + K-parameter

**Author:** rocky-sigil (Claude Opus 4.7, Epsilon)
**Date:** 2026-05-29
**Crate:** `sigil-scoring` at `sigil/crates/sigil-scoring/` (25/25 tests pass)
**Companions:** [`SIGIL_GENESIS_v0.md`](./SIGIL_GENESIS_v0.md) §4 (events), §8 (consensus), §9 (DEX); [`FLUX_DB_AUDIT_v0.md`](./FLUX_DB_AUDIT_v0.md) (event indexing)

## 0. Why three axes

Two existing scoring rails in the Flux ecosystem:

- **SAP** (Score-Adjusted Priority, `flux-p2p::sap`) — 5 dims on validator/peer behaviour: contribution rate, latency, stake, accuracy, uptime
- **X-Algo** (Cross-Algorithm, `flux-p2p::x_algo`) — 5 cross-dim signals: temporal trust, consensus alignment, tx quality, topology rank, economic efficiency

Both were hardcoded to `PeerId`. SIGIL needs them for validators AND pools, wallets, LP positions — for the DEX swap score the user explicitly asked for.

A third axis lands today: **K-parameter**, ported from the q-narwhalknight `void-walker::k_parameter` physics engine (correlation + drift_rate + oscillation_freq + phase_coherence + Gaussian noise + sliding history). In Quillon it was driven by EEG amplitude and human intent; in SIGIL it's driven by chain network activity and `SigilEvent` kind tags.

Default composite weighting: **0.5 · SAP + 0.3 · X-Algo + 0.2 · K-correlation**, tunable.

## 1. Crate structure

```
sigil/crates/sigil-scoring/
├── Cargo.toml              # deps: serde, blake3, rand, rand_distr, parking_lot
├── src/lib.rs              # exports + composite_score + SigilSubject + weights
├── src/subject.rs          # SubjectScoreTable<K> (SAP) + XAlgoTable<K>
├── src/kparam.rs           # KParameterEngine (SIGIL-flavored)
└── src/dex.rs              # PoolHealthScore, SwapQualityScore, LpReputationScore,
                            # score_pool_health, score_lp_reputation, score_swap
```

LOC: ~1,100. All math pure (no platform deps beyond serde+blake3+rand).

## 2. Public API surface

```rust
// lib.rs
pub enum SigilSubject { Validator, Pool, Wallet, LpPosition }
pub struct CompositeWeights { sap, xalgo, kparam }
pub fn composite_score(sap, xalgo, kparam) -> f64
pub fn composite_score_weighted(sap, xalgo, kparam, w) -> f64

// subject.rs — SAP
pub struct SubjectScoreTable<K>
pub struct SubjectScore<K> { subject, total, components, updated_at_ms, rounds_participated }
pub struct SapComponents { contribution, latency, stake, accuracy, uptime }
pub struct SapWeights { ... }   // default 0.30/0.25/0.20/0.15/0.10
impl<K> SubjectScoreTable<K> {
    pub fn new() / with_weights() / get / get_full / update / set_ema_alpha
    record_participation / update_latency / update_stake / mark_equivocation / top / worst
}

// subject.rs — X-Algo
pub struct XAlgoTable<K>
pub struct XAlgoScore<K> { subject, total, dimensions, sap_correlation, computed_at_ms }
pub struct XAlgoDimensions { temporal_trust, consensus_align, tx_quality, topology_rank, econ_efficiency }
pub struct XAlgoWeights { ... }   // default 0.30/0.25/0.20/0.15/0.10
impl<K> XAlgoTable<K> {
    pub fn new() / with_window / get / record_round / update_topology / update_econ / top / correlate_with_sap
}

// kparam.rs
pub struct KParameterEngine
pub struct KParameterState { correlation, drift_rate, oscillation_freq, quantum_noise, phase_coherence, timestamp_ms }
pub struct KStabilityReport { mean_correlation, std_deviation, trend, stability_score, sample_count }
impl KParameterEngine {
    pub fn new(baseline_k) / with_noise_amplitude / with_max_history
    update_event(activity_amp, event_kind) -> KParameterState
    current_correlation / normalised_correlation / phase_coherence / history / stability
}

// dex.rs
pub struct PoolHealthScore { total, liquidity, volume_velocity, price_stability, uptime }
pub struct SwapQualityScore { total, effective_price, pool_health, counterparty_trust, k_correlation }
pub struct LpReputationScore { total, fees_per_deploy, tenure, stability }
pub struct SwapInput<PoolId, AccountId> { pool, sender, reference_price, effective_price, slippage_bps }
pub fn score_pool_health(pool, sap_table) -> PoolHealthScore
pub fn score_lp_reputation(lp, sap_table) -> LpReputationScore
pub fn score_swap(swap, pool_sap, wallet_sap, wallet_xalgo, kparam, weights?) -> SwapQualityScore
```

## 3. SAP dimensions reinterpreted per subject

| SAP dim | Validator (default) | Pool | Wallet | LP position |
|---|---|---|---|---|
| `contribution` | vertices produced / round | trade volume rate vs cohort | tx submission rate | fee-share contribution |
| `latency` | response p50 (ms) | time-to-fill | time-to-confirm | (unused → 0) |
| `stake` | QUG staked (vs max) | liquidity depth (vs largest pool) | balance / circulating | shares / pool-total |
| `accuracy` | 1.0 minus equivocation | IL / price-tracking proxy | tx success rate | no-rugpull behaviour |
| `uptime` | rounds participated / total | active-trade epochs | active epochs | epochs in pool |

The same `SubjectScoreTable<K>` keeps the same component math; *only the indexer's choice of inputs* changes per subject. A pool indexer updates `liquidity` and `volume_velocity` each block; a wallet indexer updates `balance` and `tx success rate`.

## 4. K-parameter input re-interpretation

| q-narwhalknight void-walker | SIGIL sigil-scoring |
|---|---|
| `eeg_amplitude: f64` (mV) | `network_activity_amplitude: f64` (txs/s, ~50 normalisation) |
| `intent: &str` (free string) | `event_kind: &str` (SigilEvent variant name, e.g. "SwapExecuted") |
| Attosecond clock | Millisecond clock |
| `ATTOSECOND = 1e-18` | `MILLI_TICK = 1e-3` |
| `PLANCK` × 1e6 noise scale | `DEFAULT_NOISE_AMPLITUDE = 1e-9` (tunable via `with_noise_amplitude`) |

Math preserved verbatim: `correlation = baseline + activity_influence + kind_influence + oscillation + noise`; `drift_rate = Δcorrelation / Δt`; `phase_coherence` accumulates with activity influence.

## 5. Composite formula

```
composite = 0.5 · SAP + 0.3 · X-Algo + 0.2 · K-correlation
```

For a swap, the three axes are populated as:

- **SAP-axis** ← average of `pool_health.total` and `counterparty_trust = 0.6·sender_sap + 0.4·sender_xalgo`
- **X-Algo-axis** ← `effective_price = 1 - slippage_bps/10000` (execution quality is cross-algorithmic — price-vs-reference)
- **K-axis** ← `kparam.normalised_correlation()` snapshot at swap time

This keeps the composite consistent with the user-facing meaning of each axis: SAP for "how trustworthy is this participant", X-Algo for "did this execution behave well across multiple criteria", K-param for "is the chain in a stable correlation regime right now".

## 6. Integration into `flux-dex` (when ported)

The DEX port (Track D, queued — `q-dex` verbatim port per genesis §9) should:

1. Maintain `SubjectScoreTable<PoolId>` updated per-block by the pool indexer (input: trade events, reserves, fee accrual)
2. Maintain `SubjectScoreTable<WalletId>` + `XAlgoTable<WalletId>` updated per-block by the account indexer (input: tx outcomes, gas spent)
3. Hold one `KParameterEngine` chain-wide, updated on every committed event
4. On every committed `SigilEvent::SwapExecuted`, call `score_swap(...)` and attach the `SwapQualityScore` to the event payload before merkle-rooting (so the score becomes part of the inclusion proof — wallet UIs can render it verifiably)

Wire-format change to genesis §4 `SwapExecuted`:

```rust
SwapExecuted {
    pool: PoolId, in_token, in_amt, out_token, out_amt,
    slippage_bps: u16, fee_paid: u128,
    quality: SwapQualityScore,   // NEW
}
```

Backwards-compat: `SwapQualityScore::default()` if no scorer is wired yet; allows Phase 0 to ship without DEX.

## 7. Validator scoring path (consensus side)

`flux-dagknight` (port of `q-dag-knight`, Track A — rocky-59) should:

1. Hold a `SubjectScoreTable<ValidatorId>` updated per round (vertex production, latency, stake, equivocation, uptime)
2. Hold an `XAlgoTable<ValidatorId>` updated per round (consensus alignment, tx-quality of included batches, economic efficiency)
3. Feed `KParameterEngine` from per-block `MintReward` events (activity = block reward / max-reward, kind = "MintReward")
4. Use `composite_score()` to derive the **gossipsub mesh priority** + **vertex inclusion ordering** in the DAG

This is the validator-side equivalent of what `flux-p2p::sap::ScoreTable` already does for peers — but now with a third axis.

## 8. Wallet UI integration

Browser/Slint wallets surfacing the swap score:

- Show `SwapQualityScore.total` as a 0–100 grade badge next to the swap result
- Hover/click expands into the four components (effective_price, pool_health, counterparty_trust, k_correlation)
- Color thresholds: >0.85 violet glow ("A-grade"), >0.65 white ("B"), >0.45 muted-yellow ("C"), <0.45 dim-red ("D")
- The chain commits to the score via the event log root (§4 of genesis) → score is verifiable, not advisory

## 9. Why this design vs in-place refactor of `flux-p2p`

Considered: refactor `flux-p2p::{sap, x_algo}` to make `PeerId` generic (`K: Hash+Eq+Clone`). Rejected for v0:

- PeerId is referenced 22+ times in flux-p2p::sap, 21+ in x_algo; changes ripple to `flux-p2p::dagknight`, `flux-search::ranking`, `fluxc-mcp` dashboards, `fluxc/serve.rs` SSE schema
- SIGIL's needs are different in *interpretation* not in *math* — wrapping is cleaner than punching through a shared crate's API
- A future consolidation (one canonical scoring core, both flux-p2p and sigil-scoring as adapters) is welcome but not on the critical path

`sigil-scoring` has zero `flux-p2p` dep — math is duplicated, not imported. ~250 LOC of duplicated math is small; the gain is independence from libp2p/gossipsub/network deps in a pure-compute crate that's path-deppable by any consumer including light clients + WASM verifiers.

## 10. Test coverage (25 tests, all green)

| Module | Tests | Covers |
|---|---|---|
| `lib::tests` (4) | composite weights default + clamps + basic mix | composite formula correctness |
| `subject::tests` (6) | SAP basic / top-N / equivocation; X-Algo history record / window cap / econ update / SAP correlation | both tables, both update paths |
| `kparam::tests` (7) | baseline / history growth / cap / consistent kind drift / clamp / stability report (insufficient + full) | engine math + history rules |
| `dex::tests` (8) | pool health basic + unknown-pool; swap quality at 0 + 50% slippage + explicit bps; LP reputation; unknown-sender resilience | end-to-end DEX scoring shapes |

Compile-verified by temp-copying into `flux/crates/sigil-scoring/` for `fluxc build` — canonical home stays at `sigil/crates/`. Per SIGIL rule 1 (genesis §13) release binaries must come from Delta; this verification is dev-side only.

## 11. Future work (queued, post-v0)

- **G_KP1** — wire `KParameterEngine` to a `flux-db` CF so its history survives node restart (currently in-memory only)
- **G_KP2** — STARK proof of `score_swap()` result so a light client can verify a swap's quality badge without trusting the producer
- **G_KP3** — `sigil-scoring` MCP tool surface: `sigil_score_pool`, `sigil_score_swap`, `sigil_score_lp`, `sigil_top_pools_by_quality`
- **G_KP4** — wallet-side ranking helpers: `best_pool_for_swap(in_token, out_token, amount)` chooses among multiple pools by predicted `SwapQualityScore`
- **G_KP5** — consolidation prototype: hoist `SubjectScoreTable<K>` into a `flux-scoring-core` crate that both `flux-p2p` and `sigil-scoring` adapt against

## 12. Hand-off

When the DEX port lands (Track D), this crate plugs in via `Cargo.toml` path-dep:

```toml
[dependencies]
sigil-scoring = { path = "../sigil-scoring" }
```

The DEX wires three indexers (pool / wallet / event-stream-to-K) and the score becomes part of every committed `SwapExecuted`. The wallet renders the badge. The chain commits to the score in the event-log root. Quality is provable, not advertised.

---
*Filed under rocky-sigil-64 (`sigil-scoring` scaffold + SAP + X-Algo + K-parameter + DEX scorers). Companion docs: `SIGIL_GENESIS_v0.md`, `FLUX_DB_AUDIT_v0.md`.*
