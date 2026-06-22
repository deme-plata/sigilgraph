# SIGIL rpcd lazy-load fix — design v0 (2026-06-21, claude-desktop-viktor)

## Symptom
`sigil-rpcd.service` (serves `:8099 /api/v1/{mining,swap,wallet}`) is deliberately
STOPPED. Starting it OOM-crash-loops: a ~29 MB on-disk snapshot expands to >24 GiB
live and dies under `MemoryMax=6442450944` (6 GiB). This is why sigil-top mining
"doesn't connect" — the producer endpoint is down (server-side, reproducible on ANY
client incl. 4.0.1).

## Root cause (verified)
- `sigil-rpcd.rs:462-467` opens flux-db state + `load_snapshot()`; bincode-decodes the
  whole `SigilState` into RAM before serving.
- `sigil-state/src/lib.rs:130` `wallets: BTreeMap<(WalletId,TokenId), u128>`. With ~132M
  entries, BTreeMap node + allocator overhead turns 29 MB serialized into >24 GiB live.
  (`contracts` is the same shape; `pools` is small.)
- NOT a block-replay problem and NOT fixable by raising MemoryMax (it OOM'd at 24G too).

## Why the fix is tractable
Root computation is ALREADY O(1): `wallet_acc`/`native_supply`/`pool_acc`/`contract_acc`
accumulators are maintained incrementally in `set_balance`/`set_pool`/`set_contract_slot`,
so `roots()` never iterates the full map. The ONLY thing forcing the full map into RAM is
point read/write (`balance_of`, `set_balance`) + snapshot (de)serialization.

## Fix design
Back `wallets` (and `contracts`) with a flux-db column family instead of `BTreeMap`:
- `balance_of(w,t)` -> flux-db `get(cf_wallets, key=(w‖t))` (default 0).
- `set_balance(w,t,v)` -> flux-db `put/delete` + the EXISTING O(1) accumulator update
  (sub old leaf, add new) + `native_supply` delta. No semantic change.
- Startup: do NOT decode the wallets map. Load only the small header (native_supply,
  master_wallet, the four accumulators, pools) from the snapshot; the wallet CF is already
  durable on disk. RAM stays bounded (MB, not GiB).
- Keep an in-memory write-back/LRU cache for hot wallets if needed for throughput.

## Consensus / byte-stability safety
- Roots are derived from accumulators, NOT map layout -> identical roots, no consensus drift.
- Snapshot layout changes (no longer embeds the full wallets map). Migration: on first boot,
  if an old full-map snapshot is found, stream it once into the wallet CF, then write the new
  small-header snapshot. Gate behind a falsifiable check: post-migration `wallet_state_root`
  == pre-migration root (MUST match or abort).
- Preserve the bincode field order note at lib.rs:208 (events_acc stays maintained).

## Test plan (each a falsifiable gate)
1. Unit: balance_of/set_balance via flux-db == old BTreeMap semantics (proptest).
2. Root-equivalence: replay a known block range, assert all four roots byte-identical to a
   pre-change run.
3. Boot-RAM: load the real 132M snapshot; RSS stays < 1 GiB (was >24 GiB); rpcd serves
   /api/v1/mining/challenge within N seconds.
4. Migration: old snapshot -> new store -> wallet_state_root unchanged; kill-9 mid-migration
   leaves a recoverable store.

## Interim options (NONE are clean — documented honestly)
- Raising MemoryMax: REJECTED (OOM'd at 24G, risks the whole box / other sigil services).
- "Mine-only" rpcd that serves challenge/submit without the full money map: still needs
  wallet credit on accepted shares -> effectively a subset of this fix. Not shorter.
- => The flux-db backing IS the path. Estimate: focused 1-2 day implement+test pass.

## Ownership
sigil-state + sigil-rpc are UNLEASED as of 2026-06-21 (flux_file_list). Money-path:
needs the two-mind / DeepSeek adversarial review on the root-equivalence + migration gates
before any restart. Do NOT start sigil-rpcd until gate 3 (boot-RAM) passes.
