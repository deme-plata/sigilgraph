# SIGIL Hardening Backlog

**Status: 2026-07-18.** This is the precise, tracked list the whitepaper v1.1 "open" rung
points at. It supersedes the prose framing in `SECURITY_AUDIT_2026-06-10.md`, whose *theft*
findings (C1/C3/C5/H8) are fixed and enforcing — the RPC edge is now wallet-signature +
nonce authenticated (`authorize()` at `crates/sigil-rpc/src/bin/sigil-rpcd.rs:682`, ~21
gated routes). What remains is **deeper than the RPC edge**: consensus-layer signature
verification, the bridge, and network/peer trust. Ordered by severity.

Every item: severity · where · what's wrong · fix sketch · test gate. Nothing here is
deployed speculatively; consensus-touching fixes are branch-only until Alpha-Docker-tested
and operator-approved (CLAUDE.md balance-integrity rules).

---

## CRITICAL

### H1 — block-apply verifies no producer signature
- **Where:** `crates/sigil-header/src/lib.rs:270-305`, `chain.rs:86`, chronos `lib.rs:333`.
- **What's wrong:** blocks carry `producer_sig` but apply-path never verifies it; the field
  is written as `vec![0u8; 292]` (a zeroed SQIsign-length placeholder). Any peer can forge a
  block header the network will apply. This is the single highest-severity open item.
- **Fix sketch:** verify the producer's signature over the canonical header bytes on apply,
  **height-gated** (mainnet-safe upgrade pattern: `if height >= ACTIVATION { verify } else
  { legacy }`, so historical blocks still validate). Producer pubkey from the block's
  declared `producer` + a validator registry / DNS anchor. A height-gated implementation is
  drafted + unit-tested on branch `hardening/ws-2026-07-18` (WS4) — **not deployed**.
- **Test gate:** honest block verifies; forged-sig block rejected; pre-activation block still
  applies under legacy rules; activation-height boundary tested both sides.

### C9 — bridge mint is unbounded, no replay set, no withdrawal proof
- **Where:** `crates/sigil-bridge/src/lib.rs:66-101`.
- **What's wrong:** mint amount + recipient are not bound to the proven source tx; there is no
  spent-set so a proof can be replayed; withdrawal requires no proof/signature.
- **Fix sketch:** bind minted amount+recipient to the SPV-proven source output; maintain a
  persistent `spent_proofs` set keyed by source-txid:vout; require a signed withdrawal
  authorization. Gate the whole bridge behind an explicit feature until done — it should not
  be reachable on `sigil-g0` while unbounded.
- **Test gate:** replay of a used proof rejected; mint amount ≠ proven amount rejected;
  withdrawal without auth rejected; conservation across the bridge holds.

### C10 — VDF over a fixed known-structure modulus (forgeable)
- **Where:** `flux-vdf/src/lib.rs:179` (used at `sigil-rpcd.rs` mining paths).
- **What's wrong:** the Wesolowski VDF runs over `bench_2048`, a hardcoded modulus of known
  structure — an attacker who knows the factorization can shortcut the "sequential time"
  proof, defeating the Ω lane's whole purpose.
- **Fix sketch:** derive the modulus from a trustless/verifiable source (class-group VDF with
  no trapdoor, or an RSA-UFO / distributed-setup modulus). Coordinate with the mining
  difficulty path so honest miners re-derive it. Consensus-adjacent → Alpha Docker first.
- **Test gate:** VDF proof for a shortcut factorization rejected; honest proof verifies;
  timing scales with `T` as expected.

---

## HIGH

### H2 — verify-before-sync handshake is a length-only stub, never called
- **Where:** `crates/sigil-handshake/src/handshake.rs:142-168`.
- **What's wrong:** the "verify-before-sync" gate that should authenticate a peer before
  accepting its blocks is a length check and is not wired into the sync path → no peer auth.
- **Fix sketch:** implement the real handshake (peer identity + network-id + tip-proof
  challenge) and call it in the block-pack request path before accepting ranges.
- **Test gate:** peer with wrong network-id / bad tip-proof refused before any block transfer.

### H3 — follower trusts peer-declared difficulty
- **Where:** `crates/sigil-rpc/src/bin/sigil-rpcd.rs:150,172`.
- **What's wrong:** a follower credits full reward for a share whose `bits`/`vdf_t` were
  declared by the peer, so a cheap share can claim an expensive reward.
- **Fix sketch:** recompute/verify the required difficulty from chain state (not the peer's
  claim) before crediting; reject shares below the chain-derived target.
- **Test gate:** under-difficulty share credited 0; correct share credited exactly.

### H4 — wire tip-proofs are keyless (`Blake3Fingerprint` only)
- **Where:** `sigil-node/src/main.rs:1089`, `sigil-tip-proof`.
- **What's wrong:** tip-proofs are exchanged only in the integrity-only `Blake3Fingerprint`
  flavor; `verify_sqisign` exists but is never used on the wire, so any peer can fabricate a
  tip-proof. (This is also why WS2's proof-carrying `/balance` gates on
  `TipProofFlavor::adversary_resistant()` and refuses the BLAKE3 flavor.)
- **Fix sketch:** produce + verify `SqiSignBlob` (P4.1) tip-proofs on the wire; keep BLAKE3
  as integrity-only, never as a trust root.
- **Test gate:** fabricated BLAKE3 tip-proof does not satisfy the adversary-resistant gate;
  a valid SQIsign tip-proof does.

### H5 — no nonce/replay protection at the block/state layer
- **Where:** `crates/sigil-tx/*` (the RPC layer has `auth_nonces`; the block/state layer does
  not check `SignedTx.nonce`).
- **What's wrong:** `SignedTx.nonce` is neither signed nor checked at apply, so a tx included
  via the block path (not the RPC gate) could replay.
- **Fix sketch:** include nonce in the signed payload and enforce a per-wallet committed-nonce
  monotonic check inside `commit_state_transition`.
- **Test gate:** replayed tx at apply rejected; correct-nonce tx applied once.

### H6 — event_log_root is order-blind vs binary-Merkle inclusion proofs
- **Where:** `sigil-state:201` (accumulator) vs `sigil-events:295-372` (inclusion proofs).
- **What's wrong:** the committed `event_log_root` is an order-blind accumulator, but the
  inclusion-proof machinery assumes a positional binary Merkle tree → proofs don't validate
  against the committed root.
- **Fix sketch:** commit the event log as a positional binary Merkle root matching the
  inclusion-proof construction (or vice-versa); pick one and make both sides agree.
- **Test gate:** an event's inclusion proof verifies against the header `event_log_root`;
  a non-included event's proof fails.

---

## MEDIUM / STAGED

### Snapshot-pull M2 — signed anchor + SQIsign/fold trailer
- **Where:** `sigil-top/src/block_sync/{mod.rs,fetch.rs}` (client, coded), `sigil-node/src/main.rs`
  codec=2/3/4 responder (server, wired), `sigil-header` (shared wire types, byte-symmetric).
- **What's wrong:** the whole path is coded and byte-symmetric but **gated off**: no signed
  anchor is published (`SIGIL_ANCHOR_PK_HEX` unset, DNS `_sigil-tip` is a dead template) and
  the trailer serves **empty** `anchor_sig`/`fold_blob` (M1 structural-only). Fast-forward
  authenticates by hash equality, never flux_fold-verifies.
- **Fix sketch (two independent pieces):** (a) publish a real signed anchor (operator pins the
  trust-root pubkey, signs a tip anchor, publishes at `SIGIL_ANCHOR_URL` + DNS); (b) M2: fill
  the trailer with a SQIsign signature over the anchor + a real `flux_fold` `FoldCheckpoint`,
  and have `fast_forward_to_anchored_checkpoint` verify the fold, not just the hash.
- **Test gate:** node with the pinned pubkey accepts a valid signed-anchor snapshot and
  rejects a wrong-signature one; fold-verify catches a tampered prefix.

### sigil-top adaptive multi-substream scheduler — coded but dark
- **Where:** `sigil-top/src/block_sync/fetch.rs` (`adaptive_chunk`, `hedge_fanout`,
  `adaptive_inflight` — pure fns, unit-tested; not wired into `mod.rs`).
- **What's wrong:** the live node (`sigil-node`) already has the request-ahead window that
  fixed the sync caps; the `sigil-top` adaptive/hedged multi-substream scheduler is a separate
  policy layer that drives nothing yet.
- **Fix sketch:** wire the three pure fns into `mod.rs`'s refill loop (the aspirational
  "launch() wires these in 3 spots"); land the multi-substream request wire.
- **Test gate:** measured live sync improvement over the current request-ahead window, on the
  real node — not a microbench.

---

## Notes on process

- **Consensus/bridge fixes never cowboy prod.** H1/C9/C10/H2/H3 touch consensus or money
  conservation → implement + test in an isolated `CARGO_TARGET_DIR`, soak on Alpha Docker,
  operator go/no-go before any deploy to `sigil-g0`.
- **Height-gate every validation change** so historical blocks keep validating under the
  rules that were live when they were produced.
- Build with `fluxc` only, never raw `cargo`.
- The `/balance` proof route (WS2) and this backlog were produced under the 2026-07-18
  isolation-only hardening window on branch `hardening/ws-2026-07-18`.
