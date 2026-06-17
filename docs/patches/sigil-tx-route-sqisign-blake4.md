# Patch — SIGIL native transfer route `/tx`, SQIsign-authenticated, BLAKE4 tx-id

Fixes the wallet's **"✗ unknown route"** on send (wallet POSTs `/tx`; sigil-rpcd
has no such route — falls through to `bad("unknown route")` at
`crates/sigil-rpc/src/bin/sigil-rpcd.rs:1580`). Simultaneously closes the audit
finding *"sigil-rpcd money routes have zero auth"* by requiring a **SQIsign L5**
signature, and stamps each transfer with a **BLAKE4** tx-id.

Build on **Delta** (`fluxc build --package sigil-rpc`); never hot-swap money
routes onto live Epsilon until reviewed + the rest of the audit is addressed.

## Wire contract (matches the existing wallet)
`POST /tx`
```json
{
  "from":        "<64-hex sender pubkey-hash>",
  "to":          "16e0abea45d1dc4562f0f9f3f3d58aa7aa12a13a3732df88d7d2c1e969305976",
  "amount":      "1.00000000",
  "nonce":       <u64, per-sender monotonic — replay guard>,
  "sqisign_pk":  "<base64 SQIsign L5 public key, ~? bytes>",
  "sqisign_sig": "<base64 SQIsign L5 signature, 292 B at L5>"
}
```
Response: `{ "tx_id": "<blake4-hex>", "status": "applied", "height": <h> }`

## Handler sketch (add to the router match in sigil-rpcd.rs, before the `_ =>` arm)
```rust
"/tx" => {
    let req: TxReq = match serde_json::from_slice(&body) { Ok(r) => r, Err(e) => return bad(&format!("bad tx json: {e}")) };

    // 1. AUTH — SQIsign L5 over the canonical preimage. Closes the zero-auth gap.
    //    preimage = from ‖ to ‖ amount ‖ nonce  (domain-separated, fixed encoding)
    let preimage = tx_preimage(&req.from, &req.to, &req.amount, req.nonce);
    let pk  = b64(&req.sqisign_pk)?;     let sig = b64(&req.sqisign_sig)?;
    if !flux_sqisign::verify::<Level5>(&pk, &preimage, &sig) {   // L5 — see flux-sqisign size fix
        return bad("sqisign verification failed");
    }
    // the signing key must hash to `from` (binds sig → sender account)
    if blake4_hash(&pk) != hex_to_bytes(&req.from)? { return bad("pubkey != from"); }

    // 2. REPLAY — nonce must be strictly greater than the stored sender nonce.
    if req.nonce <= state.nonce_of(&req.from) { return bad("stale nonce"); }

    // 3. EXECUTE through the ONE cap-enforced money chokepoint (never touch
    //    balances directly — same path as submit_share/credit). Atomic debit+credit.
    let tx_id = blake4_hex(&[preimage.as_slice(), &now_le()].concat());  // BLAKE4 tx-id
    match sigil_rpc::execute_transfer(&state, &req.from, &req.to, &req.amount, req.nonce, &tx_id) {
        Ok(height) => { persist(&state); ok_json(&json!({"tx_id": tx_id, "status":"applied", "height": height})) }
        Err(e)     => bad(&format!("transfer rejected: {e}")),
    }
}
```

## Primitives (both already in the tree)
* **SQIsign L5** — `flux-sqisign` (`generate::<Level5>` / `verify::<Level5>`, 292 B sigs;
  see `project_flux_sqisign_size_overclaim` — must pass `Level5` explicitly, the
  default is L1/148 B). PQ-secure transfer authorization.
* **BLAKE4** — `flux_miner::pow` BLAKE3-compression at R=7 (KAT-verified ≡ BLAKE3;
  `sigil/docs/BLAKE4.md`). Used for the tx-id and the `pk → from` account binding.

## New types
```rust
#[derive(Deserialize)]
struct TxReq { from: String, to: String, amount: String, nonce: u64, sqisign_pk: String, sqisign_sig: String }
```
`execute_transfer` lives next to `credit_share` in `sigil-rpc` (the chokepoint),
adds: balance check, atomic debit/credit, nonce bump, append to the sigil-events
ledger. Cap/conservation invariants enforced exactly like the mining credit path.

## Why this is the right shape
* fixes the **live send** (route now exists, wallet unchanged);
* the route is **authenticated** (SQIsign) — anon callers can't move funds, unlike
  today's open money routes;
* **BLAKE4** binds sender identity (`pk→from`) and stamps an immutable tx-id;
* transfers flow through the **single** money chokepoint, so the conservation
  audit holds by construction.

## Test gate (run on Delta: `fluxc test --package sigil-rpc`)
1. valid signed transfer applies; balances move; nonce bumps; tx-id is BLAKE4(preimage‖ts).
2. wrong-key / tampered-amount sig → `sqisign verification failed`, no balance change.
3. replayed nonce → `stale nonce`.
4. `pk` not hashing to `from` → `pubkey != from`.
5. over-balance → `transfer rejected`, atomic (no partial debit).
