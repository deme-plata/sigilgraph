# SIGIL Hardening Backlog

**Status: 2026-07-18 (ground-truthed against live code, not the audit).** Every item below
was re-verified against the current tree — the `SECURITY_AUDIT_2026-06-10.md` is **stale** and
several of its findings are already fixed. Verdicts: **FIXED** (done + tested), **PARTIAL**
(the primitive/groundwork landed but is not wired into the live consensus/apply/wire path — so
the remaining work is *wiring*, not *building*), **OPEN** (genuinely unaddressed).

The RPC edge is authenticated (wallet-signature + nonce, `authorize()` at
`crates/sigil-rpc/src/bin/sigil-rpcd.rs:682`). The real residual risk is deeper — consensus
crypto and the sync/follower ingress. Consensus/money-touching fixes are **branch-only** until
Alpha-Docker-tested and operator-approved (CLAUDE.md), and every validation change is
**height-gated** so historical blocks keep validating.

## Status at a glance

| Item | Verdict | One-line |
|---|---|---|
| H1 block-apply producer-sig | **PARTIAL** (fix on branch, dormant) | verification implemented height-gated + dormant (WS4); needs producer-side signing + registry + soak before activation |
| H7 future-ts bound on apply | **PARTIAL** (fix on branch, dormant) | LANE-R made block ts a money input; follower APPLY now bounds peer ts vs local clock, height-gated + dormant; see `docs/H7_TS_GUARD_GO_NO_GO.md` |
| C9 bridge mint | **FIXED** | deposit/withdrawal/LN all bound + spent-set + owner-sig; 25 tests green |
| H6 event_log_root | **FIXED** | positional binary Merkle both sides; inclusion roundtrip test green (audit premise stale) |
| C10 VDF modulus | **PARTIAL** | trustless `production()`/`rsa2048()` built + `assert_secure`, but consensus still calls `bench_2048()` |
| H5 state-layer nonce | **PARTIAL** | `check_and_bump_nonce` committed + signed, but zero callers in node/rpcd; legacy `SignedTx.nonce` still unchecked |
| snapshot-pull M2 | **PARTIAL** | trailer serves M1 `archive_root`; `anchor_sig`/`fold_blob` still empty |
| WS2 wallet_smt_root header commit | **PARTIAL** (read-path on branch) | proof-carrying `/balance` works; committing the SMT root in the header is the height-gated last inch |
| H2 verify-before-sync handshake | **PARTIAL** (real crypto + ingress gate on branch, log-only) | ed25519 verify fail-closed + sigil-node rr-backfill gate + channel binding; sigil-top attach + enforcement flip remain |
| H3 follower trusts peer difficulty | **OPEN** | follower adopts peer `bits`/`vdf_t` (reward is safe; easy share accepted) |
| H4 keyless wire tip-proofs | **OPEN** | always `Blake3Fingerprint`; no receiver-side verify; `new_sqisign` never on wire |

---

## FIXED (verified — do not re-work)

### C9 — bridge mint / withdrawal  ✅ FIXED
`crates/sigil-bridge/src/lib.rs`. `process_deposit` (`:82`) verifies the SPV proof first, binds
amount+recipient to the proven tx via `deposit_intent()` (not caller args), enforces a spent-set
(`is_spent`/`mark_spent`), gates to BTC-only, and takes a PoW difficulty floor. `process_withdrawal`
(`:124`) requires the owner's ed25519 signature over `(asset,amount,owner,dest,nonce)` + idempotent
`withdrawal id`. `process_ln_deposit` (`:146`) binds amount+payment_hash to the signed BOLT11 invoice
+ spent-set. **25 bridge tests green** (incl. `deposit_mints_amount_and_recipient_bound_to_the_proof`,
`unauthorized_withdrawal_is_rejected_collateral_untouched`). The audit's "unbounded / no replay / no
sig" is stale.

### H6 — event_log_root vs inclusion proofs  ✅ FIXED
The audit's "order-blind accumulator" premise is factually stale. `sigil-state::hash_event_log`
(`crates/sigil-state/src/lib.rs:1089`) builds a **positional balanced binary Merkle** tree
(duplicate-last-leaf padding, BLAKE3(l‖r)); `roots()` uses it at `:217`. `sigil-events`
(`prove_inclusion` `:379`, `verify_inclusion` `:416`) builds the **identical** tree with the same
padding + left/right rule. Roundtrip test `sigil-events:527` verifies every event's proof against a
root built by the real `commit_state_transition` path; `proof_rejects_tampered_event:546` confirms
rejection. Residual (minor): event ordering is positional-but-producer-chosen — the order itself
isn't independently constrained, but the structural mismatch is gone.

---

## PARTIAL (groundwork landed — the remaining work is WIRING)

### H1 — block-apply producer-signature verification  (fix on branch, DORMANT)
Verification implemented height-gated in WS4 (`sigil-header::verify_producer_sig` / `verify_at_height`,
`H1_PRODUCER_SIG_ACTIVATION_HEIGHT = u64::MAX`). 7 tests green. **Not deployed.** Before activation:
producer-side signing must ship (today the producer writes a zeroed sig), plus either move the hot
path to `Ed25519Hot` or add a validator registry for the PQ schemes, plus an Alpha-Docker soak. See
`docs/H1_PRODUCER_SIG_GO_NO_GO.md`. This remains the highest-severity item because it is not yet
enforcing.

### C10 — VDF modulus  (trustless modulus built, not wired)
`crates/flux-vdf/src/lib.rs:195` adds `rsa2048()` (RSA-2048 Factoring-Challenge semiprime, unknown
factorization / nothing-up-my-sleeve) + `production()` + `assert_secure()` (`:226`, rejects
small/even/prime/smooth N, fails closed). **But the live consensus + mining path still calls the
forgeable `bench_2048()`** (`sigil-rpcd.rs:291` follower apply, `:1215` `/mining/submit`, and every
miner: `flux-miner/src/engine.rs:105,294`, `client.rs:211,231`, the miner bins). The exploit is
therefore still live. **Fix = wire `production()` into every consensus/mining verify site**
(height-gated: pre-activation blocks were mined against `bench_2048`, so a clean cutover needs a
height boundary + coordinated miner upgrade). Small change, high blast radius → Alpha Docker first.
Test gate: a `bench_2048`-shortcut proof rejected under the production modulus; honest proof verifies.

### H5 — block/state-layer nonce replay  (primitive committed, not enforced live)
`sigil-state::check_and_bump_nonce` (`lib.rs:336`) stores a per-wallet high-water as a reserved
`NONCE_TOKEN` balance — so it rides `wallet_state_root` (committed) and a forged `SetBalance` to
`NONCE_TOKEN` is rejected (`:882`, `ReservedToken`). `sigil-tx::apply_signed_batch` (`:1360`) folds
nonce into the signed digest and rejects `nonce <= stored` (`NonceReplay`). Tested. **Gap:**
`apply_signed_batch`/`check_and_bump_nonce` have **zero callers in `sigil-node`/`sigil-rpcd`** — the
live money path never runs them; and the legacy `SignedTx.nonce` (the field the audit named) is still
neither signed nor checked (`sigil-tx:18-20`). **Fix = route the live apply path through
`apply_signed_batch`** (or call `check_and_bump_nonce` in the RPC apply). Test gate: a replayed
signed batch rejected at the live apply path.

### snapshot-pull M2 — signed anchor + SQIsign/fold trailer
`sigil-node/src/main.rs:1030` now serves a real M1 `archive_root` (BLAKE3 over the ordered
`SkeletonRecord`s, matching the client `SnapshotVerifier`). **But `anchor_sig` and `fold_blob` are
still empty** (`:1037`) — M1 structural only; fast-forward authenticates by hash, never
flux_fold-verifies. **Fix (two pieces):** (a) publish a real signed anchor (operator pins the
trust-root pubkey, `SIGIL_ANCHOR_PK_HEX` + DNS/URL); (b) fill the trailer with a SQIsign signature +
a real `flux_fold` checkpoint and verify the fold on fast-forward. Test gate: a wrong-signature
snapshot rejected; a tampered prefix caught by fold-verify.

### WS2 — wallet_smt_root header commitment (the proof-carrying `/balance` last inch)
Read-path realized on branch (WS2): `prove_balance()` + `/balance?proof=1` + `sigil-balance-verify`
(gates on `TipProofFlavor::adversary_resistant()`). The committed `wallet_state_root` is still the
additive accumulator; committing the **SMT** root in the header (so the tip-proof attests it) is the
height-gated Phase-3 swap the `roots()` doc anticipates. **Fix = add the SMT root to the header/roots
(height-gated), maintain the SMT incrementally in `set_balance`.** Test gate: a light client trusts a
`/balance?proof=1` response only after checking it against the tip-attested SMT root.

---

## OPEN (genuinely unaddressed)

### H2 — verify-before-sync handshake  🟡 PARTIAL (real crypto + ingress gate on branch, log-only)
**2026-07-19:** `verify_handshake` is REAL ed25519 over the canonical transcript, fail-closed
(Blake3Stub → `StubRejected`, unwired PQ algs → `UnsupportedAlgorithm`); `sign_with_ed25519` binds
the verifying key into the transcript. First live caller: `sigil-node/src/sync_auth.rs` gates the
rr-backfill serve path — the node mints a 12h `ValidatorPeer` handshake at boot (key
`<db_path>/handshake_ed25519.key`, sync-only, not a wallet key), attaches it to every outgoing
`BackfillReq`, and the server verifies + caches sessions with **channel binding**
(`session_pubkey` == the arriving libp2p peer id, so cross-peer replay fails inside the validity
window). Rollout-safe: log-only default with authed/anon/refused telemetry;
`SIGIL_HANDSHAKE_REQUIRE=1` refuses before any block transfer. 11/11 crate + 42/42 node tests.
**Remaining:** sigil-top client-side attach (monitor authenticates as `McpAgent`), then the
enforcement flip once fleet telemetry shows the followers authenticate. Test gate met in unit form
(wrong network / bad identity / cross-peer replay refused when enforcing); repeat live after the flip.

### H3 — follower trusts peer-declared difficulty  ⛔ OPEN
`sigil-rpcd.rs:249` (`apply_block`) reads `bits`/`vdf_t` straight from the peer's block JSON, builds
the `Challenge` with `target_from_bits(bits)` + that `vdf_t`, verifies the share against the
**peer-declared** target, then adopts it (`:281` `n.bits = bits;`) — explicit in-code "trust boundary"
(`:279`). Mitigation already present: the *reward* is independently recomputed (`:260`), so no
over-credit — but a malicious producer serving `bits=4` still gets an easy share accepted. The
self-mining `/mining/submit` path (`:1209`) is safe (server-derived target). **Fix = derive the
required target from chain state on the follower path too, and reject shares below it.** Test gate: an
under-difficulty peer share is not accepted on the follower path.

### H4 — keyless wire tip-proofs  ⛔ OPEN
`sigil-node/src/main.rs:1686,1750` always builds `TipProof::new_blake3`; the only `.verify()`
(`:1788`) is the producer's own dry-run self-check, not receiver ingress; `TOPIC_TIP_PROOFS` only
appears in the publish direction (`:1877`) with no verifying subscriber. `SqiSignBlob` exists as a
flavor (`sigil-tip-proof:52`) but `new_sqisign` is never called for tip-proofs. **Fix = produce +
verify `SqiSignBlob` tip-proofs on the wire; keep BLAKE3 integrity-only, never a trust root.** (This
is also what WS2's `/balance` verifier gates on via `adversary_resistant()`.) Test gate: a fabricated
BLAKE3 tip-proof fails the adversary-resistant gate; a valid SQIsign tip-proof passes.

---

## Notes on process
- **Every verdict here was read from the current tree.** When the audit and the code disagree, the
  code wins — re-verify before working an item (C9, H6 were both already done; C10/H5/M2 are
  further along than the audit implies).
- Consensus/money fixes: isolated `CARGO_TARGET_DIR`, Alpha Docker soak, operator go/no-go, and a
  height-gate on every validation change. `fluxc` only, never raw `cargo`.
- Branch for the 2026-07-18 isolation window: `hardening/ws-2026-07-18`.
