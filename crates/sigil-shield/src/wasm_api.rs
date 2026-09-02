//! Browser-callable bindings for a real, client-side shielded-to-shielded send
//! (PV-1 step 6, 2026-08-25).
//!
//! Everything in [`crate::wallet`] already builds and proves a spend natively; this
//! module is a thin `wasm-bindgen` skin over it so `gui/sigil-wallet-tron-embedded.html`
//! — a single self-contained HTML file with no bundler — can call the REAL zk-STARK
//! prover (`spend_full_v4`) instead of the JS-ported crypto that page already uses for
//! `doShield()`. Shielding needs no proof (a signed commitment is enough); paying a
//! third party privately does, and a browser cannot fake a STARK — this is what makes
//! that possible.
//!
//! # Why hex/decimal strings everywhere instead of native ints
//!
//! `wasm-bindgen` maps Rust `u64` to a JS `bigint`, which is correct but easy to trip
//! over from plain inline `<script>` code (no bundler, developers copy-pasting numbers).
//! Every amount crosses the boundary as a decimal string instead — the exact convention
//! this page's own `doShield()` already uses when it POSTs `value.toString()` to
//! `/v1/shield` (see that function's comment on why `sigil_state::u128_str` needs a JSON
//! string, not a number). Consistent boundary, one fewer footgun.
//!
//! # What this does NOT do
//!
//! It does not talk to the network. The caller (JS) fetches `/v1/shielded/leaves` for
//! the pool state and `/v1/shielded/address` for the recipient's published address,
//! passes the results in here, gets back a JSON string shaped exactly like
//! [`crate` sibling] `sigil_api::shielded::ShieldedSendRequest`, and POSTs that itself to
//! `/v1/shielded_send`. Keeping HTTP out of the wasm module is deliberate: it stays
//! testable without a mock server and reusable from a non-browser host later.

use wasm_bindgen::prelude::*;

use crate::note_cipher::{seal_note, NotePlaintext, ShieldedAddress};
use crate::note_v1::{from_wire, padding_leaf_wire, to_wire};
use crate::wallet::{build_spend, NoteStore, OwnedNote, ShieldedAccount};
use winterfell::math::fields::f64::BaseElement;

/// `sigil_state::shielded::SHIELDED_FEE` — a shielded send must pay exactly this (a
/// chosen fee is itself a fingerprint). Ported as a constant rather than pulled in via a
/// `sigil-state` dependency: `sigil-state` drags in the wider chain-state stack (flux-db
/// etc.) that has never been asked to compile to wasm32 and has no business inside a
/// browser tab. This mirrors how `sigil-wallet-tron-embedded.html`'s own `doShield()`
/// already ports `DENOMINATIONS` verbatim rather than depending on the crate that owns
/// it. If the server-side constant ever changes, this wasm module needs rebuilding
/// anyway (the circuit's public inputs would no longer match).
const SHIELDED_FEE: u64 = 1_000;

fn hex32(s: &str) -> Result<[u8; 32], JsValue> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| JsValue::from_str(&format!("bad hex: {e}")))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| JsValue::from_str("expected exactly 32 bytes"))?;
    Ok(arr)
}

fn parse_u64(s: &str, field: &str) -> Result<u64, JsValue> {
    s.parse::<u64>()
        .map_err(|_| JsValue::from_str(&format!("{field} must be a decimal integer, got {s:?}")))
}

/// This account's wire-hex `pk_shield` — the circuit key a payer binds an output note
/// to. Publish this (together with an X25519 encryption key) via
/// `POST /v1/shielded/register` so others can pay you privately.
#[wasm_bindgen(js_name = shieldPublicKey)]
pub fn shield_public_key(seed_hex: &str) -> Result<String, JsValue> {
    let account = ShieldedAccount::from_seed(hex32(seed_hex)?);
    Ok(hex::encode(to_wire(account.public_key())))
}

/// The X25519 note-delivery key ciphertexts are sealed to — the OTHER half of this
/// wallet's shielded address. `pk_shield` alone makes a wallet payable but not
/// notifiable: without publishing this key too, nothing can tell the wallet a payment
/// landed. The wallet page calls this during auto-registration.
///
/// 2026-08-29: this export existed only in an UNCOMMITTED working tree the 08-26 site
/// build was cut from — the page called it, the committed crate lacked it, and every
/// fresh WASM build silently broke registration. Re-added from the committed
/// `ShieldedAccount::address` derivation, the same one the node and MCP use.
#[wasm_bindgen(js_name = shieldEncryptPublicKey)]
pub fn shield_encrypt_public_key(seed_hex: &str) -> Result<String, JsValue> {
    let seed = hex32(seed_hex)?;
    let account = ShieldedAccount::from_seed(seed);
    Ok(account.address(&seed).pk_enc)
}

/// The commitment this account would publish for a SELF-CREATED note at `index` holding
/// `value` — deterministic from the seed alone. A wallet shields a deposit by computing
/// this locally (already done in JS on this page for `doShield()`'s fixed-denomination
/// path) and later needs the SAME value here to find that note's leaf position by
/// matching it against `GET /v1/shielded/leaves`'s `leaves` array — this function exists
/// so that lookup uses the real prover's own math rather than a second hand-ported copy.
#[wasm_bindgen(js_name = shieldNoteCommitment)]
pub fn shield_note_commitment(seed_hex: &str, index: u32, value_str: &str) -> Result<String, JsValue> {
    let account = ShieldedAccount::from_seed(hex32(seed_hex)?);
    let value = parse_u64(value_str, "value")?;
    let note = account
        .note(index as u64, value)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(hex::encode(to_wire(note.commitment())))
}

/// Build and PROVE a real shielded-to-shielded send: spend one self-created note,
/// paying `amount` to `recipient_pk_shield_hex`/`recipient_pk_enc_hex` (the recipient's
/// published shielded address — see `GET /v1/shielded/address`) with the rest returned
/// as a change note back to this account. Returns a JSON string with exactly the fields
/// `sigil_api::shielded::ShieldedSendRequest` expects, ready for
/// `POST /v1/shielded_send` with `Content-Type: application/json`.
///
/// # Parameters
/// - `seed_hex`: this wallet's 32-byte seed (same seed `doShield()` derives the spend
///   key and blinding from — `sha3_256(mnemonic)` on this page).
/// - `note_index` / `note_value_str`: the derivation index and value of the SELF-CREATED
///   note being spent (from this wallet's local note-index bookkeeping — see
///   `noteIndexNext`/`noteRecord` on this page).
/// - `note_position`: that note's leaf position in the pool, found by matching
///   [`shield_note_commitment`]'s output against `GET /v1/shielded/leaves`'s `leaves`.
/// - `unpadded_leaves_json`: the JSON array of hex commitment strings from that SAME
///   response's `leaves` field — the real, unpadded leaves only.
/// - `capacity`: that response's `capacity` field. Padding up to it is done HERE (via
///   the real [`padding_leaf_wire`], not a JS port) rather than shipped over the wire —
///   the server deliberately omits padding to avoid sending ~1MB of derivable zeros.
/// - `recipient_pk_shield_hex` / `recipient_pk_enc_hex`: the recipient's published
///   shielded address (`GET /v1/shielded/address?wallet=...` returns both).
/// - `amount_str`: how much to pay the recipient, in atomic units.
///
/// Errors surface as a JS exception carrying the underlying reason (non-conserving
/// amount, note not found at the claimed position, bad recipient key, etc.) — never a
/// silently wrong proof.
#[wasm_bindgen(js_name = buildPrivateSend)]
#[allow(clippy::too_many_arguments)]
pub fn build_private_send(
    seed_hex: &str,
    note_index: u32,
    note_value_str: &str,
    note_position: u32,
    unpadded_leaves_json: &str,
    capacity: u32,
    recipient_pk_shield_hex: &str,
    recipient_pk_enc_hex: &str,
    amount_str: &str,
) -> Result<String, JsValue> {
    build_private_send_with_memo(
        seed_hex, note_index, note_value_str, note_position, unpadded_leaves_json, capacity,
        recipient_pk_shield_hex, recipient_pk_enc_hex, amount_str, "",
    )
}

/// [`buildPrivateSend`] plus a private memo (UTF-8, at most `note_cipher::MEMO_LEN` = 512
/// bytes) sealed to the recipient alongside the note. A separate export rather than an
/// extra parameter so pages built against the memo-less signature keep working unchanged.
#[wasm_bindgen(js_name = buildPrivateSendWithMemo)]
#[allow(clippy::too_many_arguments)]
pub fn build_private_send_with_memo(
    seed_hex: &str,
    note_index: u32,
    note_value_str: &str,
    note_position: u32,
    unpadded_leaves_json: &str,
    capacity: u32,
    recipient_pk_shield_hex: &str,
    recipient_pk_enc_hex: &str,
    amount_str: &str,
    memo: &str,
) -> Result<String, JsValue> {
    let account = ShieldedAccount::from_seed(hex32(seed_hex)?);
    let note_value = parse_u64(note_value_str, "note_value")?;
    let amount = parse_u64(amount_str, "amount")?;

    // Rebuild the padded pool the chain is anchored on: real leaves as sent, then the
    // deterministic padding constant for every slot past what actually landed.
    let leaves_hex: Vec<String> = serde_json::from_str(unpadded_leaves_json)
        .map_err(|e| JsValue::from_str(&format!("bad unpadded_leaves_json: {e}")))?;
    if leaves_hex.len() as u32 > capacity {
        return Err(JsValue::from_str("more real leaves than the given capacity"));
    }
    let mut pool_commitments: Vec<[u8; 32]> = Vec::with_capacity(capacity as usize);
    for h in &leaves_hex {
        pool_commitments.push(hex32(h)?);
    }
    for i in pool_commitments.len() as u64..capacity as u64 {
        pool_commitments.push(padding_leaf_wire(i));
    }

    // A single-note local store: everything build_spend needs to know about the note
    // being spent, addressed by store position 0 (see wallet::build_spend's doc on why
    // it selects by store position, not derivation index).
    let blinding = account.blinding(note_index as u64);
    let mut store = NoteStore::new();
    store.notes.push(OwnedNote {
        index: Some(note_index as u64),
        value: note_value,
        blinding,
        position: Some(note_position as u64),
        spent: false,
        memo: None,
    });

    let recipient_pk: BaseElement = from_wire(&hex32(recipient_pk_shield_hex)?)
        .map_err(|e| JsValue::from_str(&format!("bad recipient_pk_shield_hex: {e}")))?;
    let my_pk = account.public_key();

    let change = note_value
        .checked_sub(SHIELDED_FEE)
        .and_then(|v| v.checked_sub(amount))
        .ok_or_else(|| {
            JsValue::from_str(&format!(
                "note value {note_value} cannot cover amount {amount} + fee {SHIELDED_FEE}"
            ))
        })?;

    let outs_spec = [(amount, recipient_pk), (change, my_pk)];
    let bundle = build_spend(&account, &mut store, &pool_commitments, 0, SHIELDED_FEE, &outs_spec)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    // Seal the recipient's output so they can discover it by trial-decryption — the
    // receiving gap `note_cipher` closes. The change output needs no ciphertext: this
    // account derived it itself (index recorded in `bundle.out_indices`), so it already
    // knows `(value, blinding)` without anyone telling it.
    let recipient_addr = ShieldedAddress::new(recipient_pk, recipient_pk_enc_hex);
    let (out0_value, out0_blinding) = bundle.out_preimages[0];
    let pt = NotePlaintext::new(out0_value, out0_blinding)
        .with_memo(memo)
        .map_err(|e| JsValue::from_str(&format!("memo rejected: {e}")))?;
    let ct = seal_note(&pt, &recipient_addr)
        .map_err(|e| JsValue::from_str(&format!("could not seal note to recipient: {e}")))?;

    let (change_value, change_blinding) = bundle.out_preimages[1];
    let change_index = bundle.out_indices.first().copied();

    let result = serde_json::json!({
        "anchor": hex::encode(bundle.anchor),
        "nullifier": hex::encode(bundle.nullifier),
        "cm_outs": [hex::encode(bundle.cm_outs[0]), hex::encode(bundle.cm_outs[1])],
        "fee": SHIELDED_FEE.to_string(),
        "proof": hex::encode(bundle.proof),
        "note_ciphertexts": [ct.0, serde_json::Value::Null],
        // Not part of the POST body — the caller (JS) uses these to book the change
        // note locally, same shape as this page's existing `noteRecord()`.
        "change_index": change_index,
        "change_value": change_value.to_string(),
        "change_blinding_hex": hex::encode(to_wire(change_blinding)),
    });
    Ok(result.to_string())
}

/// Trial-open ONE published note ciphertext with this seed's encryption key.
///
/// The receiving half of private payments, in the browser: a page walks every ciphertext
/// on chain, calls this, and each success is a note that is ours — value, blinding (so the
/// note can be spent) and the memo the sender wrote. Throws for any ciphertext not sealed
/// to us, which is the common case and carries no information.
#[wasm_bindgen(js_name = openNoteCiphertext)]
pub fn open_note_ciphertext(seed_hex: &str, ciphertext_json: &str) -> Result<String, JsValue> {
    let id = crate::note_cipher::enc_identity_from_seed(&hex32(seed_hex)?);
    let ct = crate::note_cipher::NoteCiphertext(ciphertext_json.to_string());
    let pt = crate::note_cipher::try_open_note(&ct, &id)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(serde_json::json!({
        "value": pt.value.to_string(),
        "blinding_hex": hex::encode(to_wire(pt.blinding)),
        "memo": pt.memo.text(),
    })
    .to_string())
}

/// Sanity/self-test entry point for the standalone WASM-loading smoke-test page: no
/// secrets, no proving, just confirms the module loaded and its exports are callable.
#[wasm_bindgen(js_name = wasmApiVersion)]
pub fn wasm_api_version() -> String {
    "sigil-shield-wasm-api/1".to_string()
}
