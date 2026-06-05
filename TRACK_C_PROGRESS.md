# SIGIL Track C — Progress

**Owner:** rocky-sigil (wallet `qnk7154…1ccb`)
**Status:** Phase 0 scaffold complete · 18 unit tests passing
**Spec source:** `SIGIL_GENESIS_v0.md` §2, §3, §4

## What shipped

### `flux-sigil-header` — block schema v0

`crates/flux-sigil-header/` · 372 LOC · **7 tests pass**

- `SigilBlockHeaderV0` struct with every field from §2 mandatory
- `NETWORK_ID = b"sigil-g0"` constant
- `HEADER_VERSION = 0` constant
- `SigScheme` enum (SqiSign5 default / Dilithium5 fallback) with `expected_sig_len()`
- `SqiSignature` wrapper around Vec<u8> with `SQISIGN_L5_LEN = 292` constant + validation constructors (`from_array`, `from_vec`, `as_bytes`, `is_well_formed`)
- Placeholders for `WesolowskiProof`, `StarkProof`, `ProofBundle` matching the field shapes their owner crates will define when ported
- `SigilBlockHeaderV0::hash()` — content-addressed BLAKE3 over canonical JSON
- `SigilBlockHeaderV0::signing_bytes()` — payload for `producer_sig` (zeroes out the sig field before serializing, no self-reference cycle)
- `SigilBlockHeaderV0::precheck()` — version + network + sig length + nonce length + VDF input consistency (no crypto yet)
- `HeaderError` enum covering all precheck failure modes

### `flux-sigil-state` — SMT chokepoint

`crates/flux-sigil-state/` · 365 LOC · **6 tests pass**

- `SigilState` struct with **all mutators `pub(crate)`** — rule #6 enforced at the API surface (flux_ai_audit will catch any cross-crate write bypass when it runs at MIR time)
- Four state containers: `wallets: BTreeMap<(WalletId, TokenId), u128>`, `pools: BTreeMap<PoolId, PoolState>`, `block_events: Vec<[u8;32]>`, `contracts: BTreeMap<(ContractId, SlotId), [u8;32]>`
- `StateRoots` struct — the four roots that go verbatim into the header
- `StateMutation` enum + `StateTransition` (batched, atomic)
- `commit_state_transition()` — **the only function that mutates state**, returns the post-transition roots, clears block-scoped event buffer after commit
- Read-only accessors: `balance_of`, `pool`, `contract_slot`
- Phase 0 root computation: BLAKE3 over sorted-serialized maps (will swap for real SMT with non-membership proofs in P3)
- Event log root: balanced binary Merkle, last-leaf padding for odd counts

### `flux-sigil-events` — typed event ledger

`crates/flux-sigil-events/` · 365 LOC · **5 tests pass**

- `SigilEvent` enum, 11 variants matching §4 (Send, Receive, SwapExecuted, LpDeposited, LpWithdrawn, ContractCall, ContractDeploy, MintReward, TokenDeployed, ValidatorJoined, ValidatorLeft)
- Stable, dense tag space (`tag() -> u8`) for the `events_by_type` flux-db CF
- `encode()` — deterministic serialization (canonical JSON in P0, bincode in P3)
- `leaf_hash()` — BLAKE3 of encoded bytes, what the SMT receives
- `MerkleProof` — position-bound inclusion proof
- `prove_inclusion()` — build proof from `(events, index)` matching the state machine's root construction (same last-leaf padding)
- `verify_inclusion()` — full roundtrip including tampered-event detection
- `ProofError` enum covering index-out-of-range, wrong-depth, root mismatch

## What's NOT in Phase 0 (queued for later phases)

| Item | Phase | Owner crate |
|---|---|---|
| Real SQIsign verification of `nonce_sqisign` + `producer_sig` | P1 | `flux-sqisign` (port from Quillon) |
| Real Wesolowski VDF verify | P1 | `flux-vdf` (port from Quillon) |
| Real STARK proof verify of `state_transition_proof` | P3 | `flux-zk-stark` (already in flux workspace ✓) |
| Real SMT with non-membership proofs over `flux-db` CFs | P3 | this crate + `flux-db` |
| bincode swap for `SigilEvent::encode` + header serialization | P3 | this crate + `flux-sigil-header` |
| `flux_ai_audit` MIR rule enforcing chokepoint | P2 | `flux-ai` rule |
| `fluxc_artifact_proof` verifier wired through `fluxc-core::provenance` | P1 | this crate calls fluxc-core |

## Compile + test verification

Ran from sigil workspace root:

```
cargo build  --package flux-sigil-header --package flux-sigil-state --package flux-sigil-events
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.99s

cargo test   --package flux-sigil-header --package flux-sigil-state --package flux-sigil-events
   18 passed; 0 failed; 0 ignored
```

Dep graph stays tight — only `serde`, `serde_json`, `blake3`, `thiserror` plus the inter-crate path deps (header → none, state → header, events → header+state). No PQ-crypto or RocksDB pulled in yet, so the workspace remains light enough for an LSP to live-check.

**Skill rule reminder:** scaffolding compilation was done on Epsilon (cargo-check only, ~7 s). Real production builds + provenance signing happen on Delta per skill rule #1 once the crates need PQ-crypto.

## Next slices for Track C

After P0 stabilizes:

1. Wire `flux-sqisign` (already in flux workspace) into `nonce_sqisign.verify(producer_pubkey)` — replaces the precheck's length-only check
2. Define `WalletId` / `PoolId` / `ContractId` as opaque newtypes with constant-time eq instead of bare `[u8; 32]` (Quillon foot-gun fix)
3. Define `BlockHash` similarly
4. Move from `BTreeMap` to a real Sparse Merkle Tree backed by `flux-db` CFs once flux-db audit lands
5. Add inclusion-proof generation tied to the SMT (not the in-memory Merkle) so wallets can verify against a header without holding the full event list
