# SIGIL — the hard 21,000,000 supply cap + O(1) state roots

> Viktor directive (2026-05-31): "nodes at compile time check for correctness;
> enforce the 21M max supply rule at genesis with sqisign and what not."
> Shipped + measured. — rocky-sigil 🟣

## The cap

```
SIGIL_DECIMALS = 8
MAX_SUPPLY     = 21_000_000 × 10^8 = 2,100,000,000,000,000 base units   (compile-time const)
```

Native SIGIL supply can **never** exceed 21M. Not a runtime parameter, not a
genesis field that could be set wrong — a constant baked into every node binary,
enforced at four independent layers.

## Four enforcement layers (all live, `sigil-state`, 31/31 tests)

| layer | mechanism | what it stops |
|---|---|---|
| **1. compile-time** | `const _: () = assert!(MAX_SUPPLY == 2.1e15)` | a wrong edit to the cap **fails the build** — no node can even be produced with a bad cap |
| **2. consensus** | `commit_state_transition` rejects any block whose post-state `native_supply > MAX_SUPPLY` → `CommitError::SupplyCapExceeded` | every block, every node — no emission / coinbase / mint path (present or future) can inflate past 21M |
| **3. genesis** | the SQIsign-signed genesis allocation flows through the *same* cap-checked chokepoint at height 0 | a >21M genesis is rejected before block 0 can seal; the signed genesis hash binds the within-cap distribution |
| **4. provenance** | the binary's `fluxc .proof` attests the build (including the cap const); recorded in block 0's `fluxc_artifact_proof` | this is the "nodes check correctness at compile time" — peers verify the producer ran a binary whose provenance proof carries the right cap |

`native_supply` is tracked **O(1)** in `set_balance` (transfers net to zero —
only mints/burns move it). The cap check is a single `u128` compare per block.

### Test (`supply_cap_enforced_at_21m`)
- mint **exactly** 21M → allowed
- mint **+1 base unit** → `SupplyCapExceeded` (block rejected; producers commit on
  a scratch clone, so the good state is untouched)
- transfers preserve supply
- non-native tokens (wQUG, USDS, custom) are **not** bound by the SIGIL cap — they
  carry their own supply

## The companion fix: O(1) state roots (incremental multiset accumulator)

Enforcing the cap means tracking supply, which means touching the wallet-root
path — so the same work wired Stargate #1's **incremental multiset accumulator**
into the live commit path. `roots()` now returns the wallet root in **O(1)**
(`wallet_acc`, maintained per-mutation) instead of an O(state) rehash.

### Measured — `large_state.rs` (21M-capped, correctness asserted at every N)

```
  accounts   incr root   from-scratch    speedup     blk/s ceiling (rehash-every-block)
  1,000,000    0.163µs      296.2 ms    1,817,689×    3.4 /s
 10,000,000    0.163µs    3,151.9 ms   19,368,201×    0.3 /s
 50,000,000    0.163µs   15,149.1 ms   92,907,507×    0.1 /s
```

- `roots()` is **flat at 0.163µs** at any state size (O(1)); from-scratch is linear.
- At 50M accounts a rehash-every-block chain stalls at **0.1 blocks/sec**;
  incremental keeps roots in microseconds → unbounded.
- `incr_root == wallet_root_recompute()` verified at every N — no drift at 50M.
- This is the production shape (millions of accounts). The chronos batch-auth
  bench (10k accounts, ~1M ops/block) is the OPPOSITE shape and can't show it —
  there the per-mutation BLAKE3 leaf even slightly regresses (88k vs 98k TPS),
  because mutations ≫ state. Both numbers are honest; they measure different regimes.
- `wallet_root_recompute()` is also a real **audit/recovery path**: a node can
  recompute from scratch on boot to prove the incremental accumulator hasn't drifted.

## Code map
- `crates/sigil-state/src/lib.rs`: `SIGIL_DECIMALS`, `MAX_SUPPLY`, `const_assert`,
  `native_supply` field + `native_supply()` getter, `wallet_acc` accumulator +
  `acc_add/acc_sub/wallet_leaf/acc_to_root`, `wallet_root_recompute()`,
  `CommitError::SupplyCapExceeded`, the cap check in `commit_state_transition`.
- Tests: `supply_cap_enforced_at_21m`, `incremental_wallet_acc_never_drifts`.
- Harness: `crates/sigil-state/examples/large_state.rs`.
- Within-cap funding fixed in `sigil-chronos::throughput` (both harnesses).

## Open / next
- Extend the accumulator to `dex_state_root` + `contract_state_root` (same
  pattern; currently still O(pools)/O(contracts), which are small).
- `flux_ai_audit` MIR-time rule: flag any `SetBalance` that bypasses the
  supply-checked chokepoint (deepens layer 4 from "attest the build" to "prove
  the mint path is gated").
- Emission controller (the [[project_sigil_emission_oracle]] lane) plugs in
  UNDER this cap — it can schedule issuance but never exceed 21M.
