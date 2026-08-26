# SIGIL — Full Chain Reset & Mainnet Launch Runbook

**Version:** written for **sigil-top v7.1.98** · workspace `sigil` 5.1.1
**Date:** 2026-08-26
**Author:** Rocky
**Status of THIS run:** testnet reset rehearsal. **We stay on testnet after it.** Every
mainnet-only step is marked 🔴 **MAINNET-ONLY — DO NOT RUN TODAY**.

---

## 0. What this document is for

Two things at once:

1. **The procedure for today** — wipe the SIGIL chain and every balance, restart from a
   fresh genesis, still as a testnet.
2. **A rehearsal for mainnet** — the mainnet launch is the *same* procedure plus a small
   number of extra, irreversible steps. Doing the testnet reset with this document in hand
   is how we find out which steps are wrong *while mistakes are still free*.

Read §1 before touching anything. The single most important idea:

> **A reset is only safe if old nodes CANNOT follow the new chain, and new nodes CANNOT
> follow the old one.** Everything else is bookkeeping.

---

## 1. The levers that separate two chains — there are THREE, not one

> **CORRECTION, learned the hard way during the 2026-08-26 g0→g1 reset.** An earlier draft
> of this document said `NETWORK_ID` was "the one lever". That was wrong, and following it
> produced a half-separated chain — twice. There are **three** independent places the
> generation is written, in two different crates, and bumping only some of them gives you
> the worst of both worlds: nodes still find each other and gossip, but every block is
> rejected at header validation. It looks like a mysterious sync failure, not like two
> chains being deliberately separated. Budget for all three.

| # | Where | What it controls | Symptom if you forget it |
|---|---|---|---|
| 1 | `sigil-header` `NETWORK_ID: [u8; 8]` | the id inside every block header | old blocks would be *accepted* — the actual danger |
| 2 | `sigil-net` `NETWORK_ID` / `NETWORK_ID_STR` | libp2p protocol negotiation, paths, sync-auth | banner still prints the old id; auth scoped to the old chain |
| 3 | `sigil-net` `PROTOCOL_PREFIX` + all five `TOPIC_*` | gossipsub topic names | **old nodes still deliver blocks**, rejected one-by-one at precheck — measured **429 log lines/minute** on a root filesystem that was 86 % full |

All three are now guarded by tests that fail the build if they drift:

- `sigil_net::tests::network_id_matches_the_header_crates_network_id` — #1 vs #2
- `sigil_net::tests::every_topic_carries_the_current_network_id` — #2 vs #3, deriving the
  expected prefix from `NETWORK_ID_STR` so the six literals cannot be missed again

**Run `fluxc test -p sigil-net` immediately after bumping the generation.** If those two
pass, the separation is complete. That check costs seconds and would have saved two full
rebuild-and-restart cycles.

## 1b. Why separation matters at all

The generation id is baked into every block header AND into the libp2p protocol prefix and
topic names. Bump all three (§1) and the two networks become mutually invisible: an old
node's blocks fail header validation, and the protocol/topic strings no longer match so the
two never exchange anything in the first place.

**If you wipe the database but do NOT bump the ids, you have not reset the chain.** You have
created a node that will re-sync the *old* chain from any peer that still has it, and every
wiped balance comes straight back. That is the failure this document exists to prevent — and
the partial version of it (headers bumped, topics not) is the one that actually happened.

| Run | generation | status |
|---|---|---|
| previous | `sigil-g0` | retired 2026-08-26, DB preserved |
| **current testnet** | **`sigil-g1`** | ✅ live since 2026-08-26 23:36 |
| 🔴 mainnet | `sigil-m1` (or similar — must differ from every testnet id ever used) | not started |

Exactly 8 bytes for #1 (it is a `[u8; 8]`, so a wrong length will not compile — a deliberate
guard). The topic prefix takes the short form: `sigil-g1` → `/sigil/g1/`.

---

## 2. What a reset destroys

Be explicit about this before running it, because it is not recoverable:

- **Every balance**, including Viktor's ~241 SIGIL and every miner's accumulated rewards.
- **Every mined block** — 2.3M+ heights, ~107 GB at `/home/storage/sigil-snap-db`.
- **Every shielded note** in the pool.
- **Every DEX pool and position.**
- **Every bridge lock record** (these are in-memory anyway and die on restart).
- **Total supply resets** to the genesis allocation; emission restarts from height 0.

What SURVIVES (because it is off-chain):

- Wallet **seeds/addresses** — a wallet still exists, it just has 0 balance.
- The **bridge vault seed** (`/root/.config/sigil/bridge-vault.seed`).
- The **release signing seed** (`/root/.config/sigil/release-sign.seed`).
- Anything wrapped on **Polygon** — ⚠️ see §6, this is the sharp edge.

---

## 3. Pre-flight (do these BEFORE stopping anything)

```bash
# 3.1 — Record the state you are about to destroy, so the reset is auditable.
curl -s http://127.0.0.1:18181/v1/supply
curl -s http://127.0.0.1:18181/v1/mining/miners | head -c 2000
journalctl -u sigil-node -n 50 --no-pager | grep -oE 'tip H=[0-9]+' | tail -1
```

```bash
# 3.2 — Back up the seeds. Losing these is unrecoverable and a reset does NOT regenerate them.
tar czf /home/storage/sigil-seeds-backup-$(date +%Y%m%d).tar.gz \
  -C / root/.config/sigil/bridge-vault.seed \
       root/.config/sigil/release-sign.seed \
       root/.config/sigil/producer-signing.seed
chmod 600 /home/storage/sigil-seeds-backup-*.tar.gz
```

```bash
# 3.3 — Keep the OLD chain rather than deleting it outright. Disk is cheap; a reset you
#       cannot inspect afterwards is a reset you cannot debug.
mv /home/storage/sigil-snap-db /home/storage/sigil-snap-db.g0-retired-$(date +%Y%m%d)
```

> Rename, do not `rm -rf`. If the new chain fails to start you want the old one still
> there. Delete it only after the new chain has been healthy for a few days.

**⚠️ Retire to a SIBLING path, never a nested one — and prune old generations.** Measured
2026-08-26: the "107 GB testnet chain" was only **7.8 GB** of actual chain. 91 GB of it was
`aether.pre-reset-20260815/` — a chain retired *inside* the store by an earlier reset and then
carried along by every reset since, like a Russian doll. Each reset silently inherited every
previous one.

Retiring the whole store directory (as §3.3 does) is correct. Retiring the inner `aether/`
directory in place is what creates the nesting. After any reset, check and prune:

```bash
du -sh /home/storage/sigil-snap-db*/                      # one entry per generation
du -sh /home/storage/sigil-snap-db*/aether*/ | sort -rh   # nested leftovers = a bug
```

Keep the most recent retired generation for rollback; delete older ones. **Preserve the tiny
key files first** — a retired store still contains `snapshot-sqisign.sk`:

```bash
tar czf /home/storage/sigil-keys-from-<gen>.tar.gz -C <old-store> \
  snapshot-sqisign.sk snapshot-sqisign.pk state-snapshot.bin chain.idx
```

Also prune `aether/chain.log.corrupt-backup` (7.4 GB in the g0 store) once the chain it
belonged to is retired — it is a recovery artifact, not history.

🔴 **MAINNET-ONLY:** additionally publish a **pre-announced launch time** and freeze the
release ≥24 h beforehand. A mainnet genesis nobody was told about is a mainnet nobody
joins.

---

## 4. The reset

### 4.1 Bump `NETWORK_ID`

```rust
// crates/sigil-header/src/lib.rs
pub const NETWORK_ID: [u8; 8] = *b"sigil-g1";
```

### 4.2 Set a fresh genesis timestamp

```rust
// crates/sigil-node/src/genesis.rs
pub const GENESIS_TIMESTAMP_MS: u64 = <the chosen launch instant, in ms>;
```

This **must** be a constant, never `now_ms()`. Every node computes block 0 locally; if the
timestamp differs by one millisecond between two nodes their genesis hashes differ and they
fork at height 0 with no error message that says so. The current value
(`1_748_538_000_000`) is the old launch; pick a new one and hard-code it.

### 4.3 Review the genesis allocation

`crates/sigil-node/src/genesis.rs`:

- `GENESIS_AI_WALLETS` — Rocky, Vicarious, Quinn, Mimer + `GENESIS_AI_ENDOWMENT`.
- `MASTER_WALLET_GENESIS` — Viktor's dev-fee wallet (5 % of coinbase, 0.3 % of DEX swaps).
- `DEMO_WALLET` / `DEMO_INITIAL_BALANCE` — 🔴 **MAINNET-ONLY:** remove or zero the demo
  allocation. A demo wallet with a known key holding real value is a free withdrawal.

The AI-wallet table is BLAKE3-committed into the genesis header, so this table *is* part of
the chain identity. Changing it changes the genesis hash — which is fine and intended here,
but it means the table must be final **before** launch, not after.

### 4.4 Build

```bash
cd /home/storage/deepseek-codewhale/sigil
systemd-run --scope -p MemoryMax=16G -p CPUQuota=600% bash -c \
  'export FLUX_WRAPPER_PATH=$HOME/.flux/bin/fluxc; \
   cd /home/storage/deepseek-codewhale/sigil && \
   ionice -c3 nice -n15 $FLUX_WRAPPER_PATH build --release -p sigil-node'
```

Never raw `cargo`; always resource-capped so the live node is not starved.
⚠️ `fluxc build --release -p a -p b` silently builds only ONE package — issue one command
per package. (Measured 2026-08-26; it cost a deploy cycle.)

### 4.5 Start the new chain

```bash
systemctl stop sigil-bridge-relayer     # ALWAYS stop the relayer first — see §6
systemctl restart sigil-node
journalctl -u sigil-node -f | head -40  # expect H=0,1,2… and the new genesis hash
```

Verify it is genuinely a new chain, not a resumed old one:

```bash
curl -s http://127.0.0.1:18181/v1/supply     # native_supply == the genesis allocation only
journalctl -u sigil-node -n 200 --no-pager | grep -oE 'tip H=[0-9]+' | tail -1   # small
```

If the height jumps into the millions, the `NETWORK_ID` bump did not take effect and the
node is syncing the old chain from a peer. **Stop and fix §4.1 before continuing.**

### 4.6 Re-key the bridge vault (recommended)

The vault's note ledger refers to notes that no longer exist on the new chain. Retire it so
index allocation starts clean:

```bash
systemctl stop sigil-node
mv /root/.config/sigil/bridge-vault-notes.jsonl \
   /root/.config/sigil/bridge-vault-notes.jsonl.g0-retired
systemctl start sigil-node
journalctl -u sigil-node -n 50 --no-pager | grep "bridge vault"   # expect the pk line
```

Keep the **seed** (the vault identity is fine to carry over); retire only the ledger.

---

## 5. Post-reset verification — do not skip

| # | Check | Command | Expected |
|---|---|---|---|
| 1 | Node healthy | `systemctl show sigil-node -p NRestarts -p ExecMainStartTimestamp` | `NRestarts=0` |
| 2 | Producing | `journalctl -u sigil-node -n 50 \| grep 🏭` | height climbing from 0 |
| 3 | Supply is genesis-only | `curl -s .../v1/supply` | equals the allocation |
| 4 | Balances wiped | `curl -s ".../v1/balance?wallet=<any>"` | `0` |
| 5 | Vault loaded | `journalctl -u sigil-node \| grep "bridge vault"` | `🔐 bridge vault loaded` |
| 6 | Peers | `journalctl -u sigil-node \| grep heartbeat` | `peers=` non-zero |
| 7 | **Old chain unreachable** | fresh node with OLD binary | must NOT peer |

Check 7 is the one that proves the reset. If an old-`NETWORK_ID` node can still peer, the
chains are not separated.

---

## 6. 🚨 The bridge is the sharp edge of any reset

**Wrapped SIGIL on Polygon does not reset.** The ERC-20 at
`0xc224602C32F5c7f68d3Ef002aE4C99e4C7Df25B7` keeps whatever supply it had, but the SIGIL
that backed it is gone with the old chain. The 1:1 invariant breaks the instant you reset,
unless you handle it deliberately.

Today this is harmless — verified 2026-08-26, `totalSupply() == 0`, the bridge has never
successfully minted. **That is exactly why this rehearsal is cheap and a mainnet reset
would not be.**

Rules:

1. **Stop the relayer before the reset, start it after.** A running relayer during a reset
   can mint against locks whose backing no longer exists.
2. **Reset the relayer watermark** — `/home/orobit/sigil-bridge-relayer/state.json` →
   `last_lock_id: 0`. Lock ids restart at 1 on the new chain, so a stale watermark would
   skip every new lock forever.
   ```bash
   systemctl stop sigil-bridge-relayer
   python3 - <<'PY'
   import json,pathlib
   p=pathlib.Path('/home/orobit/sigil-bridge-relayer/state.json')
   s=json.loads(p.read_text()); s['last_lock_id']=0; p.write_text(json.dumps(s,indent=2))
   PY
   ```
   Leave `last_polygon_block` alone — Polygon did not reset.
3. 🔴 **MAINNET-ONLY:** if wrapped supply is non-zero at launch, either deploy a **fresh**
   wrapped contract or honour the old supply with a genesis allocation. Do not hand-wave
   this; it is real value.

---

## 7. Release ceremony — cutting v7.1.98

A binary in `downloads/` reaches **nobody**. The auto-updater trusts only the *signed
manifest*. One command does the whole ceremony and fails loud at every step:

```bash
cd /home/storage/deepseek-codewhale/sigil
bash scripts/release-sigil-top.sh 7.1.98 "chain reset — NETWORK_ID sigil-g1, fresh genesis"
```

It bumps `Cargo.toml`, builds linux+windows via `fluxc`, signs both (SQIsign-L5 + Ed25519),
publishes to **both** download channels, writes and signs `sigil-top-latest.json`, then
**re-fetches the live manifest over the network and verifies it against the pinned key**
before declaring success. Confirm independently:

```bash
curl -s https://sigilgraph.fluxapp.xyz/downloads/sigil-top-latest.json
```

⚠️ Existing clients on the old `NETWORK_ID` will update to v7.1.98 and then find themselves
unable to peer with the old chain — which is the intent. Say so in the release note, or it
reads as a bug.

---

## 8. 🔴 MAINNET-ONLY — the extra steps

Everything above, plus:

1. **`NETWORK_ID` = a mainnet id** never used by a testnet.
2. **Remove demo allocations** (`DEMO_WALLET` / `DEMO_INITIAL_BALANCE`).
3. **Fresh master keypair** via `scripts/gen-master-key.sh`; back it up off-box. The testnet
   master key must never guard mainnet value.
4. **Finalise the genesis allocation table** — it is BLAKE3-committed into the header and
   cannot change afterwards without a fork.
5. **Decide `SHIELDED_ONLY_HEIGHT`.** It is `0` today, and its own comment says that is only
   safe because *"nothing is deployed, every shielded pool is empty"*. For mainnet: keep `0`
   for privacy-from-genesis (then **every** money path must be shielded-aware — the bridge
   now is; verify the DEX and USDS bridge before launch), **or** set a future height and
   announce it.
6. **Seed backups off-box**, tested by restoring to a scratch host.
7. **Announce the launch time** and publish the expected genesis hash so joiners can verify
   they are on the right chain.
8. **Independent verification** — a second machine syncing from genesis and reaching the
   same tip hash. One node agreeing with itself proves nothing.

---

## 9. Rollback

Nothing here is irreversible **until the seeds are gone or the old DB is deleted**.

```bash
systemctl stop sigil-node sigil-bridge-relayer
mv /home/storage/sigil-snap-db          /home/storage/sigil-snap-db.g1-abandoned
mv /home/storage/sigil-snap-db.g0-retired-<date> /home/storage/sigil-snap-db
# revert NETWORK_ID + GENESIS_TIMESTAMP_MS, rebuild, restart
```

This works **only** because §3.3 renamed the old database instead of deleting it. That is
the entire reason for the rule.

---

## 9b. What the 2026-08-26 rehearsal actually taught us

Three things that were not obvious in advance, all now folded into the steps above:

1. **Three id locations, not one** (§1). Cost two extra rebuild+restart cycles. Now
   test-guarded.
2. **`fluxc build --release -p a -p b` silently builds only ONE package.** Issue one command
   per package. This wasted a deploy cycle earlier the same evening.
3. **Disk accounting was 93 % misleading.** The chain looked like 107 GB; it was 7.8 GB of
   chain plus 91 GB of nested dead generations and a 7.4 GB corrupt-backup. Always break the
   number down before believing it.
4. **Blocks are stored as JSON**, not a binary encoding — `serde_json::to_vec(block)`, so every
   32-byte hash is written as a decimal array like `[153,13,136,…]`. Measured **3,589 bytes per
   block** on blocks carrying ~zero transactions, roughly 4× what bincode would use. Changing
   the settlement-path storage format is cheap now (a 5k-block chain) and expensive once mainnet
   has history — see §10.
5. **A stale client on the old generation will hammer the new node.** An old
   `sigil-top-v7.1.42` on the same box pushed g0 blocks at ~429 rejected blocks/minute until
   it was stopped. Before declaring a reset clean, check for leftover clients:
   `pgrep -af "sigil-top|sigil-node"`. Post-reset, **every** existing client is on the wrong
   network until it updates — that is by design, but say so in the release note or it reads
   as an outage.

Measured result of the rehearsal: genesis hash `b0671d03…`, supply `0` at H=0, ~26–29
blocks/s, 6 live miners reconnected, 0 cross-chain rejects once the stale client was stopped.

## 10. Open items to settle before mainnet

- [ ] **Bridge return leg** (Polygon → SIGIL). `submit_unlock` still builds a transparent
      `SigilTx::Send` from the vault, which consensus retired — the same defect that broke
      the outbound lock. Must become an `Unshield` (proof-carrying, via
      `sigil_shield::wallet::build_spend`) before the bridge is two-way.
- [ ] **Wallet frontend** must use the two-phase lock
      (`/v1/bridge/lock/prepare` → sign → `/v1/bridge/lock`).
- [ ] **DEX and USDS bridge** audited against the privacy-only rule the way the bridge now is.
- [ ] **Independent second node** verified from genesis.
- [ ] **`SHIELDED_ONLY_HEIGHT` decision** recorded (§8.5).
- [ ] **Binary-encode the chain log** (bincode + zstd, both already used elsewhere in the tree)
      instead of JSON. ~4× smaller on disk and faster to replay/backfill, since JSON parsing
      dominates those paths. Do it BEFORE mainnet has history to migrate.
