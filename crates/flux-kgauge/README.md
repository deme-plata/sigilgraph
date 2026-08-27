# flux-kgauge

The DAG-Knight consensus observability gauge from *The Theoretical Minimum for
Blockchain Consensus* v4 (Kristensen, April 2026), as a pure Rust crate with no
chain dependencies.

Lifted from Quillon Graph's `q-api-server::k_parameter_gauge` and
`q-resonance::{k_parameter, k_effective}`, reworked so SIGIL — or any sibling
chain, or a simulator — can use it without dragging in `ndarray-linalg` /
OpenBLAS (1.7 GB) or a node's `AppState`.

```
serde only.  No async, no I/O, no chain types.  83 tests.
```

## What it computes

| | equation | what it answers |
|---|---|---|
| `K_base` | Eq. 10 | how stressed is this node's view of consensus |
| `Omega_node` | Eq. 17 | how much of the network can this node even see |
| `K_obs` | Eq. 18 | the stress reading, discounted by poor visibility |
| `d_commit` | Eq. 19 | how many blocks are built on the one I care about |
| `Lambda_commit` | Eq. 20 | how irreversible the current ordering is |
| `f_irrev` | Eq. 23 | what fraction of a window is past the reorg bound |
| `D_reorg` | Eq. 24 | `kappa * ceil(log2(1/eps))` = 360 at `kappa=18, eps=1e-6` |
| `K_enhanced` | Eq. 25 | the two corrections applied to the base gauge |
| observed `k` | — | DAG-Knight's anticone width, kept **separate** from `K` |

## The two things called "k"

They are different quantities, they move in opposite directions, and no formula
relates them. The crate reports both and never mixes them.

| symbol | is | small means |
|---|---|---|
| `k` / `kappa` (`anticone` module) | DAG-Knight anticone bound — how many blocks may be concurrent | network is fast, blocks nearly serial |
| `K` (`base`/`gauge` modules) | composite stress score, Eq. 10 | node is healthy |

## What this adds over a transcription

The paper's whole discipline is that a placeholder is never presented as a
measurement, and its Table 1 catalogs which is which. Here that catalog lives in
the type system:

- **`Provenance`** tags every value `Measured` / `Derived` / `Protocol` /
  `Placeholder` / `Unavailable`. A derived value is never more trustworthy than
  its worst input. `Protocol` counts as operational (`kappa = 18` is *known*);
  `Placeholder` does not.
- **A channel with no data reports `None`, never `0.0`.** "We never heard a peer
  height" and "we are perfectly in sync" must not produce the same number.
- **`Confidence`** separates `K = 0` because everything is healthy from `K = 0`
  because nothing happened.
- **`lambda_ceiling`** reports the largest `Lambda` the current setup could
  *ever* produce, so a `Lambda` pinned by window length is visible as such.
- **`f_irrev` returns `None`** when the window is shorter than `D_reorg`.
- **Every report carries `Caveat`s** with machine-stable codes.

## Three real defects found in the reference implementation

Each is reproduced by a test in this crate.

**1. `f_irrev` is structurally pinned to zero.** Eq. 23 counts blocks in a
window deeper than `D_reorg = 360`. The deepest block in a window of `L` blocks
has depth at most `L - 1`. A 60-second window on a 3.5 blocks/s chain holds
~210 blocks, so the count is zero *whatever the chain is doing*. The reference
publishes that zero as a measurement. Here it is `None` with a stated reason.
→ `commitment::tests::short_window_cannot_answer_f_irrev`

**2. `Lambda_commit` measures the window, not the chain.** Read literally,
Eq. 20 wants `d_commit(tip)`, which is zero by definition — nothing is built on
the tip. The reference substitutes "blocks added since the window opened", which
saturates at `1 - exp(-210/1800) ~ 0.11` and stays there, so `K_enhanced` sits
at ~9x `K_base` permanently on a healthy chain. `CommitmentBasis` makes the
reference block explicit and `lambda_is_window_limited()` flags the artefact.
→ `commitment::tests::window_base_lambda_is_pinned_by_window_length`

**3. Eq. 25 is inert when `K_base = 0`.** The enhancement is a *product*. A
quiet, in-sync node reads `K_base = 0` exactly, so no coverage collapse can move
`K_enhanced` off zero — the Sybil detection the enhancement exists for does not
fire in the very case it was designed for. The paper's Table 6 never exercises
this: every row starts from `K_base = 0.19` or `1.27`. The crate raises the
`enhancement_inert` caveat and tells you to read `Omega` directly.
→ `gauge::tests::enhancement_is_inert_when_the_base_gauge_reads_exactly_zero`

Two smaller ones: the reference still carries `hbar = 1.0` that v4 §13.2
explicitly removed (numerically a no-op), and it hardcodes the expected block
rate — a chain judged against another chain's normal reports stress that is
purely a calibration error. `GaugeConfig` carries a per-chain rate and raises
`target_rate_mismatch` when the observed rate is more than 5x off.
→ `gauge::tests::wrong_target_block_rate_is_called_out`

## Live reading, Epsilon, 2026-08-28

A 60-second window against `sigil-api` on `127.0.0.1:18181`:

```
K-gauge: K_base = 0.0000  K_enhanced = 0.0000  phase = stable (driven by Base)
  trust: confidence = Partial, worst provenance = derived (M*), actionable = true
  dH = 0.0000  ds = 0.0040  tau = 60.0s
  Omega = 0.5276 (6 peers / n_total 8)  Lambda = 0.0274 (d_commit 50)
  f_irrev = unavailable
  DAG: straight chain: 200 blocks, 0 merge-parents, 2 producer(s)
       — observed anticone k = 0
  7 caveats
```

What that says, and what it does not:

- **`K_base = 0` is real but weak.** `dH` is exactly zero — 271 shares
  submitted, 271 accepted, no peer churn — and the geometric mean makes the
  whole gauge zero regardless of `ds`. Two of five channels had no data at all
  (`sigil-api` exposes no P2P byte counters and no peer height), so the reading
  is a *lower bound* flagged `Partial`, not a clean bill of health.
- **The DAG-Knight `k` is 0.** Zero merge-parents across 200 blocks: the chain
  is running strictly serial. That is exactly what a small, low-latency fleet
  should produce, and it is what "low k" correctly refers to.
- **`Omega = 0.53` off a stated `n_total = 8`.** Against the reference's
  hardcoded `n_total = 50` the same six peers would read `Omega = 0.11` and
  inflate the gauge ~1.3x for no reason.
- **Two producers, and 199 of 200 blocks report `is_blue = false` while
  `blue_score` advances every block.** The chain's own colouring contradicts
  its own scoring. Anything derived from that colouring is unsafe until it is
  explained — this is the finding worth chasing, not the gauge value.
- The `sigil()` preset's block rate was corrected from 6.28 to **0.83
  blocks/s** by this run: 6.28 is a catch-up rate, not steady state. Before
  the fix the gauge reported `ds = 0.867` on a healthy chain; after, `0.004`.

## Using it

```rust
use flux_kgauge::{GaugeConfig, KGauge, NetworkSize, Observables, CounterSample};

let obs = Observables {
    previous: /* counters at window start */ CounterSample::default(),
    current:  /* counters at window end   */ CounterSample::default(),
    window_secs: 60.0,
    network_size: NetworkSize::Known(8), // say what you actually know
    ..Observables::default()
};

let mut gauge = KGauge::new(GaugeConfig::sigil());
let report = gauge.observe(&obs);
println!("{}", report.render());
report.prometheus("sigil_kgauge");
```

Against a live node:

```sh
./scripts/sigil-probe.sh 60 | cargo run --example from_stdin -- sigil
```

`sigil-probe.sh` is the chain-specific adapter and lives **outside** the crate
on purpose — the gauge must never learn what a SIGIL block looks like. It exits
non-zero when the reading is not safe to act on, so a cron wrapper can tell
"healthy" from "I could not tell".

## What it deliberately does not do

The Hamiltonian, effective temperature, phase diagram, and diffusion model from
Parts II and III are not implemented. They depend on `delta`, `f/n`, mesh degree
and anticone size — all hardcoded placeholders in every implementation the paper
surveys. The K-gauge is the part the paper itself calls "the only component
where all inputs are genuinely measured from live network state".

Nothing here is proven. The paper's one exact result, the Ground State Theorem
(`arg min H_DAG = PHANTOM ordering`), is about the Hamiltonian, which this crate
does not implement.

## Verify

```sh
fluxc check -p flux-kgauge --tests
fluxc build -p flux-kgauge --tests && ./target/debug/deps/flux_kgauge-<hash>
```

`fluxc test --package` is broken in this tree (valueless `--package`); run the
test binary directly. Never raw `cargo`.
