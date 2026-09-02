// sigil-top/src/mine_local_api.rs — LOCAL-ONLY signing/shield endpoints for the embedded
// wallet server (v7.1.86, 2026-08-26).
//
// THE POINT (operator, verbatim): "when i press w and create wallet inside the enter
// sigil it should remember the important stuff so send just works of course" — no typed
// recovery phrase, ever, for a wallet opened via [W] on THIS box when `SIGIL_MINE_SEED`
// is already configured (the same 64-hex secret this process already uses to mine).
// `sigilgraph.org` (a remote server with zero access to this machine's environment)
// correctly keeps prompting — that split is intentional, not a bug.
//
// WHY THIS ISN'T JUST "return a signature": Shield and the private-send feature don't
// only need a signature over a final message — the raw seed is needed to construct the
// actual private note client-side first (spend key, per-note blinding, the commitment
// itself; see `gui/sigil-wallet-tron-embedded.html`'s `doShield`/`doPrivateSend`). Swap
// and Bridge, by contrast, only ever call `window.sigilSign(priv, action, fields, nonce)`
// — nothing else touches the private key — so those two get the simple "just sign it"
// treatment. Because `sigil-top` is a native Rust program and the SAME crypto the
// browser's WASM prover is compiled from (`sigil-shield`) is available to it directly —
// no wasm boundary needed server-side — this module can perform Shield and
// private-send's ENTIRE operation itself: derive, construct, prove, sign, submit.
//
// ENDPOINTS (see `serve.rs`'s `handle_conn` for the dispatch — checked BEFORE the
// generic `/api/`/`/v1/` local_api/proxy block, since these are POST mutations with a
// JSON body that must NEVER be forwarded to a remote node):
//   POST /api/v1/mine-shield        — full local execution (derive + build notes + sign
//                                      + submit each denomination part to /v1/shield).
//   POST /api/v1/mine-send-private  — full local execution (build + PROVE a real
//                                      spend_full_v4 STARK natively + submit to
//                                      /v1/shielded_send). Can only spend a note this
//                                      endpoint is TOLD about (the `notes` field, mirroring
//                                      the browser's own localStorage bookkeeping) — it has
//                                      no server-side note store of its own beyond the
//                                      index counter `mine-shield` persists.
//   POST /api/v1/mine-sign          — simple signature over the SAME canonical message
//                                      `window.sigilSign` builds
//                                      (`sigil-rpc/v1|{action}|{fields.join('|')}|nonce=`),
//                                      for Swap and Bridge.
//
// SECRET-SAFETY INVARIANT (audited): `SIGIL_MINE_SEED` and every value derived from it
// that isn't meant to be public (the raw seed, the shield spend key, a note's blinding,
// its plaintext value/blinding pair as sealed to a THIRD party's key) never crosses the
// HTTP boundary in any response here. Every JSON response below carries only public
// outcomes: a wallet address, a signature, a commitment/anchor/nullifier hex, a txid, an
// index (a monotonic counter value, not a secret), or an error string. If you add a
// field to any response here, check this paragraph again before you do.
//
// AVAILABILITY CONTRACT: any endpoint here returns `404 Not Found` (or, once past that
// gate, an HTTP 200 body `{"ok":false,"error":"..."}`) whenever it cannot complete
// entirely locally. The caller (this page's JS) treats EITHER as "not available right
// now" and falls straight through to the existing recovery-phrase-prompt flow,
// unchanged — this module is a strictly additive fast path, never a required step.

/// `sigil-rpc/v1|{action}|{fields}|nonce={nonce}` — a signature-only local endpoint.
///
/// Covers Swap and Bridge: both only ever call `window.sigilSign(priv, action, fields,
/// nonce)` with no other use of the private key (confirmed by reading `doSwap`/
/// `doBridgeLock` in the wallet HTML — neither touches `sigil-shield` at all), so signing
/// with the LOCAL seed and handing back `(address, signature)` is the complete operation.
/// Unconditional (no `shield-register` gate): this only needs `sigil_oauth::Keypair`,
/// already a non-optional dependency.
fn handle_mine_sign(body: &str) -> (&'static str, String) {
    #[derive(serde::Deserialize)]
    struct Req {
        action: String,
        fields: Vec<String>,
        nonce: u64,
    }

    let Some(kp) = crate::miner_keypair() else {
        return not_available("no local mining seed configured (SIGIL_MINE_SEED unset)");
    };
    let req: Req = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return bad_request(format!("bad request body: {e}")),
    };
    // Byte-for-byte the same message `window.sigilSign` builds — this signature verifies
    // wherever the browser's own client-side signature would have (sigil-api's dex.rs /
    // bridge.rs / shielded.rs each rebuild this exact string to check it).
    let msg = format!(
        "sigil-rpc/v1|{}|{}|nonce={}",
        req.action,
        req.fields.join("|"),
        req.nonce
    );
    let sig = kp.sign(msg.as_bytes());
    ok_json(serde_json::json!({
        "ok": true,
        "address": kp.pubkey_hex(),
        "signature": hex::encode(sig),
    }))
}

#[cfg(feature = "shield-register")]
mod shield_ops {
    //! Full local execution for Shield and private-send — needs `sigil-shield`'s real
    //! note-construction/proving math, so this submodule only exists in a
    //! `shield-register` build (default-on; see the crate's Cargo.toml).

    use ed25519_dalek::{Signer, SigningKey};
    use sigil_shield::note_cipher::{seal_note, NotePlaintext, ShieldedAddress};
    use sigil_shield::note_v1::{from_wire, padding_leaf_wire, to_wire};
    use sigil_shield::wallet::{build_spend, NoteStore, OwnedNote, ShieldedAccount};
    use winterfell::math::fields::f64::BaseElement;

    use super::{bad_request, not_available, ok_json};

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn hex_decode32(s: &str) -> Option<[u8; 32]> {
        let v = hex::decode(s.trim().trim_start_matches("0x")).ok()?;
        v.try_into().ok()
    }

    /// Persistent per-wallet note-derivation-index counter, mirroring the browser's own
    /// `localStorage['sigil-shielded-noteidx-'+addr]` scheme (see the wallet HTML's
    /// `noteIndexNext`) but on THIS box's filesystem, so repeated `mine-shield` calls
    /// (even across `sigil-top` restarts) never reuse an index for a DIFFERENT note —
    /// reusing one would reuse that note's `blinding` too (`derive(seed, "blinding",
    /// index)` is a pure function of index), which doesn't break fund safety (nullifiers
    /// are bound to leaf POSITION, not index) but does needlessly weaken the note's
    /// per-instance randomness. NOT coordinated with the browser's OWN localStorage
    /// counter for the same wallet — a wallet that shields BOTH via this fast path and
    /// via the manual browser prompt could in principle allocate the same index from two
    /// independent counters; low-stakes (same non-issue as above) and out of scope to
    /// fully unify without a shared server<->browser index-allocation protocol.
    fn note_index_path(addr: &str) -> String {
        format!("{}/.flux/sigil-shield-noteidx-{addr}", crate::flux_home())
    }
    fn load_note_index(addr: &str) -> u64 {
        std::fs::read_to_string(note_index_path(addr))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
    fn save_note_index(addr: &str, next: u64) {
        let _ = std::fs::create_dir_all(format!("{}/.flux", crate::flux_home()));
        let _ = std::fs::write(note_index_path(addr), next.to_string());
    }

    /// Serializes concurrent `mine-shield` calls for the SAME process so two overlapping
    /// requests can never read-then-write the index counter file and both land on the
    /// same index (a local single-operator tool, so contention is not expected — this is
    /// cheap insurance, not a load-bearing lock).
    static NOTE_INDEX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `POST /api/v1/mine-shield` — body `{"amount": "<raw units, base-10 string>"}`.
    ///
    /// Full local execution: derive the shielded spend key from `SIGIL_MINE_SEED`,
    /// decompose `amount` into the chain's standard ramp denominations (mirrors
    /// `sigil_state::shielded::decompose`, the SAME split `sigil-api`'s
    /// `submit_shield_split` and this page's `doShield()` already use), build + sign one
    /// real `Shield` request per part (`account.note(index, value)` — identical math to
    /// `sigil_shield::wallet::shield_note`, just without needing an in-memory `NoteStore`
    /// since the index comes from the persistent counter above), and submit each to this
    /// node's `/v1/shield`. Returns `{"ok":true,"landed":[{"txid","value","index"},...]}`
    /// once ANY part lands — including a `"warning"` field if a LATER part then failed,
    /// so the caller can show a truthful partial receipt instead of silently discarding
    /// already-shielded value (falling back to the manual flow after a partial success
    /// would re-attempt the WHOLE amount from scratch and double-shield the landed part).
    pub fn handle_mine_shield(body: &str) -> (&'static str, String) {
        #[derive(serde::Deserialize)]
        struct Req {
            amount: String,
        }

        let Some(seed) = crate::miner_seed() else {
            return not_available("no local mining seed configured (SIGIL_MINE_SEED unset)");
        };
        let req: Req = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => return bad_request(format!("bad request body: {e}")),
        };
        let amount: u128 = match req.amount.trim().parse() {
            Ok(a) if a > 0 => a,
            _ => {
                return bad_request(
                    "amount must be a positive base-10 integer string (raw units)".into(),
                )
            }
        };
        let parts = match sigil_state::shielded::decompose(amount) {
            Some(p) => p,
            None => {
                return bad_request(format!(
                    "{amount} does not decompose into standard shielded denominations"
                ))
            }
        };
        let total_parts = parts.len();

        // Bit-for-bit `sigil_oauth::Keypair::from_seed(&seed)` (verified by
        // `shield_setup.rs`'s own `wallet_derivation_matches_the_existing_sigil_mine_seed_
        // convention` test) — the exact same wallet `miner_keypair()` mines to.
        let sk = SigningKey::from_bytes(&seed);
        let from = hex::encode(sk.verifying_key().to_bytes());
        let account = ShieldedAccount::from_seed(seed);

        let node = crate::engine_node_url();
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => return bad_request(format!("http client init failed: {e}")),
        };

        let _guard = NOTE_INDEX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut index = load_note_index(&from);
        let mut landed: Vec<serde_json::Value> = Vec::new();
        let mut failure: Option<String> = None;

        for value128 in parts {
            let value = value128 as u64; // denominations top out at 5e15, well within u64
            let this_index = index;
            index += 1;

            let note = match account.note(this_index, value) {
                Ok(n) => n,
                Err(e) => {
                    failure = Some(format!("note build failed: {e}"));
                    break;
                }
            };
            let cm_hex = hex::encode(to_wire(note.commitment()));
            let fee: u128 = 0;
            // Strictly increasing per part, same idiom as the browser's `Date.now()+i`.
            let req_nonce = now_ms() + this_index;
            let msg =
                format!("sigil-rpc/v1|shield|{from}|{value}|{cm_hex}|{fee}|nonce={req_nonce}");
            let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
            let payload = serde_json::json!({
                "from": from,
                "amount": value.to_string(),
                "cm": cm_hex,
                "fee": fee.to_string(),
                "sig": sig,
                "req_nonce": req_nonce,
            });
            let url = format!("{}/v1/shield", node.trim_end_matches('/'));
            match client.post(&url).json(&payload).send() {
                Ok(r) => {
                    let parsed: serde_json::Value =
                        r.json().unwrap_or_else(|_| serde_json::json!({}));
                    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                        let txid = parsed
                            .get("txid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        landed.push(
                            serde_json::json!({"txid": txid, "value": value, "index": this_index}),
                        );
                    } else {
                        let reason = parsed
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string();
                        failure = Some(reason);
                        break;
                    }
                }
                Err(e) => {
                    failure = Some(format!("network error: {e}"));
                    break;
                }
            }
        }

        // Persist however far we actually got — never reuse an index, whether we
        // succeeded, partially succeeded, or failed on the very first part.
        save_note_index(&from, index);
        drop(_guard);

        if landed.is_empty() {
            return ok_json(serde_json::json!({
                "ok": false,
                "error": failure.unwrap_or_else(|| "no parts landed".into()),
            }));
        }
        let landed_count = landed.len();
        let mut v = serde_json::json!({ "ok": true, "landed": landed });
        if let Some(reason) = failure {
            v["warning"] = serde_json::json!(format!(
                "partial — {landed_count}/{total_parts} parts landed, then: {reason}"
            ));
        }
        ok_json(v)
    }

    /// `POST /api/v1/mine-send-private` — body:
    /// ```json
    /// {"recipient_pk_shield":"<hex64>","recipient_pk_encrypt":"<hex64>",
    ///  "amount":"<raw units string>","notes":[{"index":0,"value":"..."},...]}
    /// ```
    /// `notes` is exactly this wallet's own candidate note list — the SAME
    /// `{index, value}` pairs the browser already keeps in
    /// `localStorage['sigil-shielded-notes-'+addr]` (see `myNotes()`/`noteRecord()` in
    /// the wallet HTML). This endpoint has no note store of its own beyond what
    /// `mine-shield` persists as an index counter (not a value list), so it can only
    /// spend a note the CALLER tells it about — a note shielded manually before this
    /// endpoint existed is invisible here, which is fine: that's just one more reason
    /// this call can fail and fall through to the manual flow, not a foreclosed case.
    ///
    /// Builds + PROVES a real `spend_full_v4` STARK natively — the exact same
    /// `sigil_shield::wallet::build_spend` the WASM module
    /// (`crates/sigil-shield/src/wasm_api.rs::build_private_send`) calls from the
    /// browser, ported here without the wasm boundary (native builds never compile that
    /// module — see `sigil-shield/src/lib.rs`'s `#[cfg(target_arch = "wasm32")]` gate —
    /// so this is a parallel native call site, not a shared function, but it mirrors that
    /// function's logic step for step, including its own quirk: `build_spend`'s two
    /// OUTPUT notes are allocated from a FRESH, single-call `NoteStore` (indices 0/1
    /// every time), same as the browser's WASM path already does — this endpoint doesn't
    /// invent stricter bookkeeping the manual path doesn't already have).
    #[allow(clippy::too_many_arguments)]
    pub fn handle_mine_send_private(body: &str) -> (&'static str, String) {
        #[derive(serde::Deserialize)]
        struct CandidateNote {
            index: u64,
            value: String,
        }
        #[derive(serde::Deserialize)]
        struct Req {
            recipient_pk_shield: String,
            recipient_pk_encrypt: String,
            amount: String,
            notes: Vec<CandidateNote>,
            /// Optional private message, sealed to the recipient with the note.
            #[serde(default)]
            memo: String,
        }

        let Some(seed) = crate::miner_seed() else {
            return not_available("no local mining seed configured (SIGIL_MINE_SEED unset)");
        };
        let req: Req = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => return bad_request(format!("bad request body: {e}")),
        };
        let amount: u64 = match req.amount.trim().parse() {
            Ok(a) if a > 0 => a,
            _ => {
                return bad_request(
                    "amount must be a positive base-10 integer string (raw units)".into(),
                )
            }
        };
        let Some(recipient_pk_bytes) = hex_decode32(&req.recipient_pk_shield) else {
            return bad_request("recipient_pk_shield must be 64 hex chars".into());
        };
        let recipient_pk: BaseElement = match from_wire(&recipient_pk_bytes) {
            Ok(v) => v,
            Err(e) => return bad_request(format!("bad recipient_pk_shield: {e}")),
        };

        let account = ShieldedAccount::from_seed(seed);
        let my_pk = account.public_key();
        let fee = sigil_state::shielded::SHIELDED_FEE as u64;

        let node = crate::engine_node_url();
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => return bad_request(format!("http client init failed: {e}")),
        };

        // Fetch the pool's real (unpadded) leaves FRESH — proving against a stale anchor
        // produces a proof the node will correctly reject.
        let leaves_url = format!("{}/v1/shielded/leaves", node.trim_end_matches('/'));
        let leaves_json: serde_json::Value = match client
            .get(&leaves_url)
            .send()
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => match r.json() {
                Ok(v) => v,
                Err(e) => return bad_request(format!("bad leaves response: {e}")),
            },
            Err(e) => return bad_request(format!("could not fetch the shielded pool: {e}")),
        };
        let Some(leaves_hex) = leaves_json
            .get("leaves")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
        else {
            return bad_request("leaves response missing 'leaves'".into());
        };
        let capacity = leaves_json
            .get("capacity")
            .and_then(|v| v.as_u64())
            .unwrap_or(sigil_state::shielded::POOL_CAPACITY as u64) as usize;

        // Pick the smallest of OUR OWN candidate notes that covers amount+fee AND has
        // actually landed (its commitment is present in the live leaf set) — same
        // "prefer the smallest covering note" policy `doPrivateSend()` uses.
        let mut best: Option<(u64, u64, usize)> = None; // (index, value, position)
        for cand in &req.notes {
            let Ok(value) = cand.value.trim().parse::<u64>() else {
                continue;
            };
            if value < amount.saturating_add(fee) {
                continue;
            }
            let Ok(note) = account.note(cand.index, value) else {
                continue;
            };
            let cm_hex = hex::encode(to_wire(note.commitment()));
            let Some(pos) = leaves_hex.iter().position(|c| c.eq_ignore_ascii_case(&cm_hex)) else {
                continue;
            };
            let better = match &best {
                Some((_, best_value, _)) => value < *best_value,
                None => true,
            };
            if better {
                best = Some((cand.index, value, pos));
            }
        }
        let Some((note_index, note_value, position)) = best else {
            return bad_request(format!(
                "no locally-known landed note covers {amount} + fee {fee}"
            ));
        };

        let Some(change) = note_value.checked_sub(fee).and_then(|v| v.checked_sub(amount)) else {
            return bad_request(format!(
                "note value {note_value} cannot cover amount {amount} + fee {fee}"
            ));
        };

        let mut pool_commitments: Vec<[u8; 32]> = Vec::with_capacity(capacity);
        for h in &leaves_hex {
            match hex_decode32(h) {
                Some(b) => pool_commitments.push(b),
                None => return bad_request("bad leaf hex from node".into()),
            }
        }
        for i in pool_commitments.len() as u64..capacity as u64 {
            pool_commitments.push(padding_leaf_wire(i));
        }

        // A single-note local store, exactly `wasm_api.rs::build_private_send`'s own
        // approach: everything `build_spend` needs to know about the note being spent,
        // addressed by store position 0.
        let blinding = account.blinding(note_index);
        let mut store = NoteStore::new();
        store.notes.push(OwnedNote {
            index: Some(note_index),
            value: note_value,
            blinding,
            position: Some(position as u64),
            spent: false,
            memo: None,
        });

        let outs_spec = [(amount, recipient_pk), (change, my_pk)];
        let bundle = match build_spend(
            &account,
            &mut store,
            &pool_commitments,
            0,
            fee,
            &outs_spec,
        ) {
            Ok(b) => b,
            Err(e) => return bad_request(format!("could not build the private spend: {e}")),
        };

        let recipient_addr = ShieldedAddress::new(recipient_pk, &req.recipient_pk_encrypt);
        let (out0_value, out0_blinding) = bundle.out_preimages[0];
        let pt = match NotePlaintext::new(out0_value, out0_blinding).with_memo(&req.memo) {
            Ok(p) => p,
            Err(e) => return bad_request(format!("memo rejected: {e}")),
        };
        let ct = match seal_note(&pt, &recipient_addr) {
            Ok(c) => c,
            Err(e) => return bad_request(format!("could not seal the note to the recipient: {e}")),
        };
        let change_index = bundle.out_indices.first().copied();
        let (change_value, _change_blinding) = bundle.out_preimages[1];

        let payload = serde_json::json!({
            "anchor": hex::encode(bundle.anchor),
            "nullifier": hex::encode(bundle.nullifier),
            "cm_outs": [hex::encode(bundle.cm_outs[0]), hex::encode(bundle.cm_outs[1])],
            "fee": fee.to_string(),
            "proof": hex::encode(bundle.proof),
            "note_ciphertexts": [ct.0, serde_json::Value::Null],
        });
        let send_url = format!("{}/v1/shielded_send", node.trim_end_matches('/'));
        let resp = match client.post(&send_url).json(&payload).send() {
            Ok(r) => r,
            Err(e) => return bad_request(format!("network error submitting the private send: {e}")),
        };
        let parsed: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return bad_request(format!("bad response from node: {e}")),
        };
        if parsed.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let reason = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return bad_request(reason);
        }
        let txid = parsed
            .get("txid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        ok_json(serde_json::json!({
            "ok": true,
            "txid": txid,
            "spent_index": note_index,
            "change_index": change_index,
            "change_value": change_value.to_string(),
        }))
    }
}

fn ok_json(v: serde_json::Value) -> (&'static str, String) {
    ("200 OK", v.to_string())
}

fn not_available(reason: &str) -> (&'static str, String) {
    (
        "404 Not Found",
        serde_json::json!({ "ok": false, "error": reason }).to_string(),
    )
}

fn bad_request(reason: String) -> (&'static str, String) {
    // Deliberately HTTP 200 (not 4xx): the request reached this box and was understood,
    // it just couldn't be completed locally (bad amount, no covering note, the real node
    // rejected it, ...). The caller only needs to distinguish "understood this JSON body
    // but the operation failed" from `not_available`'s "this feature doesn't exist here
    // at all" — both make the JS fall back to the manual flow, but 200-vs-404 is what a
    // browser's own network layer distinguishes cheaply (`fetch(...).ok`).
    (
        "200 OK",
        serde_json::json!({ "ok": false, "error": reason }).to_string(),
    )
}

/// True for any path `serve.rs` should route to [`handle`] instead of the generic
/// `/api/`/`/v1/` local_api/proxy dispatch.
pub fn is_local_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/mine-shield" | "/api/v1/mine-sign" | "/api/v1/mine-send-private"
    )
}

/// Dispatch one of the three local-only endpoints. `body` is the raw request body
/// (already extracted by `serve.rs`'s `handle_conn`).
pub fn handle(path: &str, body: &str) -> (&'static str, String) {
    match path {
        "/api/v1/mine-sign" => handle_mine_sign(body),
        #[cfg(feature = "shield-register")]
        "/api/v1/mine-shield" => shield_ops::handle_mine_shield(body),
        #[cfg(not(feature = "shield-register"))]
        "/api/v1/mine-shield" => {
            not_available("this build was compiled without the shield-register feature")
        }
        #[cfg(feature = "shield-register")]
        "/api/v1/mine-send-private" => shield_ops::handle_mine_send_private(body),
        #[cfg(not(feature = "shield-register"))]
        "/api/v1/mine-send-private" => {
            not_available("this build was compiled without the shield-register feature")
        }
        _ => not_available("unknown local endpoint"),
    }
}
