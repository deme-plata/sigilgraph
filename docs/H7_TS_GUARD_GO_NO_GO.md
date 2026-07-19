# H7 — Future-Timestamp Bound on Block Apply · Go/No-Go

**Branch:** `hardening/ws-2026-07-18` · **Status: implemented + unit-tested, NOT deployed.**
Prepared under the isolation-only window. This note is the operator decision gate before
H7 ever touches the live `sigil-g0` chain.

## What H7 is

LANE-R made emission **time-based**: a block's reward is the integral of the emission rate
over `[prev_ts, block_ts]`, and the follower APPLY path (`apply_block` in `sigil-rpcd`)
recomputes that reward from the peer's **stored timestamp verbatim** — with no bound
against the verifier's own clock. A producer stamping future timestamps pulls the emission
curve forward: still capped at 21M, but it steals the schedule (e.g. stamping +4 years
mints the entire first epoch's 10.5M SIGIL immediately). Not exploitable today — the chain
has one trusted producer — but it must be closed before any untrusted producer exists.

Backwards timestamps need no guard: `block_reward_time()` yields 0 reward for
`ts ≤ prev_ts`, so the past cannot mint.

## What landed (this branch, dormant)

- `sigil_emission::check_block_ts(height, block_ts_us, local_now_us)` — height-gated:
  below activation it is a no-op (every historical block still applies); at/above it
  rejects any block stamped more than `MAX_FUTURE_DRIFT_US` ahead of the verifier's clock.
- `MAX_FUTURE_DRIFT_US = 120 s` (in µs). Bitcoin tolerates 2 h; SIGIL producers are
  NTP-synced servers stamping µs, so 2 minutes is generous for real skew while capping a
  future-stamper's pull-forward to ~2 minutes of emission (~10 SIGIL at epoch 0).
- `ts_within_future_bound()` — the pure, ungated predicate, exposed so telemetry can
  observe real drift on the live chain **before** activation is ever scheduled.
- `H7_TS_GUARD_ACTIVATION_HEIGHT: u64 = u64::MAX` — **dormant.** Merging this code changes
  **nothing** on the live chain until an operator sets a real height.
- Wired at the ONE site that consumes a peer-stamped ts for money: `sigil-rpcd`
  `apply_block()`, immediately before the reward recompute. The producer path
  (`/mining/submit`) stamps its own `now_us()` and needs no guard.
- 4 unit tests green: dormant no-op with an absurd future stamp; active rejects
  bound+1 µs with exact excess; past/now/exact-bound all pass; predicate is ungated.

## What is NOT done (the "no-go until" list)

1. **Cross-node clock audit.** Before activation, run the ungated predicate as telemetry on
   every follower for ≥1 week and confirm observed peer-vs-local drift stays far inside the
   120 s bound (expected: <1 s on NTP-synced boxes). If real drift approaches the bound,
   fix NTP first — do not widen the bound.
2. **All-followers-first rollout.** The guard runs on the APPLY (follower) side. Every
   follower must run the H7 binary with a scheduled activation height **before** that
   height arrives; a mixed fleet where some followers enforce and some don't = fork risk
   at the first rejected block.
3. **Same-height coordination with H1.** If H1 (producer-sig) and H7 activate at different
   heights there are two separate consensus flag-days; schedule them at the SAME height to
   pay the coordination cost once.
4. **Alpha/Docker soak.** A 2-node mesh where one node deliberately stamps +10 min must
   show: block rejected, no fork, producer re-stamps and chain continues.

## Rollback

Below the activation height the code is a literal no-op — rollback before activation is
"do nothing." After activation, rollback = ship a binary with the const raised back to
`u64::MAX` to every follower (same all-followers-first discipline).
