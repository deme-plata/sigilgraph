# SIGIL Prototype 6 — "Soak survives"

> **Status:** Draft scope (rocky, 2026-05-29 late). Locks the bridge from P5 to a beta candidate. Predecessors: P3 ✓ (state agreement), P4 ✓ (tip-verify join), P5 ✓ (DEX + bank).
> **One-line goal:** SIGIL runs *unsupervised* on Delta + Epsilon for 72 hours, processes real-shape traffic, auto-updates once across the network without a human, and emerges with the genesis bank installed, the master wallet collecting fees, and zero state-root divergences in the log.

---

## Why P6 (and not P7's DagKnight + VDF) is the right next step

Three reasons:

1. **P0-P5 shipped the pieces; nothing has yet proved they work together at runtime.** Every "tested ✓" line in the scope docs to date refers to unit tests + local smoke runs. The sigil-updater test is `keygen → publish → verify → apply` *all on one machine*. Two real machines, sharing a gossipsub topic, exchanging a release announcement, downloading + verifying + atomically swapping — that's still untested. P6 closes that.
2. **The DagKnight Professor whitepaper carries over to SIGIL almost verbatim** ([[project-sigil-chain]] §post-DagKnight analysis). The algorithm is proven on Quillon over millions of blocks. Porting it as a single-producer "good enough" prototype is *not* the high-risk move; running on real hardware unsupervised is. P7 ports `q-vdf` + `q-dag-knight`; P6 makes sure the floor we're standing on holds.
3. **Beta-readiness is "can the chain crash overnight and not corrupt data" — a property only observable in time, not in code review.** P6 buys that observation.

---

## Three pillars

### Pillar A — Auto-update validation over the wire (closes REL-1)

The current state (verified this session):
- Delta `:9501` is running an `/opt/orobit/shared/sigil-node` binary, md5 `22344c5…`, no systemd unit, started 2026-05-29 16:24 manually.
- That hash matches **none** of v0.0.1 / v0.0.2 / v0.0.3 release artifacts. Delta is a manual-SCP-era deploy from before the release scripts were finalised.
- `/sigil/g0/release` gossipsub topic exists in the wire constants but has never carried a real announcement between hosts.

**What ships in Pillar A:**

| Subtask | Owner | Scope |
|---|---|---|
| A-1 | open | Cut v0.0.4 — `scripts/release.sh` build + sign + emit `releases/v0.0.4/{binary,.proof,.announcement.json}`. Binary MUST include sigil-dex + sigil-bank crates (delta between v0.0.3 and v0.0.4 = today's chain work). |
| A-2 | open | `scripts/host-setup.sh delta` + `scripts/host-setup.sh epsilon` from scratch on both hosts. Generates the systemd unit + `/home/orobit/sigil-data/` skeleton + firewall rules. Document any drift from the runbook. |
| A-3 | open | `scripts/deploy.sh delta 0.0.4` + verify Delta drains topic, atomically swaps, restarts on new binary. Then same for Epsilon. Document md5 of the live binary on each host against the v0.0.4 release artifact. |
| A-4 | open | Stretch: `scripts/release.sh` v0.0.5 (a no-op bump — bump `version` field, rebuild, re-sign) and publish via the SIGIL `/sigil/g0/release` topic from Epsilon. Both Delta + Epsilon should auto-update from the gossipsub announcement *without* `scripts/deploy.sh` being invoked. THIS is the actual auto-update validation. |

A-4 is the high-value test. A-1/2/3 are scaffolding for it.

**Definition of done:**
- `ssh root@5.79.79.158 "md5sum /opt/orobit/shared/sigil-node"` matches the v0.0.5 release artifact md5.
- Same for Epsilon.
- Both reached the v0.0.5 state via the auto-update path, **not** manual SCP.
- The transition is documented in `releases/v0.0.5/auto-update-validation.md` with timestamps + topic-trace excerpts.

### Pillar B — 72-hour soak

Once Pillar A is green, leave both nodes running for 72 hours under simulated traffic. The chain doesn't yet have a VDF tick, so traffic is operator-driven:

| Subtask | Owner | Scope |
|---|---|---|
| B-1 | open | `scripts/soak-driver.sh` — runs on Beta (out-of-band), every 30s POSTs a randomly-shaped `produce-block` request to Delta's API. Mixes tx kinds: 60% Send, 20% Swap, 10% LpDeposit, 10% LpWithdraw. Uses a pool of 5 demo wallets pre-funded at genesis. |
| B-2 | open | Soak observability: ship to a flat-file log at `/home/orobit/sigil-data/soak.jsonl` — per-block `(height, hash, 4_roots, tx_count, wall_ms)` plus any error path. Beta tails + diffs across nodes every minute via `scripts/soak-diff.sh`. |
| B-3 | open | Halt-on-divergence: leverage the P3 state-root invariant. If Delta and Epsilon ever differ at the same height, both should `exit(78)` per the genesis spec. The soak script checks for exit code 78 in journal every minute and pages a Slack/webhook (or emails the master wallet — eat your own dogfood). |
| B-4 | open | Soak report: at hour 72, generate a `releases/v0.0.5/soak-72h-report.md` summarising: total blocks, total tx, exit count, divergence events, master-wallet balance accrual, swap count, LP count. PDF via `flux-report --period "Soak 2026-05-29 → 2026-06-01"`. |

**Definition of done:**
- 72 hours of continuous operation.
- Zero divergence events (or, if any, each documented + reproduced + understood).
- Master wallet's NATIVE balance > 0 (mining path doesn't exist yet, but if any of the 20% swap traffic credited it, the 5 bps skim should be visible).
- Soak report exists, PDF on Beta + Epsilon downloads.

### Pillar C — Genesis ceremony with bank installed

Closes P5-MW from the open task board. The bank exists as code; it has not yet been *installed* via a real genesis transition.

| Subtask | Owner | Scope |
|---|---|---|
| C-1 | open | `keys/sigil-master-genesis.{sk,pk}.hex` — SQIsign keypair generated at v0.0.5 cut time. The .sk.hex is locked away (eventually multisig; v0 just chmod 600). The .pk.hex derives the master_wallet address. |
| C-2 | open | `sigil-node mint-genesis` emits a transition that includes `StateMutation::SetMasterWallet { wallet: <derived from master pk> }` alongside the existing state seed. Verified: `sigil-node show-tip` then `sigil-node query --wallet master` shows master_wallet is Some after genesis. |
| C-3 | open | Reserved-nickname slate baked into block 0: emit `StateMutation::ReserveNickname { nickname }` for each of (postmaster, admin, abuse, noreply, support, rocky, codex, adrian, viktor). Requires the new variant — coordinate with EMAIL-E. If EMAIL-E hasn't landed, ship the variant + commit as part of C-3 itself. |
| C-4 | open | Genesis hash check: `mint-genesis` produces a deterministic block 0 hash. Pin it in `SIGIL_GENESIS_v0.md` §15 + add a `sigil-node verify-genesis` subcommand that recomputes + asserts. Halts if mismatched. |

**Definition of done:**
- v0.0.5 genesis block 0 hash is pinned in the spec, on-chain in the producer, and verifiable by any joining node.
- Genesis includes `SetMasterWallet` + reserved nicknames.
- `sigil-node verify-genesis` returns exit 0 on the canonical genesis, exit 78 on any tamper.

---

## Sequencing

```
C-1 ──┐
       ├──► C-2 ──► C-3 ──► C-4 ──┐
                                   ├──► A-1 (v0.0.4 with new genesis) ──► A-2 ──► A-3 ──► A-4 (auto-update v0.0.5)
                                   │                                                       │
                                   │                                                       ▼
                                   └──────────────────────────────────────────────────► B-1 (soak driver) ──► B-2/B-3 (72h) ──► B-4 (report)
```

Pillar C must land first (it changes the genesis hash). Pillar A consumes the new genesis. Pillar B consumes a deployed v0.0.5.

---

## What P6 deliberately does NOT include

- **DagKnight + VDF mining** — that's P7. Single-producer assumption stays; soak traffic comes from an out-of-band driver.
- **Browser wallet** — Phase 8 of the genesis roadmap. CLI-only soak is fine. (P7 picks this up.)
- **Inbound MX** — EMAIL-F/G/H. The master-wallet skim notification + soak-alert email are *outbound only*, which EMAIL-A already supports.
- **Public sigilgraph.com landing** — Phase 9 of the genesis roadmap. P6 is internal-soak, not external-discovery.
- **Multi-validator quorum** — the chain still trusts a single producer (Delta). Multi-validator is P7.
- **DEX governance + LP per-wallet ledger** — open Q3 from P5 scope. Defers.

---

## P7+ teaser

- **P7 — Beta polish**: parallel tracks (a) DagKnight + flux-vdf + flux-mining port, (b) browser wallet at `wallet.sigilgraph.com`, (c) nickname registry + EMAIL-B/C user-facing flows, (d) sigilgraph.com landing page.
- **P8 — Beta candidate cut**: invite a small external cohort (3-5 strangers) to install + run a node + hold a balance + swap. First real-user feedback loop.
- **P9 — Public beta**: open invite.

---

## Estimated effort

- Pillar A: 1-1.5 days (mostly waiting on systemd + network propagation + writing the auto-update verification)
- Pillar B: half-day setup, then 72h elapsed + half-day reporting. Most effort is observability discipline.
- Pillar C: 1 day (genesis ceremony + verify-genesis subcommand + reserved-nickname slate)

**Total: 3-4 active days + 72 elapsed hours for soak. Parallelizable across 2-3 agents (A and C are sequential; B is partly parallel once C is in).**

---

## Open questions for swarm before lock

1. **Soak traffic shape** — 60/20/10/10 mix proposed (Send/Swap/LpDeposit/LpWithdraw). Realistic? Or biased toward Send to mirror Quillon's actual mainnet traffic (which is >95% Send)? Vote: *Quillon-mirrored (95/3/1/1)* — better stress-tests the dominant path, swap/LP get covered as edge cases.
2. **Master genesis key custody** — single-key chmod 600 file on Beta vs 3-of-5 multisig from day one? 3-of-5 is the eventual target (Lock #15) but the multisig infrastructure isn't built yet. Vote: *single-key v0, document the migration path, ship multisig in P7+*.
3. **Reserved nicknames committed at block 0 vs runtime-enforced** — block 0 SetReservation transitions are visible + tamper-evident, runtime is cheaper. Vote: *block 0* — the audit trail matters more than the bytes.
4. **Divergence-halt during soak** — should both nodes halt on any single divergence (current spec) or only halt if divergence persists across N blocks? Single is the safer default; multi could mask real issues. Vote: *single* — paranoia is the point of the invariant.
5. **Should soak run on Alpha Docker too?** — gives us a third diverse-hardware data point. Marginal cost is low. Vote: *yes, opportunistic — if Alpha is free, add it as a third node; if it's busy with other workloads, skip*.

---

## Why this lands the chain in beta-survivable state

Every gap in the "Beta polish" P7 list (DagKnight, browser wallet, nickname UX, sigilgraph.com) is *additive* — adding them makes SIGIL more attractive. Every gap in P6 (auto-update across the network, genesis-with-bank, soak-tested unsupervised operation) is *load-bearing* — without them, even a beautiful SIGIL crashes on its first overnight.

P6 is the unsexy work that makes the sexy work safe to ship.

— rocky 🟠
