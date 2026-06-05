# SIGIL Mixer — Privacy Architecture Proposal

> **Viktor's directive (2026-05-29):** "I want the quillon graph quantum mixer all included in sigil and also the zk-stark and zk-snark for private total transactions no opt in for that so we already got a lot zk stark going on in flux."

This doc proposes how `sigil-mixer` (rocky-updater, scaffolded today) integrates with `sigil-tx` + `sigil-state` (rocky-sigil's territory) to make every SIGIL transaction private by default — no opt-in for cleartext.

**Status:** scaffolding shipped (26/26 tests). Integration into sigil-tx/state is a coordinated next-session task with rocky-sigil. This doc is the proposal for that coordination.

---

## What "private by default" means on the wire

A SIGIL transaction never carries `(from, to, amount)` in cleartext. The wire format for a payment is:

```
ShieldedSend {
  input_nullifiers:   Vec<[u8; 32]>   // spend tags, sorted
  output_commitments: Vec<[u8; 32]>   // new commitments, sorted
  fee:                u128             // cleartext (block-reward calc)
  token_hint:         [u8; 32]         // single-token shortcut; empty if multi
  proof:              PrivacyProofBundle  // ZK proof of validity
}
```

What a non-participant sees: 32-byte tags going in, 32-byte tags coming out, a fee, a proof. Nothing about who sent what to whom.

What the network enforces (via the embedded ZK proof, Phase 1 onwards):
1. Every `input_nullifier` was derived from a commitment in the shielded pool.
2. Every `output_commitment` is well-formed (value in `[0, 2^64]`, range proof).
3. `Σ input_values == Σ output_values + fee`, all under one token tag.
4. The signer owns the spending keys for each input.

---

## Crate layout (proposed)

| Crate | Owner | Role |
|---|---|---|
| `sigil-mixer` | rocky-updater (shipped today) | Commitment / Nullifier / ShieldedPool / ShieldedSendTxData / verify |
| `sigil-tx` | rocky-sigil | Replace `SigilTx::Send` with `SigilTx::ShieldedSend(ShieldedSendTxData)`. Same for `Swap`, `LpDeposit`, `LpWithdraw` — all private. |
| `sigil-state` | rocky-sigil | Replace `wallets: BTreeMap<(WalletId, TokenId), u128>` with `shielded_pool: ShieldedPool`. `wallet_state_root` becomes `shielded_pool_root` (Merkle root over commitments + nullifiers). |
| `sigil-events` | rocky-sigil | `SigilEvent::Send` / `Receive` drop `(from, to, amount)` fields; carry `(nullifiers, output_commitments)` instead. |
| `sigil-mixer-coord` | future | Active mixing rounds (q-quantum-mixing's `mixing_pool.rs` port, 565 LOC). Phase 2+. |
| `sigil-mixer-stealth` | future | Stealth addresses (q-quantum-mixing's `stealth_addresses.rs`, 525 LOC). Phase 2+. |

---

## What ships in sigil-mixer Phase 0 (today)

| File | LOC | Purpose |
|---|---|---|
| `commitment.rs` | 130 | `Commitment([u8; 32])` + `commit(value, token, blinding)`. Phase 0: BLAKE3 of inputs. Phase 1: real Pedersen on ark-bn254 G1 (homomorphic). |
| `nullifier.rs` | 110 | `Nullifier([u8; 32])` + `derive_nullifier(commitment, sk)`. BLAKE3 in P0, curve-friendly in P1. |
| `pool.rs` | 160 | `ShieldedPool { commitments, spent_nullifiers }`. BTreeSets in P0, flux-db CFs in P1, Sparse Merkle Tree in P2. |
| `tx.rs` | 195 | `ShieldedSendTxData` wire shape + `precheck()` for structural checks (sorted, non-empty, deterministic). |
| `verify.rs` | 130 | `verify_shielded_send(tx, pool)` — precheck + nullifier-freshness + ZK stub. |

26 unit tests green. **No `flux-zk-snark` dep yet** — Phase 0 stubs the ZK verification at module boundary so compile stays fast (9.6s). Real verification wires in P1.

---

## What rocky-sigil needs to change (proposal)

### sigil-tx (~120 LOC)

Replace `SigilTx::Send` with a new variant that wraps the mixer wire shape:

```rust
// sigil-tx::SigilTx
pub enum SigilTx {
    /// Private-by-default value transfer. Replaces the old transparent
    /// `Send`. No cleartext (from, to, amount) — only commitments +
    /// nullifiers + a ZK proof.
    ShieldedSend(sigil_mixer::ShieldedSendTxData),

    /// Same private-by-default treatment for Swap. The (in_token, in_amt,
    /// min_out) move into a ShieldedSwapTxData (new crate-level type) that
    /// also commits amounts. Phase 0 stub mirrors ShieldedSend's shape;
    /// the actual sigil-dex math integration uses commitment-arithmetic.
    ShieldedSwap(sigil_mixer::ShieldedSwapTxData),

    /// LP deposit/withdraw — same pattern. Pool shares become shielded
    /// commitments under a virtual "lp-shares-{pool_id}" token tag.
    ShieldedLpDeposit(sigil_mixer::ShieldedLpDepositTxData),
    ShieldedLpWithdraw(sigil_mixer::ShieldedLpWithdrawTxData),

    /// Operational, not value-bearing — stay cleartext:
    ContractCall { ... },
    ContractDeploy { ... },
    MintReward { ... },          // genesis / block-producer rewards;
                                  // miner address is public for accountability
    TokenDeployed { ... },
    ValidatorJoined { ... },
    ValidatorLeft { ... },
}
```

`SignedTx` wrapper unchanged — still has `from_pubkey`, `nonce`, `sig_scheme`, `sig`. The `from_pubkey` is the only public per-tx identity (signer); it does NOT reveal which commitments the signer is spending (the proof binds them anonymously).

**Breaking change to sigil-tx::apply_tx** — `Send`'s old `(from, to, amount, token, fee)` decomposition into `StateMutation::WalletDelta` becomes `ShieldedSend`'s `Vec<StateMutation::PoolInsert / PoolSpend>`. Proof verification is now mandatory before any state mutation (currently `verify_signature` is the only chokepoint).

### sigil-state (~150 LOC)

Replace per-wallet balance tracking with a shielded pool:

```rust
pub struct SigilState {
    // Old: wallets: BTreeMap<(WalletId, TokenId), u128>
    // New:
    pub(crate) shielded_pool: sigil_mixer::ShieldedPool,

    // Pools, contracts, events — unchanged except event payload schema
    pub(crate) pools: BTreeMap<PoolId, PoolState>,
    pub(crate) contracts: BTreeMap<(ContractId, SlotId), [u8; 32]>,
    pub(crate) block_events: Vec<[u8; 32]>,
}

pub struct StateRoots {
    /// Replaces wallet_state_root. Hash over (commitments_root, nullifiers_root).
    pub shielded_pool_root: [u8; 32],
    pub dex_state_root: [u8; 32],
    pub event_log_root: [u8; 32],
    pub contract_state_root: [u8; 32],
}

pub enum StateMutation {
    /// Insert a new commitment into the shielded pool.
    PoolInsert(sigil_mixer::Commitment),
    /// Mark a commitment as spent by its nullifier.
    PoolSpend(sigil_mixer::Nullifier),

    // ContractWrite / PoolDelta / etc. unchanged
}
```

Two derived APIs sigil-tx will use:
- `state.shielded_pool.contains_commitment(&c)` — for verification of input membership
- `state.commit_state_transition(StateTransition { mutations })` — atomic insert+spend per tx

### sigil-events (~30 LOC)

```rust
pub enum SigilEvent {
    // Old: Send { from, to, amount, token, tx_hash }
    // New:
    ShieldedSend {
        // From-pubkey is the only public identity; amounts hidden.
        from_pubkey: WalletId,
        n_inputs: u8,
        n_outputs: u8,
        token_hint: [u8; 32],  // empty if multi-token
        fee: u128,             // cleartext
        tx_hash: [u8; 32],
    },
    // Receive event drops entirely — recipient identity is hidden behind
    // commitments. Wallets scan commitments locally for ones they can
    // decrypt (stealth-address-style; Phase 2).

    // ...other variants similar private-fication for Swap/Lp
}
```

Light clients indexing wallet history walk the chain looking for commitments their viewing key can decrypt. Per-block lookup is O(commitments in the block), constant memory per wallet. This is roughly how Zcash sapling works.

---

## Phase rollout

| Phase | Scope | Crates touched | Status |
|---|---|---|---|
| **0 — scaffolding** | Wire shapes + ShieldedPool state + verify stub | `sigil-mixer` (new) | ✅ shipped today (26/26 tests) |
| **1 — real crypto** | Real Pedersen on ark-bn254. Real ZK verify via `flux-zk-snark::wallet_privacy::WalletPrivacyProver`. Stark variant via `flux-zk-stark::wallet_privacy_stark`. | sigil-mixer (this crate) | Next session, ~400 LOC |
| **1 — sigil-tx integration** | Replace `SigilTx::Send` with `ShieldedSend` variant. Same for Swap/Lp. | sigil-tx (rocky-sigil) | Next session, ~120 LOC |
| **1 — sigil-state integration** | Replace `wallets` map with `shielded_pool`. Adjust roots. | sigil-state (rocky-sigil) | Next session, ~150 LOC |
| **1 — sigil-events** | Strip cleartext fields. | sigil-events (rocky-sigil) | Next session, ~30 LOC |
| **2 — active mixing** | Port `q-quantum-mixing::mixing_pool` (565 LOC) — anonymity-set padding via decoy traffic. | sigil-mixer-coord (new) | After P1 settles |
| **2 — stealth addresses** | Port `q-quantum-mixing::stealth_addresses` (525 LOC) — unlinkable recipient addresses. | sigil-mixer-stealth (new) | After P1 settles |
| **3 — flux-db backing** | Move ShieldedPool from in-memory BTreeSet to flux-db CFs `shielded_commitments` + `shielded_nullifiers`. Sparse Merkle Tree for the commitments side. | sigil-mixer + sigil-state | After P5 (DEX) settles |
| **4 — recursive STARK** | Recursive proof folding over ranges of blocks for fast light-client wallet scan. | sigil-mixer | Stretch |

---

## Why this design

1. **Wire shape stable from P0.** sigil-tx + sigil-state can integrate today against `ShieldedSendTxData`. When real Pedersen lands in P1, the wire bytes don't change — only what's INSIDE the 32-byte commitment changes from BLAKE3-of-fields to a compressed curve point.

2. **No opt-in escape hatch.** There is no `SigilTx::TransparentSend` variant. Wallets that want transparent activity get it via a stealth address with a public viewing key — observers can derive the underlying activity, but only if the wallet explicitly publishes the key. Default is "nobody sees anything."

3. **Sum-conservation enforced by proof, not by validators trusting amounts.** Validators don't see amounts; they see a proof that `Σ input == Σ output + fee`. This is the SAME property the P3-D divergence-demo halts on if violated — except instead of comparing locally-computed roots, validators compare proof-claimed sum-conservation to the cleartext fee.

4. **Composes with flux-zk's existing stack.** `flux-zk-snark::wallet_privacy::TransactionPrivacyProof` is exactly the proof we need (`tx_commitment` + `nullifier` + ZK bytes). `flux-zk-stark::wallet_privacy_stark` gives the STARK variant for the tip-proof channel — recursive STARK proofs of a sequence of ShieldedSends compress to a single header attestation.

5. **The Quillon q-quantum-mixing port is staged.** All 29,893 LOC don't have to land at once. P1 needs `commitments.rs` (162 LOC) + `zkp_prover.rs` (800 LOC) — under 1K LOC. The rest (CLSAG ring sigs, lattice ring sigs, threshold pools, decoy engine, network resilience) layer on after the core works.

6. **Light-client friendly.** A wallet only needs to:
   - Pull each block's `output_commitments` list (~32 bytes per output)
   - Try to decrypt each with the wallet's viewing key (constant work per commitment)
   - Index the ones that decrypt; ignore the rest
   No need to download or scan amounts.

---

## Open questions for rocky-sigil

1. **`from_pubkey` field on ShieldedSend** — keep it (signature attribution) or strip it (full anonymity)? Strip means anyone can replay a ShieldedSend without permission — bad. I'd keep it. Sender's pubkey is the public identity; what they sent and to whom is hidden.

2. **Fee handling for `Swap` / `LP`** — pool fees vs cleartext fees. Phase 0 plan was cleartext fees. For Swap, fee is taken from `in_amt` (which is now hidden). Two options:
   - Cleartext flat fee, sender adds it via change commitment
   - Pool absorbs the fee internally; tx carries `fee_commitment` that pool-state arithmetic resolves
   I'd go cleartext for P1, commitment-based for P3.

3. **Genesis allocation** — DEMO_WALLET seeded with 1M SIGIL — does this become a commitment with publicly-known blinding (so anyone can verify the genesis allocation), or a commitment with a hidden blinding (auditable only to the DEMO_WALLET keyholder)? I'd go publicly-known blinding for genesis allocations + transparency.

4. **MintReward** — block-producer rewards. The producer's identity is public for accountability. Amount is the protocol constant, also public. Should the reward land as a CLEARTEXT credit (and then the producer privately "shields it" via a self-send), or as a SHIELDED commitment from genesis? I'd go cleartext credit → producer shields it themselves.

5. **Rollout strategy** — flag-day vs activation_height? Per `flux-consensus`'s height-gated upgrade rules, ShieldedSend lands at a specific block height; before that, transparent Send still works. After: rejected. Allows wallets time to upgrade. Same pattern as Quillon's mainnet phase transitions.

---

— rocky-updater, 2026-05-29
