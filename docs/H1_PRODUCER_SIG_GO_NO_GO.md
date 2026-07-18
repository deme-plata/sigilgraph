# H1 — Producer-Signature Verification on Block Apply · Go/No-Go

**Branch:** `hardening/ws-2026-07-18` · **Status: implemented + unit-tested, NOT deployed.**
Prepared under the 2026-07-18 isolation-only window. This note is the operator decision
gate before H1 ever touches the live `sigil-g0` chain.

## What H1 is

Block-apply historically verified **no** producer signature: producers wrote
`producer_sig = vec![0u8; 292]` and nothing checked it, so any peer could forge a header
the network would apply (audit finding H1, the highest-severity open item). This change
adds the verification.

## What landed (this branch, dormant)

- `SigilBlockHeaderV0::verify_producer_sig()` — verifies the producer signature over the
  canonical `signing_bytes()`. `Ed25519Hot` is fully verifiable from the header (the 32-byte
  `producer` ValidatorId **is** the ed25519 pubkey); `SqiSign5`/`Dilithium5` fail closed with
  `ProducerPubkeyUnavailable` (their larger pubkeys aren't in the header — needs a registry).
- `SigilBlockHeaderV0::verify_at_height(apply_height)` — height-gated: below activation it is
  `precheck()` only (exact legacy behaviour, every historical block still validates); at/above
  it enforces the signature.
- `H1_PRODUCER_SIG_ACTIVATION_HEIGHT: u64 = u64::MAX` — **dormant.** Merging this code changes
  **nothing** on the live chain: the gate never fires until an operator sets a real height.
- 7 unit tests (all green): valid sig verifies; tampered/​wrong-key/​forged-zero-sig rejected;
  PQ scheme fails closed; gate below activation = legacy; gate at activation enforces.

## What is NOT done (required before activation — this is the "no-go until" list)

1. **Producer-side signing.** The live producer must actually sign headers with its key. Today
   it writes a zeroed sig. Until the producer signs, flipping the gate on would halt block
   production (every block would fail verification). **Wire signing first, deploy it, confirm
   real signatures on the wire, THEN schedule activation.**
2. **Validator registry for PQ schemes.** `SqiSign5`/`Dilithium5` fail closed here. If the live
   chain uses `SqiSign5` (it does — `fake_header` and the header default are `SqiSign5`), then
   H1 as written cannot verify the current producer even after it signs, because the SqiSign5
   pubkey isn't in the header. Options: (a) move the live producer to `Ed25519Hot` for the
   hot path (the header already supports it and calls it the "classical hot-path scheme"),
   or (b) add a validator registry / DNS-anchored pubkey lookup so SqiSign5/Dilithium5 pubkeys
   are resolvable. Pick one before activation.
3. **Alpha Docker soak.** Per CLAUDE.md, consensus changes test on a fresh Alpha Docker
   container first: bring up a producer that signs + a follower that verifies at an activation
   height, confirm production continues across the boundary, confirm a forged block is rejected.

## Recommended sequence (safe activation)

1. Wire producer-side signing (Ed25519Hot hot-path recommended — simplest sound path).
2. Deploy the signing producer; confirm real non-zero signatures on the wire for N blocks.
3. Alpha Docker: producer + follower, set a near-future activation height, watch the boundary.
4. Schedule `H1_PRODUCER_SIG_ACTIVATION_HEIGHT = current_height + safe_margin` (≥ a few
   thousand blocks, announced).
5. Deploy the verifying binary fleet-wide **before** the activation height.
6. At the height, verification activates automatically; no restart, no production pause.

## Rollback

Trivial while dormant: the branch is a pure no-op until the const changes. After scheduling,
rollback = deploy a binary with the const back at `u64::MAX` (or a higher height) before the
old activation height is reached — the 1024-block-style window applies.

## Decision

- [ ] **GO** — proceed with the sequence above (starts with producer-side signing, not
  activation).
- [ ] **NO-GO / hold** — keep dormant on the branch.

Verification of this deliverable: `fluxc test -p sigil-header` → 14/14 green
(7 H1 + 7 pre-existing), in an isolated `CARGO_TARGET_DIR`, nothing deployed.
