# SIGIL pool reject histogram — measured, live `:8099`

**Measured by** `grogu-rocky-mining-b` (T5 lane), 2026-08-01.
**Method:** read-only HTTP sampler against the LIVE `sigil-rpcd` pool
(`GET /mining/miners`, 0.4 s interval). No code changed, no rig touched, nothing
deployed. RULE 0: this is the live path, not a bench.

| | |
|---|---|
| Samples | 246 |
| Wall span | 724.8 s (12 min 5 s) |
| Heights covered | 282,374 → 282,420 (46 blocks, ~15.8 s/block) |
| Node | `sigil-rpcd` v0.0.7, `sigil-g0`, pid 2106603 |
| Rigs seen | 4 distinct wallets |

---

## 1. The headline: the ">70% reject rate" premise is FALSIFIED

The charter (addendum 1) reported `10 shares / 25 rejects` and `9 shares / 47
rejects` and concluded a >70 % reject rate on current-version rigs. **That
comparison is not valid, and the true number is not close to it.**

Measured over the same window, all four rigs:

| | count |
|---|---|
| Accepted shares | **843** |
| Rejected submissions | **52** |
| **Accept rate** | **94.2 %** |
| Reject rate | 5.8 % (0.072 rejects/s pool-wide) |

The reject rate is a **real defect worth fixing** — it is not zero, it is
concentrated, and it does burn miner electricity. But it is ~6 %, not ~70 %, and
the fleet is not on fire.

### Why the original number was wrong — the instrument, not the mine

`GET /mining/miners` emits two per-wallet fields on **different clocks**
(`sigil-rpcd.rs:1642-1649`):

| field | source | lifetime |
|---|---|---|
| `shares` | `n.share_count` | **cleared every block** (`:1782`) |
| `rejects` | `n.reject_by_wallet` | **never cleared — lifetime since node boot** (`:1633`) |

So `shares / rejects` divides a **~16-second window** by **the node's entire
uptime**. Early in a node's life that ratio looks catastrophic and then decays
forever, regardless of what the miners do. The rpcd process had been up 8 min
when the charter sampled it.

To get the real rate above, accepts were reconstructed as *sum over heights of
the max `share_count` observed at that height* — i.e. sampling each per-block
window before it resets. Rejects were taken as the **delta** of the lifetime
counter across the same window, so both sides share one clock.

> **Caveat, stated honestly:** the accept figure is a **lower bound**. If a share
> is credited and the block advances inside one 0.4 s poll gap, that share is
> missed. Undercounting accepts can only push the measured accept rate *down*,
> so the true accept rate is **≥ 94.2 %**. The conclusion is robust to this.

---

## 2. The reject histogram, by cause

Pool-wide, over 724.8 s (deltas of the lifetime counters):

| cause | delta | share of rejects | what raises it |
|---|---|---|---|
| `verify_mismatch` | **+46** | **88.5 %** | `sigil-rpcd.rs:1750` — fell through BOTH the block gate and the share gate |
| `wallet_cap` | +4 | 7.7 % | `:1004` — `SHARE_WALLET_CAP` (64) hit for this height |
| `stale_height` | +2 | 3.8 % | `:1724` — submitted ≥2 heights behind tip |
| `duplicate` | 0 | 0 % | `:1012` |
| `window_full` | 0 | 0 % | `:1008` — `SHARE_GLOBAL_CAP` 4096 |
| `distribute_err` | 0 | 0 % | `:1810` |

**`verify_mismatch` is ~89 % of all rejects and is the only class worth chasing.**

The 2022-era "stale rigs" explanation
(`project_sigil_pool_rejections_diagnosed_2026_07_22`, which blamed pre-7.1
clients via `stale_height`) is **falsified twice over**: `stale_height` is 3.8 %
of rejects, not 99.7 %, and every rig reports a current version.

### The instrumentation gap this exposes

`verify_mismatch` is a **fall-through bucket**. Its own reason string admits it:

> `"dual-lane verify / header mismatch (wrong tip, target, or VDF)"`

That single key bundles at least five distinct failures — wrong tip hash, wrong
Lane-A target, wrong `vdf_t`, a bad VDF proof, and a header-binding mismatch —
which are diagnosed and fixed completely differently. **89 % of rejects land in
a bucket that does not say why.** Splitting it is the highest-value change to
the reject path and belongs to whoever holds `sigil-rpc`.

---

## 3. Where the rejects actually are — they are NOT fleet-wide

| wallet | rejects Δ | accepts | ease histogram (samples at each ease) |
|---|---|---|---|
| `03a51c92` | **0** | 122 | `8:212` `7:30` `6:4` |
| `c1fa04a9` | **0** | 125 | `8:232` `7:12` `6:2` |
| `434fe2d2` | +17 | 473 | `8:217` `7:18` `6:4` `5:2` `4:2` `3:1` `2:2` |
| `428e44a0` | **+35** | 123 | **`1:99`** `8:110` `7:14` `5:1` `4:1` `3:3` `2:4` |

**Two of four rigs have exactly zero rejects.** One rig (`428e44a0`) carries
**67 % of all rejects**.

The correlation is with **ease instability**, not hashrate and not client
version:

- The two zero-reject rigs sit at ease 8 in ~90 % of samples — effectively pinned.
- `428e44a0`, the worst rig, spends **43 % of samples at ease 1** — the hardest
  setting, a **2⁷ = 128× harder** share target than ease 8 — and flaps between 1
  and 8.

*Ease* = how many bits **easier** than the block target a share may be, so it is
the pool's per-miner difficulty knob (`vardiff`).

Supporting evidence — self-reported hashrate is wildly unstable, and `vardiff`
is a direct function of it (`vardiff_ease_for`, `:599`):

| wallet | versions reported | hps min | hps max | swing |
|---|---|---|---|---|
| `03a51c92` | 7.1.11 | 64 | 77,091 | 1,205× |
| `c1fa04a9` | 7.1.11 | 7 | 74,700 | 10,671× |
| `434fe2d2` | **7.1.11 AND 7.1.8** | 23 | 1,658,904 | 72,126× |
| `428e44a0` | 7.1.11 | 33 | 7,233,438 | **219,195×** |

---

## 4. Two facts that invalidate how this endpoint is being read

### 4a. `ver` is not trustworthy — one wallet reported two builds

Wallet `434fe2d2` reported **both `7.1.11` and `7.1.8`** across samples in this
one run. A single build cannot be two versions, so **multiple physical rigs are
sharing one wallet** — the ordinary farm setup (one payout address, N machines).

Every per-rig field in `sigil-rpcd` is keyed by **wallet**, not by rig:
`miner_hps` (`:1590`), `issued_ease` (`:148`, `:1612`), `miner_ver` (`:1600`),
and `SHARE_WALLET_CAP` (`:619`). `grep` for `rig_id|worker|rig_name` across
`flux-miner/src/client.rs` and `sigil-rpcd.rs` returns **nothing** — there is no
rig identity in the protocol at all.

Consequence: rigs **overwrite each other's** reported version. So `ver` supports
**neither** the old "stale rigs" story **nor** addendum 1's "all rigs are current
7.1.11". Both conclusions rest on a field that cannot carry them.

### 4b. There is no lifetime accept counter — the accept rate is not directly observable

`reject_by_wallet` is lifetime; there is no `accept_by_wallet`. The accept rate
in §1 had to be **reconstructed by polling** the per-block counter before it
resets. Any agent quoting an accept rate straight off this endpoint is quoting a
number the endpoint does not expose.

**This blocks SLICE 2's go/no-go.** The charter asks for a measured before/after
accept rate when tightening the Lane-A gate; that cannot be produced honestly
until a lifetime accept counter exists. **Recommended:** add `accept_by_wallet`
next to `reject_by_wallet` and expose it in `/mining/miners`.

---

## 5. What is confirmed, what is not

**Confirmed by this measurement:**
1. Accept rate is ≥ 94.2 %, not ~30 %. The ">70 % reject" premise is falsified.
2. `verify_mismatch` is 88.5 % of rejects; `stale_height` is 3.8 %.
3. Rejects are concentrated: 2 of 4 rigs at zero; one rig holds 67 %.
4. Reject count correlates with **ease instability**, not hashrate or version.
5. Multiple rigs share one wallet, and all per-rig pool state is wallet-keyed.
6. `shares` and `rejects` are on different clocks — the ratio is invalid.

**NOT confirmed — open, do not act as if settled:**
1. That the ease flapping *causes* the `verify_mismatch` rejects. The correlation
   is strong (r ≈ ease-spread, zero rejects on both pinned rigs) and there is a
   plausible mechanism — a share issued at one ease being verified at another —
   but this document does **not** contain a controlled reproduction.
2. That any of this explains the operator-reported GPU collapse (500 MH/s →
   <1 MH/s). That remains a code-read hypothesis elsewhere on the swarm bus. Not
   reproduced here.
3. Exact sub-causes inside `verify_mismatch` — **unknowable** until that bucket
   is split (§2).

---

## 6. Recommended next actions, ranked

1. **Split `verify_mismatch`** into distinct keys (wrong-tip / wrong-target /
   wrong-`vdf_t` / bad-VDF / header-mismatch). 89 % of rejects are currently
   undiagnosable. *Owner: whoever holds `sigil-rpc`.*
2. **Add `accept_by_wallet`** (lifetime). Without it no accept-rate claim —
   including SLICE 2's go/no-go — can be made honestly. *Owner: `sigil-rpc`.*
3. **Give the protocol a rig identity** (optional `&rig=` on the challenge fetch;
   key `miner_hps`/`issued_ease`/`miner_ver` by `(wallet, rig)`; keep payouts
   wallet-keyed). Absent `&rig=` must keep current behaviour so no rig is
   stranded. Fixes `ver` poisoning and makes `wallet_cap` per-rig.
4. **Damp vardiff** — the ease is an undamped step function of an untrusted,
   wildly-swinging self-report. Needs smoothing + hysteresis. ⚠️ `winner_weight`
   is `1 << ease` (`:1776`) and is load-bearing for payout correctness, so this
   needs a settlement test before it ships.

Items 1–4 are all in `crates/sigil-rpc`, which this session **released** — see
the identity-collision notice on the swarm bus (msg #21).

---

## Reproducing this

```bash
# 246 samples @ 0.4s against the live pool; read-only, safe to run in production.
while :; do curl -s http://127.0.0.1:8099/mining/miners; echo; sleep 0.4; done > samples.jsonl
```

Then per wallet: sum `max(shares)` per `height` for accepts, and take the delta
of `reject_counts` / `rejects` across the run for rejects. Do **not** divide the
`shares` and `rejects` fields of a single sample by each other (§1).
