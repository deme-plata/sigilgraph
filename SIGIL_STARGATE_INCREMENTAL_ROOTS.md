# Stargate #1 — incremental roots: integration patch

> **Status:** accumulator shipped + proven (rocky-sigil). lib.rs wiring is the
> drop-in below — blocked only on rocky's active `sigil-state/src/lib.rs`
> claim (P5-B/C). Lands the #1 measured-impact Stargate item.

## What's done

`sigil-state::acc::Accumulator` — incremental additive multiset hash. **9/9
tests** (incl. `incremental_matches_from_scratch_over_random_sequence`: 5000
random ops, incremental == fresh fold). O(1) per insert/remove/update, O(1)
root.

**Micro-bench (`examples/acc_bench.rs`, release, Epsilon), 1000 blocks each
touching one leaf:**

| state size | whole-map rehash | incremental | speedup |
|---|---|---|---|
| 1,000   | 119.71 ms | 0.947 ms | **126×** |
| 10,000  | 1,116.75 ms | 0.977 ms | **1,143×** |
| 100,000 | 24,757.34 ms | 0.971 ms | **25,488×** |

Incremental is flat ~0.95ms regardless of state size. The chronos-measured
73%-of-wall-clock roots cost goes to ~0.

## The wiring patch (lib.rs — for rocky, or me when the claim frees)

### 1. Add accumulator fields to `SigilState`

```rust
pub struct SigilState {
    // ... existing fields ...
    pub(crate) wallet_acc:   acc::Accumulator,
    pub(crate) pool_acc:     acc::Accumulator,
    pub(crate) contract_acc: acc::Accumulator,
}
```

`Accumulator` is `Default`, so `#[derive(Default)]` on `SigilState` keeps working.

### 2. Canonical leaf encoders (free functions in lib.rs)

```rust
fn wallet_leaf_key(w: &WalletId, t: &TokenId) -> Vec<u8> {
    let mut k = Vec::with_capacity(64); k.extend_from_slice(w); k.extend_from_slice(t); k
}
fn u128_leaf_val(a: u128) -> Vec<u8> { a.to_le_bytes().to_vec() }
```

(Pools: key = pool id (32B), value = `serde_json::to_vec(&PoolState)`.
 Contracts: key = `contract ‖ slot` (64B), value = the 32B slot bytes.)

### 3. Mirror the BTreeMap mutators into the accumulator

The mutators are already the chokepoint. Each one reads the OLD value before
overwriting — that's exactly what `update`/`remove` need:

```rust
pub(crate) fn set_balance(&mut self, wallet: WalletId, token: TokenId, amount: u128) {
    let key = wallet_leaf_key(&wallet, &token);
    let old = self.wallets.get(&(wallet, token)).copied();
    match (old, amount) {
        (Some(o), 0)             => self.wallet_acc.remove(&key, &u128_leaf_val(o)),
        (Some(o), n) if o != n   => self.wallet_acc.update(&key, &u128_leaf_val(o), &u128_leaf_val(n)),
        (None, n) if n != 0      => self.wallet_acc.insert(&key, &u128_leaf_val(n)),
        _                        => {}            // unchanged / zero→zero
    }
    if amount == 0 { self.wallets.remove(&(wallet, token)); }
    else           { self.wallets.insert((wallet, token), amount); }
}
```

Same shape for `set_pool` (value = `serde_json::to_vec(&PoolState)`) and
`set_contract_slot` (value = the 32B; all-zero = remove, matching today).

### 4. `roots()` becomes O(1)

```rust
pub fn roots(&self) -> StateRoots {
    StateRoots {
        wallet_state_root:   self.wallet_acc.root(),
        dex_state_root:      self.pool_acc.root(),
        event_log_root:      hash_event_log(&self.block_events), // per-block, stays
        contract_state_root: self.contract_acc.root(),
    }
}
```

### 5. Keep the old `hash_map` as `slow_roots()` for a cross-check test

```rust
#[cfg(test)]
fn slow_wallet_root(&self) -> Root {
    let acc = acc::Accumulator::from_leaves(
        self.wallets.iter().map(|((w,t),a)|
            (wallet_leaf_key(w,t), u128_leaf_val(*a))));
    acc.root()
}
#[test] fn incremental_root_matches_fold_after_random_txs() { /* … */ }
```

## Honest scope

- This buys the **fast root**, not proofs. Inclusion / non-membership proofs
  still want the genesis-doc P3 Sparse Merkle Tree. Tip-proof light clients
  that only compare roots are unaffected (they already just compare).
- Single 256-bit additive lane is a Phase-0 perf prototype. Production
  soundness wants LtHash-style wide lanes or the real SMT — documented in
  `acc.rs`. The chronos wind-tunnel only needs determinism, which it has.
- Genesis hash changes (again) — expected for a root-definition change,
  fine pre-mainnet.

## Re-measure (the Stargate loop)

After wiring, re-run `sigil-throughput`. Expectation per the bench: the 73%
roots fraction collapses, execution becomes the dominant cost on small
blocks too, and single-thread TPS jumps toward the 83k that big blocks
already showed (because roots stop being the tax). That re-measurement is
the proof the wall moved — then Stargate #2 (parallel execution) is next.

— rocky-sigil 🟣

---

## MEASURED (roots_throughput wind tunnel, release, Epsilon) — 2026-05-30

10,000 wallets · 1,000 blocks × 100 tx (100k tx), real transfer workload:

| strategy | TPS | roots% | exec% |
|---|---|---|---|
| A whole-map rehash (today) | 69,104 | 97% | 3% |
| B accumulator · BLAKE3 | 368,805 | 0.1% | 99.9% |
| C accumulator · fasthash (ceiling) | 796,570 | 0.2% | 99.8% |

**Stargate #1 (A→B): 5.3× TPS, roots 97%→0.1%.** Confirmed end-to-end.

**The BLAKE4 lever (B→C): +116% TPS — a REAL lane, not dead.** Once roots are
O(1), the per-leaf hash (4× per tx: old+new for from+to) is ~half of execution.
A faster leaf hash nearly doubles TPS.

**CAVEAT — C is the ceiling, not shippable.** The wyhash leaf is non-crypto;
an additive accumulator needs collision-resistant leaves or two distinct
states can share a sum → root → state forgery. The shippable lever is a
faster *cryptographic* leaf hash (reduced-round BLAKE3 / sound fast hash),
keeping full BLAKE3 on the committed root. Capturable fraction of the +116%
TBD — that's the next measurement. Internal leaf hash → swappable primitive
under flux-eternal-cypher's crypto-agility layer (rocky #101 addendum).
