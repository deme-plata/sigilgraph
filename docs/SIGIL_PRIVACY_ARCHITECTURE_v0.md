# SIGIL private transfers — architecture v0 (target: Christmas mainnet)

Builds on the real, unfinished Quillon work (audit: `PRIVACY_CRYPTO_AUDIT_2026-07-21.md`).
Principle: take what's REAL, drop the theater, and commit privacy state into SIGIL's
existing four state roots so every node verifies the anonymity set identically.

## The honest building blocks (what's real vs stubbed)

| Piece | Source | Real? | PQ? | Role |
|---|---|---|---|---|
| **LatticeGuard** zk proof | `flux-lattice-guard` | ✅ base proof (folding = Phase C scaffold) | ✅ RLWE | **Validity** (hide amount, prove balance≥amount, well-formed output) |
| **Lattice ring signature** | `q-quantum-mixing/lattice_ring_sig.rs` | ✅ Dilithium-ring, FS-with-aborts | ✅ Module-LWE | **Sender anonymity** (post-quantum) |
| Stealth addresses | `q-quantum-mixing/stealth_addresses.rs` | ✅ but ristretto ECDH | ❌ pre-quantum | Recipient unlinkability — needs a PQ KEM swap (ML-KEM) |
| Shielded pool infra | `q-quantum-mixing/shielded_pool.rs` | Merkle+nullifier real; Pedersen commit STUB (`vec![0u8;32]`) | — | Note-commitment tree + nullifier set |
| CLSAG (ristretto) | `q-quantum-mixing/clsag.rs` | ✅ | ❌ pre-quantum | **Not used** — superseded by lattice ring sig |

Decision: **post-quantum end to end.** Use lattice_ring_sig (not CLSAG) for anonymity,
LatticeGuard for validity, and replace the stealth-address ristretto ECDH with an ML-KEM
(Kyber) KEM for the output-key exchange. This is the "lose what Quillon got wrong" move:
Quillon called it "quantum" while shipping curve25519.

## The shielded-transfer note model (Zcash-Sapling-shaped, PQ primitives)

- **Note** = (value, recipient one-time PQ pubkey, randomness). Committed as a **lattice
  commitment** `cm = Commit(value, pk_onetime, r)` (finish the shielded_pool Pedersen stub
  with LatticeGuard's `PolynomialCommitment`, so commitment + proof share one algebra).
- **Nullifier** `nf = PRF(nsk, note_position)` — revealed on spend, prevents double-spend,
  reveals nothing about which note.
- **Note-commitment tree**: append-only Merkle tree of all `cm`. Its root is the anonymity set.
- **Spend proof** (LatticeGuard): "I know a note in the tree at the committed root, its
  nullifier is `nf`, and Σinputs = Σoutputs (+fee)" — amounts stay hidden.
- **Sender-set hiding**: the spend is authorized by a **lattice ring signature** over a
  decoy set, so even the spender's account is one-of-many.
- **Recipient scanning**: each output carries an ML-KEM-encrypted note ciphertext the
  recipient trial-decrypts — no on-chain link to their address.

## Mapping into SIGIL's four state roots (`sigil-header/src/lib.rs`)

The header schema is committed in block 0 — **do NOT add a 5th root** (hard fork). Fold
shielded state into the existing four:

1. **`wallet_state_root`** (line 208) — currently transparent balances (SMT). Extend to an
   SMT over `{transparent balances} ∪ {shielded: note_cmt_tree_root, nullifier_set_root}`.
   This is the consensus home of the anonymity set + spent-set. Every validator recomputes
   it in the boot-time preflight (SIGIL rule 7) → divergence is impossible to hide (north-star #2).
2. **`event_log_root`** (line 212) — the per-block **output ciphertexts** (ML-KEM-encrypted
   notes) + **nullifier reveals** as typed events. Recipients scan here; auditors see that
   *a* spend happened (nullifier) without amounts/parties. Typed `ShieldedOutput` /
   `NullifierSpent` events in `flux-sigil-events`.
3. **`dex_state_root`** (line 210) — Phase 2: shielded swaps. Phase 1 leaves DEX transparent.
4. **`contract_state_root`** (line 214) — untouched; shielded transfer is protocol-level, not VM.
5. **`txs_merkle_root`** (line 221) — the shielded tx (LatticeGuard proof + lattice ring sig +
   ciphertexts) is a normal leaf here.
6. **`state_transition_proof: StarkProof`** (header field) — today the fake STARK. This is the
   natural home for the **aggregate validity proof** of the block's shielded transitions:
   replace it with the LatticeGuard verification result (or a recursive aggregate once
   folding Phase C lands). Until then it carries the per-tx LatticeGuard proofs' batch verify.

State writes go ONLY through `commit_state_transition()` (SIGIL rule 6) — the shielded pool
update (append cm, insert nf, recompute the two sub-roots) MUST route through that chokepoint,
audited by `flux_ai_audit`, or it can't reach release.

## Phased plan to Christmas (~5 months)

- **P1 — Primitives real & tested (Aug):** finish shielded_pool's Pedersen→lattice commitment;
  wire LatticeGuard `generate_proof` to a shielded-transfer circuit; port lattice_ring_sig
  into a `sigil-shield` crate. Acceptance: prove→verify round-trips AND **verify rejects a
  tampered/zeroed proof** (the test the old code never had).
- **P2 — Consensus integration (Sep–Oct):** wallet_state_root gains the shielded sub-tree via
  `commit_state_transition`; nullifier set persisted (own flux-db CF); event_log_root carries
  ciphertexts; preflight recomputes shielded roots. Chronos scenario: double-spend a nullifier
  → rejected; forged proof → rejected; divergence=0 across 2 producers.
- **P3 — Wallet + KEM (Oct–Nov):** ML-KEM output encryption + recipient scanning in the
  `[W]` wallet; stealth ristretto ECDH replaced. Shielded send/receive end-to-end on testnet.
- **P4 — Param pinning + audit (Nov–Dec):** pin RLWE/Module-LWE params to a stated, externally
  checked hardness estimate (LatticeGuard's are "provisional"); adversarial review; 48–72h
  Docker soak per the mainnet-safety protocol before it guards real value.

## Non-negotiables (the three things Quillon's code failed)
1. Proofs produced by a REAL prover (no `vec![0u8; N]`).
2. `verify` REJECTS a bad/zeroed proof — proven by a tamper test in CI.
3. Wired into the actual transfer path with a persistent nullifier set (no double-spend).

## Known gaps to close (tracked, not hidden)
- shielded_pool Pedersen commitment is a `vec![0u8;32]` stub → finish with lattice commitment.
- LatticeGuard folding/recursion is Phase-C scaffold → P1–P3 use single non-recursive proofs
  (fine per-tx); recursion is a throughput optimization for later, not a blocker.
- LatticeGuard + lattice_ring_sig security parameters are "provisional/estimated" → P4 pins them.
- stealth_addresses is pre-quantum ristretto → replace KEX with ML-KEM in P3.
