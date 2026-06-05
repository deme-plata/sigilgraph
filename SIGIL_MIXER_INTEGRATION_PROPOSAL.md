# SIGIL — sigil-mixer ↔ sigil-tx integration spec

> **Status:** Spec / drop-in patch. Author: rocky-sigil. 2026-05-29.
> **Predecessors:** sigil-mixer v0 (rocky-updater #56), sigil-mixer Phase 1 STARK (rocky-updater #58).
> **Direct dependency:** P5-B (rocky's "wire sigil-dex through apply_tx") must land first — both touch `sigil-tx::apply_tx` and `sigil-state::StateMutation`.

## TL;DR

Viktor's 2026-05-29 directive: *all SIGIL transactions are private by default*. This doc is the patch rocky can drop into `sigil-tx` + `sigil-state` after P5-B settles, to wire `sigil_mixer::ShieldedSendTxData` end-to-end through `apply_tx` + the chokepoint.

What's already shipped (precursor):
- `SigilEvent::ShieldedSend { token_hint, fee, n_inputs, n_outputs, proof_digest, pool_root_at_proof }` — tag 11, appended to preserve indexer tag-stability. **7/7 sigil-events tests green.**

What this doc specs (to land after P5-B):
- `SigilTx::ShieldedSend(ShieldedSendTxData)` — new variant
- `StateMutation::SpendNullifier(Nullifier)` + `StateMutation::InsertCommitment(Commitment)`
- `SigilState.shielded_pool: ShieldedPool` (`pub(crate)`)
- `SigilState::shielded_pool_root()` reader (**NOT** in header v0 — keeps 4-root schema stable)
- `apply_tx` ShieldedSend arm
- `SigilTx::Send` deprecation path (separate commit)

---

## Wire shape (sigil-tx side)

Append to the `SigilTx` enum after `ValidatorLeave`:

```rust
/// Confidential send via the SIGIL shielded pool. Replaces `SigilTx::Send`
/// once the transparent variant is removed per Viktor's directive
/// (2026-05-29: "all transactions private by default"). Wire shape comes
/// directly from `sigil_mixer::ShieldedSendTxData` — input_nullifiers +
/// output_commitments + fee + token_hint + proof bundle. The producer's
/// libp2p identity signs the *outer* SignedTx as usual; the *inner* ZK
/// proof attests membership + range + sum-conservation + ownership of the
/// spent commitments.
ShieldedSend {
    /// Inner mixer wire data — see `sigil_mixer::tx::ShieldedSendTxData`.
    data: sigil_mixer::ShieldedSendTxData,
},
```

`SigilTx::tag()` gets a new arm — tag 9 stays free (Validator{Join,Leave} occupy 7+8), so ShieldedSend takes tag 9; bump ValidatorJoin/ValidatorLeave to 10/11 — actually NO, **don't reorder**. Append at the end:

```rust
SigilTx::Send            { .. } => 0,
SigilTx::Swap            { .. } => 1,
SigilTx::LpDeposit       { .. } => 2,
SigilTx::LpWithdraw      { .. } => 3,
SigilTx::ContractCall    { .. } => 4,
SigilTx::ContractDeploy  { .. } => 5,
SigilTx::TokenDeploy     { .. } => 6,
SigilTx::ValidatorJoin   { .. } => 7,
SigilTx::ValidatorLeave  { .. } => 8,
SigilTx::ShieldedSend    { .. } => 9,
```

`SigilTx::fee()`:

```rust
SigilTx::ShieldedSend { data, .. } => data.fee,
```

`SigilTx::fee_payer()`: **the shielded path doesn't expose a payer wallet.** Fee is debited from a separate, well-known protocol fee bucket OR (Phase 0 stub) from `[0u8; 32]` and the chain accepts deficit. Cleanest: introduce a `FEE_BUCKET_WALLET` constant in sigil-state and have shielded fees draw from there. P5-B's sigil-dex routes its fees the same way (see rocky's `sigil-bank` master-wallet pattern, msg #61).

```rust
SigilTx::ShieldedSend { .. } => FEE_BUCKET_WALLET,  // see sigil-state
```

---

## State machine (sigil-state side)

### 1. Add to `SigilState`

```rust
use sigil_mixer::ShieldedPool;

pub struct SigilState {
    // ... existing fields ...
    pub(crate) shielded_pool: ShieldedPool,
}
```

`Default`, `Clone` — straightforward (ShieldedPool already derives both).

### 2. Two new `StateMutation` variants

Append at the end:

```rust
pub enum StateMutation {
    // ... existing variants ...
    /// Mark a nullifier as spent in the shielded pool. Rejects if it's
    /// already in the spent set — that's a double-spend, the producer
    /// shouldn't have included this tx.
    SpendNullifier(sigil_mixer::Nullifier),
    /// Insert a new output commitment into the shielded pool.
    InsertCommitment(sigil_mixer::Commitment),
}
```

### 3. Chokepoint dispatch

In `commit_state_transition`'s match:

```rust
StateMutation::SpendNullifier(n) => {
    state.shielded_pool.spend(n).map_err(|e| {
        CommitError::Invariant(format!("shielded pool spend: {e}"))
    })?;
}
StateMutation::InsertCommitment(c) => {
    state.shielded_pool.insert(c);
}
```

`ShieldedPool::spend` already returns `PoolError::DoubleSpend` on a re-spend — the chokepoint promotes that to `CommitError::Invariant`, which the block validator turns into a `STATE DIVERGENCE` halt at `chain.apply`.

### 4. Reader

```rust
impl SigilState {
    /// BLAKE3 fingerprint of the shielded pool — sorted (commitment-set,
    /// spent-nullifier-set). NOT in the header v0 schema. P1 header bump
    /// promotes this as a fifth state root alongside wallet/dex/event_log/
    /// contract.
    pub fn shielded_pool_root(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for c in &self.shielded_pool.commitments {
            hasher.update(c.as_bytes());
        }
        // Distinguishable boundary so a commitment of all-zeros can't
        // collide with a spent nullifier of all-zeros.
        hasher.update(b"||");
        for n in &self.shielded_pool.spent_nullifiers {
            hasher.update(n.as_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}
```

---

## apply_tx arm

```rust
SigilTx::ShieldedSend { data } => {
    // 1. Cryptographic check (precheck + nullifier freshness + ZK).
    //    In Phase 0 sigil-mixer's verify_shielded_send returns Ok for any
    //    well-formed tx; Phase 1 with --features real-zk runs the actual
    //    flux-zk-stark verify.
    let _ok = sigil_mixer::verify_shielded_send(data, &state.shielded_pool)
        .map_err(|e| TxApplyError::Shielded(e.to_string()))?;

    // 2. Fee debit from the fee bucket (Phase 0 stub — see fee_payer above).
    //    Allow deficit at Phase 0 so the demo doesn't need bucket-seeding
    //    in genesis.
    let bucket_native = state.balance_of(&FEE_BUCKET_WALLET, &NATIVE);
    let new_bucket = bucket_native.saturating_sub(data.fee);
    out.mutations.push(StateMutation::SetBalance {
        wallet: FEE_BUCKET_WALLET, token: NATIVE, amount: new_bucket,
    });

    // 3. Mark every input nullifier as spent.
    for n in &data.input_nullifiers {
        out.mutations.push(StateMutation::SpendNullifier(*n));
    }

    // 4. Insert every output commitment.
    for c in &data.output_commitments {
        out.mutations.push(StateMutation::InsertCommitment(*c));
    }

    // 5. Emit the event.
    let proof_digest = *blake3::hash(&data.proof.proof_bytes).as_bytes();
    let evt = SigilEvent::ShieldedSend {
        token_hint: data.token_hint,
        fee: data.fee,
        n_inputs:  data.input_nullifiers.len()  as u32,
        n_outputs: data.output_commitments.len() as u32,
        proof_digest,
        pool_root_at_proof: data.proof.commitment_set_root,
    };
    out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
    out.events.push(evt);
}
```

New error variant in `TxApplyError`:

```rust
/// Shielded-send verification failed in the mixer layer.
#[error("shielded send: {0}")]
Shielded(String),
```

---

## Tests (sigil-tx)

Three minimum:

```rust
#[test]
fn shielded_send_happy_path() {
    let mut s = SigilState::new();
    // Construct a ShieldedSendTxData via sigil_mixer::test_helpers
    // (rocky-updater shipped these — see sigil-mixer/src/verify.rs#tests
    // for the fixture pattern).
    let data = sigil_mixer::test_helpers::fixture_tx();
    let signed = dummy_signed(SigilTx::ShieldedSend { data: data.clone() });
    let result = apply_tx(&s, &signed).unwrap();

    // 1 fee debit + 2 nullifiers + 2 commitments + 1 event = 6 mutations
    assert_eq!(result.mutations.len(), 6);
    assert_eq!(result.events.len(), 1);
    assert!(matches!(result.events[0], SigilEvent::ShieldedSend { .. }));

    // Apply and confirm pool advanced.
    let transition = batch_into_transition([result], 1);
    commit_state_transition(&mut s, &transition, 1).unwrap();
    assert_eq!(s.shielded_pool.commitment_count(), 2);
    assert_eq!(s.shielded_pool.spent_count(), 2);
}

#[test]
fn shielded_send_rejects_double_spend() {
    let mut s = SigilState::new();
    // Pre-mark a nullifier as spent so the second attempt sees it.
    let data = sigil_mixer::test_helpers::fixture_tx();
    let pre = StateTransition {
        at_height: 0,
        mutations: vec![StateMutation::SpendNullifier(data.input_nullifiers[0])],
    };
    commit_state_transition(&mut s, &pre, 0).unwrap();

    let signed = dummy_signed(SigilTx::ShieldedSend { data });
    let err = apply_tx(&s, &signed).unwrap_err();
    assert!(matches!(err, TxApplyError::Shielded(_)));
}

#[test]
fn shielded_pool_root_changes_after_commit() {
    let mut s = SigilState::new();
    let before = s.shielded_pool_root();
    let data = sigil_mixer::test_helpers::fixture_tx();
    let signed = dummy_signed(SigilTx::ShieldedSend { data });
    let result = apply_tx(&s, &signed).unwrap();
    let transition = batch_into_transition([result], 1);
    commit_state_transition(&mut s, &transition, 1).unwrap();
    assert_ne!(s.shielded_pool_root(), before);
}
```

If `sigil-mixer` doesn't expose `test_helpers::fixture_tx` yet, the `verify.rs#fixture_tx` helper (rocky-updater's existing test module) can be promoted to `pub(crate)` or moved into a public `pub mod test_helpers` — small change in sigil-mixer.

---

## Send deprecation (separate commit)

Per Viktor's directive, `SigilTx::Send` should disappear. Suggested sequence:

1. **This integration patch** — adds `ShieldedSend`, keeps `Send` so build stays green.
2. **Follow-up #1** — wallets and `produce-block` demo fixture migrate to `ShieldedSend`. `verify-tip.html` already doesn't render tx details (works against tip-proof roots only — no change needed).
3. **Follow-up #2** — `SigilTx::Send` removed. `apply_tx` Send arm deleted. `SigilEvent::Send` + `SigilEvent::Receive` either also deleted OR repurposed for ContractCall side-effects (since `ShieldedSend` doesn't emit `Receive`).

The follow-ups can land any time; the integration patch is the unblock.

---

## What this doc deliberately is NOT

- A patch to `sigil-tx::SigilTx::Swap/LpDeposit/LpWithdraw` to make them shielded — those still need a port of `sigil-mixer` for DEX flows. Out of scope; tracked separately.
- A header schema bump to add `shielded_pool_root` as a fifth state root — that's a P1 SIGIL_GENESIS_v0.md amendment with its own broadcast.
- A change to `sigil-node produce-block`'s tx fixture — happens in follow-up #1 above.

---

## Sequencing

```
P5-B (rocky, in flight)  ─┐
                          ├──► sigil-mixer integration patch (this doc)
sigil-events ShieldedSend ┘    │
(rocky-sigil, ✓ shipped)       │
                               ├──► Send deprecation (follow-up #1, #2)
                               │
                               ├──► P4-B sigil-node join (rocky)
                               │      uses ShieldedSend events to render
                               │      anonymity-set turnover on join
                               │
                               └──► sigil-dex shielded (separate, larger)
```

— rocky-sigil 🟣
