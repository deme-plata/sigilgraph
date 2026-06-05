# sigil-lite / sigil-top — NEXT PROTOTYPE: "LIGHT-2 — everything real"

> The light client (sigil-top TUI + sigil-lite headless) is at **v0.1.10**. The honesty arc this session kept finding placeholders and labelling them. **LIGHT-2's single theme: turn every remaining placeholder REAL.** When it ships, nothing the client shows is simulated — verify-don't-trust, completed.

## Where we are (v0.1.10) — already real
- ✅ tip-verify in ~10µs (real `sigil-tip-proof`), live latency sparkline
- ✅ live testnet **data** (height/peers/supply/block-stream) via the log-sidecar snapshot
- ✅ hot self-update (`[U]` → atomic re-exec in place), version cadence 0.1.0→0.1.10
- ✅ `[E]` export verification report · LIGHT-CLIENT SCORECARD (tips verified vs 0 bytes)
- ✅ real DoH DNS-anchor lookup (honestly reports "not published yet")
- ✅ no fake sync bar (`[F]` flips straight to the real dashboard)
- ✅ dual-mode TUI: lite (verify) → full node dashboard

## The honest placeholders that REMAIN (LIGHT-2 kills these)
| placeholder today | LIGHT-2 makes it real |
|---|---|
| verifies a **genesis-sample** tip (not the chain's real roots) | the node publishes its **real tip-proof** (4 live roots + sig) in `sigil-status.json`; client verifies the *actual* chain tip |
| eclipse **K** climbs on a timer (simulated) | client fetches the tip from **K independent sources** (N DoH resolvers + M peers) → **real** f^K, real security gauge |
| DNS anchor = "not published yet" | publish + auto-rotate the **`_sigil-tip` TXT** (aether/Namecheap-API) → client's DoH verifies it → **zero-infra** verification real |
| `[F]` full = instant dashboard, no download | once the node snapshots to aether, `[F]` **downloads the RS shards → reassembles → verifies every block** → real, variable progress |

## LIGHT-2 goals (make them all real)
1. **Real tip-proof end-to-end** — sidecar/node emits the live 4-root tip-proof; client verifies the real chain, not a sample. *(the keystone)*
2. **Multi-source eclipse** — fetch the tip from ≥3 independent DoH resolvers + ≥1 peer; K = distinct agreeing sources; the security gauge becomes a measured fact.
3. **DNS anchor LIVE** — a small publisher (node/sidecar + Namecheap API) writes `_sigil-tip` every N blocks; client's existing DoH path lights up green for real.
4. **Real full-sync via aether** — ride the `aether-load-on-boot` node build: client pulls the published snapshot shards, RS-reassembles, verifies block-by-block with honest variable throughput.
5. **flux-miner half** — `[M]` mine: the lightweight node optionally *participates* (verify-and-attest, the operator-fee path), not just observes.
6. **Knot verify** — `[K]` verify a `flux-knot` artifact (proof-of-inference) → the light client becomes the **universal verifier**: chain tips *and* compute.

## Build lanes (FLUXFOOD-composed, smallest first)
- **L2-A** sidecar/node → publish real tip-proof in `sigil-status.json` (reuse `sigil-tip-proof` serialize) — unblocks #1.
- **L2-B** client multi-resolver fetch (curl ×N DoH/peers) → real K — pure client, fast.
- **L2-C** `_sigil-tip` publisher (Namecheap API or a DNS-writable provider) → anchor live.
- **L2-D** aether snapshot serve + client download/reassemble (depends on the node `aether-load-on-boot` rebuild fix).
- **L2-E** `[M]` attest/mine + `[K]` knot-verify (depends on flux-miner + flux-knot).

## The throughline
v0.1.x proved the *shape* (a 572KB client that verifies + hot-updates). **LIGHT-2 makes the shape honest to the bone**: real roots, real K, real anchor, real sync — a light client where the only thing you trust is the math, and every number on screen is a fact it just checked.

— rocky, 2026-05-31 · next milestone after the v0.1.10 cadence.
