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

    // ── TEST SEAM ───────────────────────────────────────────────────────────────────
    // In a normal build these three ARE `crate::flux_home()`, `crate::miner_seed()` and
    // `crate::engine_node_url()` — the indirection compiles away entirely.
    //
    // In a `cfg(test)` build they first consult a THREAD-LOCAL override, so a test can
    // point one handler call at a stub node and a scratch home without touching `HOME` or
    // `SIGIL_MINE_*`. Those are process-global: setting them from a test corrupts every
    // other test running concurrently in the same binary. That is not hypothetical — the
    // first version of the tests below did exactly that and broke
    // `producer::run::tests::local_mining_api_credits_a_real_solve_into_a_minted_block`,
    // which reads `engine_node_url()` too. Thread-local, so the tests also stay parallel.
    #[cfg(not(test))]
    fn ctx_home() -> String {
        crate::flux_home()
    }
    #[cfg(not(test))]
    fn ctx_seed() -> Option<[u8; 32]> {
        crate::miner_seed()
    }
    #[cfg(not(test))]
    fn ctx_node() -> String {
        crate::engine_node_url()
    }

    #[cfg(test)]
    #[derive(Clone)]
    struct TestCtx {
        home: String,
        seed: [u8; 32],
        node: String,
    }
    #[cfg(test)]
    thread_local! {
        static TEST_CTX: std::cell::RefCell<Option<TestCtx>> = std::cell::RefCell::new(None);
    }
    #[cfg(test)]
    fn ctx_home() -> String {
        TEST_CTX
            .with(|c| c.borrow().as_ref().map(|t| t.home.clone()))
            .unwrap_or_else(crate::flux_home)
    }
    #[cfg(test)]
    fn ctx_seed() -> Option<[u8; 32]> {
        TEST_CTX
            .with(|c| c.borrow().as_ref().map(|t| t.seed))
            .or_else(crate::miner_seed)
    }
    #[cfg(test)]
    fn ctx_node() -> String {
        TEST_CTX
            .with(|c| c.borrow().as_ref().map(|t| t.node.clone()))
            .unwrap_or_else(crate::engine_node_url)
    }

    /// Persistent per-wallet note-derivation-index counter — part of the money path, not
    /// bookkeeping.
    ///
    /// # What an index actually decides
    ///
    /// `ShieldedAccount::blinding(index) = derive(seed, "blinding", index)` is a pure
    /// function of the index (`sigil-shield/src/wallet.rs:161`), and a note's commitment is
    /// `compress2(compress2(value, blinding), pk)`. So the SAME `(index, value)` pair
    /// always reproduces the SAME commitment, bit for bit.
    ///
    /// # The correction (2026-09-02)
    ///
    /// This comment used to claim that reusing an index "doesn't break fund safety
    /// (nullifiers are bound to leaf POSITION, not index) but does needlessly weaken the
    /// note's per-instance randomness." **The nullifier half is still true; the conclusion
    /// is wrong, and it has been wrong since 2026-08-25.**
    ///
    /// `sigil_state::shielded::append_note_with_delivery` — the ONE entry point every mint
    /// path funnels through (`Shield`, `Unshield`, `ShieldedSpend` outputs,
    /// `ShieldedCoinbase`) — rejects any commitment the pool has EVER held, across every
    /// sealed epoch, with `ShieldedError::DuplicateCommitment`
    /// (`sigil-state/src/shielded.rs:773`). A reused index therefore does not produce a
    /// less-random note. It produces a deposit **that never lands**.
    ///
    /// And it fails in the worst possible shape, because `/v1/shield` only ENQUEUES
    /// (`sigil-api/src/shielded.rs::submit_shield` returns a txid the moment the signature
    /// and nonce check out). The duplicate is not detected until apply time, so:
    ///
    /// * this endpoint sees `{"ok":true,"txid":...}` and reports the part as landed;
    /// * the wallet page calls `noteRecord(addr, index, value)` on that receipt and books
    ///   a note it does not own;
    /// * the transparent balance is never debited (`append_note` runs FIRST in the
    ///   `Shield` arm precisely so a rejection leaves state untouched), so no value is
    ///   lost — but the user was shown a success receipt for a deposit that silently
    ///   evaporated, and every later private send that picks that phantom note fails.
    ///
    /// That is why every failure below is fail-CLOSED: an index that could not be read, or
    /// could not be fsynced, is never published. A burned index costs nothing (the space is
    /// u64); a reused one costs a deposit that looks successful and is not.
    ///
    /// # Two allocators, one pool
    ///
    /// The wallet page keeps its OWN counter in
    /// `localStorage['sigil-shielded-noteidx-'+addr]` — per-origin, erasable, and not this
    /// file. No amount of durability here can see that one. What CAN see it is the chain:
    /// a commitment the page already published is in the live leaf set. So the counter is
    /// only a starting HINT, and [`choose_indices`] skips any index the chain says it has
    /// already seen — via `/v1/shielded/has`, which answers with the very `has_ever_held`
    /// the mint chokepoint uses, across every epoch. That is the same decision, through the
    /// same endpoint, that the page's own `freeIndexFor()`/`_cmExists()` pair makes
    /// (`gui/sigil-wallet-tron-embedded.html:1813`), reached independently — and it is why
    /// this endpoint refuses to submit at all when it cannot get an answer, exactly as the
    /// page refuses ("not submitting blind — a re-used note commitment is rejected
    /// forever").
    ///
    /// The residual gap, stated plainly: `has_ever_held` only knows commitments that have
    /// LANDED. Two deposits of the same denomination issued within one block window — one
    /// from here, one from the page — can still both be told the index is free, and
    /// collide. The counter plus [`NOTE_INDEX_LOCK`] closes that window for calls to THIS
    /// endpoint; nothing closes it across the origin boundary short of a shared allocation
    /// protocol. That is NOT fixed here.
    fn note_index_path(addr: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(ctx_home())
            .join(".flux")
            .join(format!("sigil-shield-noteidx-{addr}"))
    }

    /// Read the counter, or say why it could not be read.
    ///
    /// A MISSING file is the one benign case: a wallet that has never shielded from this
    /// box genuinely starts at 0. Everything else — a truncated write, a garbled byte, a
    /// permission error, `.flux` turned into a regular file — is an ERROR, because the old
    /// `.ok().and_then(parse).unwrap_or(0)` turned every one of those into "start over at
    /// index 0", which is the rewind this counter exists to prevent.
    fn load_note_index(addr: &str) -> Result<u64, String> {
        let path = note_index_path(addr);
        match std::fs::read_to_string(&path) {
            Ok(s) => s.trim().parse::<u64>().map_err(|e| {
                format!(
                    "the note-index counter at {} is unreadable ({e}) — refusing to \
                     re-derive from 0, which would republish commitments the pool already \
                     holds. Repair or remove the file deliberately.",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(format!(
                "could not read the note-index counter at {}: {e}",
                path.display()
            )),
        }
    }

    /// Record the counter durably, or say why not. Every step is fallible on purpose.
    ///
    /// The old version was `let _ = create_dir_all(..); let _ = write(..)`. `write` creates
    /// the FILE, never the DIRECTORY, so on a box without `~/.flux` both calls failed and
    /// both errors were discarded — and the deposit went out anyway at an index recorded
    /// nowhere.
    ///
    /// Temp-file-then-rename rather than a straight overwrite: this file holds a SINGLE
    /// number, so a torn write leaves a SHORTER number, i.e. a smaller one — the exact
    /// rewind being defended against. `rename` is atomic on POSIX, so a reader sees either
    /// the old value or the new one, never half of one.
    ///
    /// And `sync_all`, not `flush`: `flush` on a `std::fs::File` is a no-op (the handle is
    /// unbuffered), which leaves the bytes in the page cache — surviving a process exit but
    /// not a power cut, and a power cut is precisely the restart that rewinds the counter.
    fn save_note_index(addr: &str, next: u64) -> Result<(), String> {
        use std::io::Write;
        let path = note_index_path(addr);
        let dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let fail = |what: &str, e: std::io::Error| {
            format!(
                "could not record note index {next} at {}: {what}: {e}",
                path.display()
            )
        };
        std::fs::create_dir_all(&dir).map_err(|e| fail("creating the parent directory", e))?;
        let tmp = dir.join(format!("sigil-shield-noteidx-{addr}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| fail("creating the temp file", e))?;
            f.write_all(next.to_string().as_bytes())
                .map_err(|e| fail("writing", e))?;
            f.sync_all().map_err(|e| fail("fsync", e))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| fail("renaming into place", e))?;
        // Best-effort, and only best-effort by necessity: the rename is a DIRECTORY
        // metadata change, which the file's own fsync does not cover, so this is what makes
        // the new name durable on Linux. Windows cannot open a directory as a file at all,
        // so a failure here is not an error — on that platform the rename's durability is
        // the filesystem's business, not ours.
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// How far past the counter to hunt for an index the pool has not already consumed.
    /// Same bound the wallet page's `freeIndexFor()` uses, for the same reason: past a few
    /// thousand collisions on one denomination something is wrong that scanning will not
    /// fix, and an unbounded loop in a request handler is its own bug.
    const INDEX_SCAN_LIMIT: u64 = 4096;

    /// Pick one index per part, starting at `base`, skipping any index whose commitment
    /// the chain has already seen (or that an earlier part in this same call just took).
    ///
    /// `taken` is the membership question, injected rather than hard-wired so this stays
    /// pure and testable without a node. In production it is [`commitment_is_taken`], i.e.
    /// the chain's own `has_ever_held`. Any error it returns propagates: an unanswerable
    /// membership question must STOP the deposit, never be guessed at.
    fn choose_indices<F>(
        account: &ShieldedAccount,
        base: u64,
        parts: &[u128],
        mut taken: F,
    ) -> Result<Vec<(u64, u64, String)>, String>
    where
        F: FnMut(&str) -> Result<bool, String>,
    {
        let ceiling = base.saturating_add(INDEX_SCAN_LIMIT);
        let mut chosen: Vec<(u64, u64, String)> = Vec::with_capacity(parts.len());
        let mut idx = base;
        for value128 in parts {
            let value = *value128 as u64; // denominations top out at 2e17, well within u64
            let mut slot = None;
            while idx < ceiling {
                let note = account
                    .note(idx, value)
                    .map_err(|e| format!("note build failed at index {idx}: {e}"))?;
                let cm = hex::encode(to_wire(note.commitment()));
                let used = chosen.iter().any(|(_, _, c)| c == &cm) || taken(&cm)?;
                idx += 1;
                if !used {
                    slot = Some((idx - 1, value, cm));
                    break;
                }
            }
            match slot {
                Some(s) => chosen.push(s),
                None => {
                    return Err(format!(
                        "no free note index for a {value}-unit part within {INDEX_SCAN_LIMIT} \
                         of {base} \u{2014} this wallet has shielded that denomination a great \
                         many times; try a different amount"
                    ))
                }
            }
        }
        Ok(chosen)
    }

    /// Ask the chain the exact question it will ask itself at apply time: has this
    /// commitment EVER been held, in any epoch?
    ///
    /// `/v1/shielded/has` answers with `ShieldedPool::has_ever_held` directly
    /// (`sigil-api/src/lib.rs:654`) \u{2014} the same guard `append_note_with_delivery` uses.
    /// That is why this endpoint and not `/v1/shielded/leaves`: leaves serves ONE epoch
    /// (the live one, unless `?epoch=` is given), so after a pool rotation a commitment
    /// sealed in an earlier generation would look free here and still be refused there.
    ///
    /// Fail CLOSED on anything ambiguous \u{2014} a network error, a non-200, a body without a
    /// boolean `present`. Submitting blind is what produces the permanently-stuck deposit;
    /// declining costs nothing, because the wallet page's manual flow performs this same
    /// check itself and simply takes over.
    fn commitment_is_taken(
        client: &reqwest::blocking::Client,
        node: &str,
        cm_hex: &str,
    ) -> Result<bool, String> {
        let url = format!("{}/v1/shielded/has?cm={cm_hex}", node.trim_end_matches('/'));
        let body: serde_json::Value = client
            .get(&url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| {
                format!(
                    "could not ask the node whether a commitment is already used ({e}) \u{2014} not \
                     submitting blind, because a re-used note commitment is rejected forever"
                )
            })?
            .json()
            .map_err(|e| format!("bad /v1/shielded/has response from the node: {e}"))?;
        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(format!(
                "the node refused a commitment-membership query: {}",
                body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
            ));
        }
        body.get("present")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| "/v1/shielded/has answered without a boolean 'present'".to_string())
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
    /// since the indices come from [`choose_indices`] over the durable counter above), and
    /// submit each to this node's `/v1/shield`. Returns
    /// `{"ok":true,"landed":[{"txid","value","index"},...]}` once ANY part lands — including a `"warning"` field if a LATER part then failed,
    /// so the caller can show a truthful partial receipt instead of silently discarding
    /// already-shielded value (falling back to the manual flow after a partial success
    /// would re-attempt the WHOLE amount from scratch and double-shield the landed part).
    pub fn handle_mine_shield(body: &str) -> (&'static str, String) {
        #[derive(serde::Deserialize)]
        struct Req {
            amount: String,
        }

        let Some(seed) = ctx_seed() else {
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

        let node = ctx_node();
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => return bad_request(format!("http client init failed: {e}")),
        };

        // ── ALLOCATE FIRST, PUBLISH SECOND ──────────────────────────────────────────
        // Everything down to the `save_note_index` below happens BEFORE a single byte is
        // POSTed. That ordering is the fix: a leaf, once published, is permanent, so the
        // record of which index produced it has to be durable already. The old code saved
        // AFTER the loop, which meant a crash — or a power cut — anywhere in between left
        // permanent leaves the counter had never heard of, and the next boot handed the
        // same indices out again.
        //
        // The cost of doing it this way is that a submit failing later BURNS the indices
        // that were reserved for it. That is the right trade and it is not close: an index
        // is one number out of 2^64, and burning one costs nothing at all, while reusing
        // one costs a deposit that returns a txid, shows a success receipt, and is then
        // silently refused at apply time with `DuplicateCommitment` — value that appears to
        // move and does not.
        let _guard = NOTE_INDEX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let base = match load_note_index(&from) {
            Ok(b) => b,
            Err(e) => return bad_request(e),
        };
        let chosen = match choose_indices(&account, base, &parts, |cm| {
            commitment_is_taken(&client, &node, cm)
        }) {
            Ok(c) => c,
            Err(e) => return bad_request(e),
        };
        // Reserve the WHOLE span (past the highest index actually taken, so the skipped
        // ones are burned too) and make it durable, or publish nothing at all.
        let reserve_to = chosen.last().map(|(i, _, _)| i.saturating_add(1)).unwrap_or(base);
        if let Err(e) = save_note_index(&from, reserve_to) {
            return bad_request(e);
        }

        let mut landed: Vec<serde_json::Value> = Vec::new();
        let mut failure: Option<String> = None;

        for (this_index, value, cm_hex) in chosen {
            let fee: u128 = 0;
            // Strictly increasing per part, same idiom as the browser's `Date.now()+i`.
            let req_nonce = now_ms() + this_index;
            let msg =
                format!("sigil-rpc/v1|shield|{from}|{value}|{cm_hex}|{fee}|nonce={req_nonce}");
            let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
            // 2026-09-08: seal the note to OUR OWN delivery key so every client of this
            // seed (phone, browser, MCP) finds this deposit by trial-decryption instead of
            // guessing the derivation index its own way. Optional on the wire; an older
            // node simply ignores it.
            let note_ciphertext: Option<String> = u64::try_from(value).ok().and_then(|v| {
                seal_note(&NotePlaintext::new(v, account.blinding(this_index)), &account.address(&seed))
                    .ok()
                    .map(|c| c.0)
            });
            let payload = serde_json::json!({
                "from": from,
                "amount": value.to_string(),
                "cm": cm_hex,
                "fee": fee.to_string(),
                "sig": sig,
                "req_nonce": req_nonce,
                "note_ciphertext": note_ciphertext,
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

        // Nothing to persist here any more: the counter was made durable BEFORE the first
        // part was published (see the block above), which is the only ordering that
        // survives a crash mid-loop.
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
    /// function's logic step for step.
    ///
    /// NOTE-INDEX SAFETY, re-checked 2026-09-02 (the counter above had the opposite
    /// problem, so this path was audited alongside it): this handler builds a fresh
    /// single-call `NoteStore`, and an older version of this comment recorded that as a
    /// "quirk" — indices 0/1 for the two outputs, every time. That is no longer true and
    /// it matters, because two payments of the same amount to the same payee with the same
    /// blinding are one byte-identical leaf, which the chain now refuses forever
    /// (`ShieldedError::DuplicateCommitment`). `build_spend` stopped taking output indices
    /// from the store's counter: it derives them with
    /// `sigil_shield::wallet::output_derivation_index(anchor, position, slot)`, bound to
    /// the leaf POSITION of the note being consumed — and a note is consumed exactly once,
    /// so that input cannot repeat. Those indices also live in their own high band
    /// (`OUT_INDEX_BASE = 2^31`), so they cannot collide with the sequential counter
    /// `mine-shield` allocates from either. Nothing on this path needs the fix applied
    /// above; it is already position-derived rather than counter-derived.
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

        let Some(seed) = ctx_seed() else {
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

        let node = ctx_node();
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

        // The set of nullifiers the chain has ALREADY seen. A note whose nullifier is in
        // here has been spent — often by another copy of this same seed in an earlier
        // session (the recurring "same seed elsewhere" trap). Proving a spend of such a
        // note wastes seconds building a STARK the node is guaranteed to reject with
        // `nullifier already spent`, and the caller would then be told a payment "went
        // through" when it never landed. So we skip those notes BEFORE proving — the
        // native equivalent of the Android wallet's `retireSpentElsewhere`. The endpoint
        // returns the full list (`nullifiers: [...]`), not just a count.
        let spent_nullifiers: std::collections::HashSet<String> = {
            let url = format!("{}/v1/shielded/nullifiers", node.trim_end_matches('/'));
            match client.get(&url).send().and_then(|r| r.error_for_status()) {
                Ok(r) => r
                    .json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.get("nullifiers").and_then(|n| n.as_array()).cloned())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_ascii_lowercase()))
                            .collect()
                    })
                    .unwrap_or_default(),
                // If the list is unreachable we do NOT block the send — the chain is still
                // the final authority and will reject a real double-spend. We just lose the
                // pre-flight skip. (Empty set = skip nothing.)
                Err(_) => std::collections::HashSet::new(),
            }
        };

        // Pick the smallest of OUR OWN candidate notes that covers amount+fee, has actually
        // landed (its commitment is present in the live leaf set), AND has not already been
        // spent (its nullifier is not in the published set) — same "prefer the smallest
        // covering note" policy `doPrivateSend()` uses.
        let mut best: Option<(u64, u64, usize)> = None; // (index, value, position)
        let mut skipped_spent = 0usize;
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
            // nf = compress2(spend_key, position) — the exact value the chain checks.
            let nf_hex = hex::encode(to_wire(account.nullifier_at(pos as u64)));
            if spent_nullifiers.contains(&nf_hex) {
                skipped_spent += 1;
                continue;
            }
            let better = match &best {
                Some((_, best_value, _)) => value < *best_value,
                None => true,
            };
            if better {
                best = Some((cand.index, value, pos));
            }
        }
        let Some((note_index, note_value, position)) = best else {
            if skipped_spent > 0 {
                return bad_request(format!(
                    "the {skipped_spent} note(s) large enough to cover {amount} + fee {fee} \
                     have already been spent (their nullifiers are on chain) — likely by \
                     another copy of this seed. Shield fresh funds and try again."
                ));
            }
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

        // CONFIRM ON SETTLE — and the settlement identity is the NULLIFIER, not the txid.
        //
        // `/v1/shielded_send` returns `ok:true` the instant the proof and fee check out and
        // the tx is QUEUED. That is acceptance, not settlement. But on a DAG braid a shielded
        // send is relayed (Dandelion++) and may be retried, so the SAME nullifier can ride
        // more than one candidate tx: one wins and delivers the payment, and a sibling's txid
        // is then correctly rejected with `nullifier already spent`. If we keyed success on
        // "did MY txid apply", that sibling rejection reads as a failure for a payment that
        // actually SETTLED — a false NEGATIVE (measured live 2026-09-05: four payments landed
        // in the recipient's pool while every job reported "failed"). The v8 bug was the
        // mirror image (false positive). Both are cured the same way: ask the one question
        // that is authoritative on this chain — **is OUR nullifier now in the spent set?**
        //
        // We only reach here after the selector confirmed our nullifier was NOT already
        // published (the skip-spent filter let this note through). So if it appears in the
        // spent set now, WE put it there: the note is spent, the value moved, the payment
        // is delivered — regardless of which txid carried it or what our own txid's status
        // says. Only a rejection for some OTHER reason (unknown anchor, bad proof) with our
        // nullifier still absent is a real failure.
        let our_nf_hex = hex::encode(bundle.nullifier).to_ascii_lowercase();
        let status_url = format!("{}/v1/transactions/{}", node.trim_end_matches('/'), txid);
        let nulls_url = format!("{}/v1/shielded/nullifiers", node.trim_end_matches('/'));
        let nf_is_spent = |client: &reqwest::blocking::Client| -> bool {
            client
                .get(&nulls_url)
                .send()
                .ok()
                .and_then(|r| r.json::<serde_json::Value>().ok())
                .and_then(|v| v.get("nullifiers").and_then(|n| n.as_array()).cloned())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .any(|s| s.eq_ignore_ascii_case(&our_nf_hex))
                })
                .unwrap_or(false)
        };

        // The one unambiguous settlement test: OUR selected note's nullifier is now in the
        // spent set. The selector proved it was NOT there when it chose the note (the
        // skip-spent filter excludes any candidate whose nullifier is already published), so
        // its APPEARANCE can only mean our own spend landed — the payment is delivered. This
        // is robust to the DAG braid's Dandelion++ relay, where one send fans out into
        // several candidate txs: whichever one applies puts our nullifier on chain, and the
        // others' txids are rejected as "already spent" — but that rejection is about a txid,
        // not the payment, so we never key success on it. Conversely, if our nullifier never
        // appears, the payment did NOT settle no matter what any single txid's status says —
        // which is exactly the false-POSITIVE this avoids (a doomed note whose txid is
        // rejected must NOT be reported paid).
        // A SHORT in-handler confirm. Measured live 2026-09-06: at steady-state block
        // production (~0.83 blk/s) a shielded send commonly needs a MINUTE-PLUS to reach the
        // spent set — a shield block, then the spend's own block. Blocking this handler that
        // long is wrong (the browser fast-path calls it over HTTP), so we only wait briefly
        // here and hand a still-pending payment to the caller to WATCH, rather than deciding
        // failure prematurely. That premature verdict was the last false-negative: eight test
        // payments ALL delivered, yet the short window made the job say "failed".
        //
        // Verdict rules:
        //   * our nullifier in the spent set        -> SETTLED (value moved; authoritative).
        //   * a HARD rejection (bad proof, unknown  -> FAILED. These are terminal: the note
        //     anchor, insufficient, …) with the        cannot be spent as submitted.
        //     nullifier still absent
        //   * "nullifier already spent" / anything  -> PENDING. On the braid a send is relayed
        //     else, or the window simply expires        into several candidate txs (Dandelion++);
        //                                               a losing sibling's txid is rejected with
        //     exactly that reason while the WINNER is still riding to a block. So this is not a
        //     failure — the caller keeps watching the nullifier (never resubmitting).
        let is_hard_reject = |reason: &str| {
            let r = reason.to_ascii_lowercase();
            !(r.contains("already spent") || r.contains("nullifier") || r.is_empty())
        };
        let mut settled = false;
        let mut hard_fail: Option<String> = None;
        for _ in 0..24 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if nf_is_spent(&client) {
                settled = true;
                break;
            }
            let Ok(r) = client.get(&status_url).send() else { continue };
            let Ok(v) = r.json::<serde_json::Value>() else { continue };
            let data = v.get("data").unwrap_or(&v);
            let st = data.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if matches!(st, "applied" | "settled" | "confirmed") {
                if nf_is_spent(&client) {
                    settled = true;
                    break;
                }
            } else if matches!(st, "rejected" | "failed" | "dropped") {
                let reason = data
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if is_hard_reject(&reason) && !nf_is_spent(&client) {
                    hard_fail = Some(reason);
                    break;
                }
                // else: a losing braid sibling — keep watching, the winner may still land.
            }
        }

        if settled || nf_is_spent(&client) {
            ok_json(serde_json::json!({
                "ok": true,
                "settled": true,
                "txid": txid,
                "nullifier": our_nf_hex,
                "spent_index": note_index,
                "change_index": change_index,
                "change_value": change_value.to_string(),
            }))
        } else if let Some(reason) = hard_fail {
            ok_json(serde_json::json!({
                "ok": false,
                "submitted": true,
                "txid": txid,
                "error": reason,
            }))
        } else {
            // Submitted, proof valid, no hard rejection — the spend is in flight but the
            // block carrying it has not landed yet. Hand the caller the nullifier to watch.
            ok_json(serde_json::json!({
                "ok": true,
                "settled": false,
                "pending": true,
                "submitted": true,
                "txid": txid,
                "nullifier": our_nf_hex,
                "spent_index": note_index,
                "change_index": change_index,
                "change_value": change_value.to_string(),
                "note": "submitted; awaiting the block that carries this nullifier. Watch \
                         /v1/shielded/nullifiers for it — do NOT resubmit.",
            }))
        }
    }

    #[cfg(test)]
    mod note_index_durability_tests {
        //! What these tests are FOR.
        //!
        //! Every one of them drives the real `handle_mine_shield` end to end against a
        //! stub node, so none of them depends on the private helpers' signatures — they
        //! assert only on what a caller can observe (the JSON response, and what the node
        //! actually received). That is deliberate: they were written and RUN against the
        //! pre-fix code first, where all four fail, before the fix existed to make them
        //! pass. A test that passes against the broken code proves nothing.

        use super::*;
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        fn seed() -> [u8; 32] {
            [0x11u8; 32]
        }
        fn wallet_addr() -> String {
            hex::encode(
                ed25519_dalek::SigningKey::from_bytes(&seed())
                    .verifying_key()
                    .to_bytes(),
            )
        }
        /// The commitment this wallet WOULD publish for `(index, value)` — the exact value
        /// `handle_mine_shield` derives, so a test can predict a collision.
        fn cm_for(index: u64, value: u64) -> String {
            let acct = ShieldedAccount::from_seed(seed());
            hex::encode(to_wire(acct.note(index, value).unwrap().commitment()))
        }

        struct Scratch(std::path::PathBuf);
        impl Scratch {
            fn new(tag: &str) -> Scratch {
                let p = std::env::temp_dir().join(format!(
                    "sigil-top-noteidx-{tag}-{}-{:?}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).expect("scratch dir");
                Scratch(p)
            }
            fn counter_path(&self) -> std::path::PathBuf {
                self.0
                    .join(".flux")
                    .join(format!("sigil-shield-noteidx-{}", wallet_addr()))
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// One request the stub node saw, together with the counter file exactly as it
        /// stood on disk AT THAT MOMENT — which is how the write-ahead test observes
        /// ordering without needing to kill a process.
        #[derive(Clone)]
        struct Hit {
            path: String,
            body: String,
            counter_at_hit: Option<String>,
        }

        struct Stub {
            addr: String,
            hits: Arc<Mutex<Vec<Hit>>>,
            stop: Arc<AtomicBool>,
            handle: Option<std::thread::JoinHandle<()>>,
        }

        fn header_end(buf: &[u8]) -> Option<usize> {
            buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
        }
        fn content_length(buf: &[u8]) -> usize {
            let text = String::from_utf8_lossy(buf).to_lowercase();
            text.lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        }

        impl Stub {
            /// `leaves` is the shielded pool's live leaf set as this stub reports it.
            fn start(leaves: Vec<String>, counter_path: std::path::PathBuf) -> Stub {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
                let addr = format!("http://{}", listener.local_addr().unwrap());
                listener.set_nonblocking(true).expect("nonblocking");
                let hits: Arc<Mutex<Vec<Hit>>> = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let handle = {
                    let hits = hits.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            match listener.accept() {
                                Ok((mut s, _)) => {
                                    let _ = s.set_nonblocking(false);
                                    let _ = s.set_read_timeout(Some(
                                        std::time::Duration::from_millis(500),
                                    ));
                                    let mut buf: Vec<u8> = Vec::new();
                                    let mut tmp = [0u8; 4096];
                                    loop {
                                        match s.read(&mut tmp) {
                                            Ok(0) => break,
                                            Ok(n) => {
                                                buf.extend_from_slice(&tmp[..n]);
                                                if let Some(bs) = header_end(&buf) {
                                                    if buf.len() - bs >= content_length(&buf) {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    let head = String::from_utf8_lossy(&buf).to_string();
                                    let path =
                                        head.split_whitespace().nth(1).unwrap_or("").to_string();
                                    let body = header_end(&buf)
                                        .map(|bs| String::from_utf8_lossy(&buf[bs..]).to_string())
                                        .unwrap_or_default();
                                    // Read the counter file BEFORE replying, so the
                                    // snapshot is "what was durable when the node was
                                    // asked to mint this leaf".
                                    let counter_at_hit = std::fs::read_to_string(&counter_path).ok();
                                    hits.lock().unwrap().push(Hit {
                                        path: path.clone(),
                                        body,
                                        counter_at_hit,
                                    });
                                    let payload = if path.starts_with("/v1/shielded/has") {
                                        // Same contract as the real handler: `present` is
                                        // `has_ever_held`, across every epoch.
                                        let asked = path
                                            .split("cm=")
                                            .nth(1)
                                            .unwrap_or("")
                                            .to_ascii_lowercase();
                                        serde_json::json!({
                                            "ok": true,
                                            "cm": asked,
                                            "present": leaves
                                                .iter()
                                                .any(|l| l.eq_ignore_ascii_case(&asked)),
                                            "epoch": 0u64,
                                        })
                                        .to_string()
                                    } else if path.contains("leaves") {
                                        serde_json::json!({
                                            "leaves": leaves, "capacity": 32768u64
                                        })
                                        .to_string()
                                    } else {
                                        serde_json::json!({"ok": true, "txid": "de".repeat(32)})
                                            .to_string()
                                    };
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    );
                                    let _ = s.write_all(resp.as_bytes());
                                    let _ = s.flush();
                                }
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    std::thread::sleep(std::time::Duration::from_millis(3));
                                }
                                Err(_) => break,
                            }
                        }
                    })
                };
                Stub {
                    addr,
                    hits,
                    stop,
                    handle: Some(handle),
                }
            }
            fn shield_hits(&self) -> Vec<Hit> {
                self.hits
                    .lock()
                    .unwrap()
                    .iter()
                    // EXACT path: `/v1/shielded/has` also has `/v1/shield` as a prefix.
                    .filter(|h| h.path.split('?').next() == Some("/v1/shield"))
                    .cloned()
                    .collect()
            }
        }
        impl Drop for Stub {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        /// Point THIS THREAD's handler calls at `home` and `node`. Thread-local, never
        /// the environment — see the test seam's comment for the test this broke when it
        /// used `set_var`.
        fn with_ctx<T>(home: &std::path::Path, node: &str, f: impl FnOnce() -> T) -> T {
            TEST_CTX.with(|c| {
                *c.borrow_mut() = Some(TestCtx {
                    home: home.to_string_lossy().into_owned(),
                    seed: seed(),
                    node: node.to_string(),
                })
            });
            let out = f();
            TEST_CTX.with(|c| *c.borrow_mut() = None);
            out
        }

        fn shield(amount: &str) -> serde_json::Value {
            let (_, body) = handle_mine_shield(&format!("{{\"amount\":\"{amount}\"}}"));
            serde_json::from_str(&body).expect("response is JSON")
        }

        /// **An unreadable counter must STOP the deposit, not silently restart it at 0.**
        ///
        /// Pre-fix `load_note_index` was
        /// `read_to_string(..).ok().and_then(parse).unwrap_or(0)`, so a truncated or
        /// garbled counter rewound the allocator wholesale — every index the wallet had
        /// ever used was handed out a second time. Reusing `(index, value)` reproduces the
        /// note bit for bit, and `sigil_state::shielded::append_note_with_delivery`
        /// refuses a commitment the pool has EVER held
        /// (`ShieldedError::DuplicateCommitment`), so those deposits are rejected at apply
        /// time — after `/v1/shield` has already returned a txid.
        #[test]
        fn a_corrupt_counter_refuses_the_deposit_instead_of_rewinding_to_zero() {
            let scratch = Scratch::new("corrupt");
            std::fs::create_dir_all(scratch.0.join(".flux")).unwrap();
            std::fs::write(scratch.counter_path(), b"\x00\x00not-a-number").unwrap();
            let stub = Stub::start(Vec::new(), scratch.counter_path());

            let resp = with_ctx(&scratch.0, &stub.addr, || shield("100"));

            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(false),
                "an unreadable counter must fail closed, got: {resp}"
            );
            assert!(
                stub.shield_hits().is_empty(),
                "nothing may be submitted when the allocator could not be read; saw {:?}",
                stub.shield_hits().iter().map(|h| h.body.clone()).collect::<Vec<_>>()
            );
        }

        /// **An index that could not be RECORDED must not be USED.**
        ///
        /// `$HOME/.flux` is made a regular FILE here, so `create_dir_all` and the write
        /// both fail. Pre-fix both were `let _ = ...`, so the failure was invisible: the
        /// deposit went out at index 0 and the very next call re-derived index 0 again.
        #[test]
        fn an_index_that_cannot_be_persisted_is_never_submitted() {
            let scratch = Scratch::new("unwritable");
            // `.flux` as a FILE: create_dir_all and every write beneath it must fail.
            std::fs::write(scratch.0.join(".flux"), b"not a directory").unwrap();
            let stub = Stub::start(Vec::new(), scratch.counter_path());

            let resp = with_ctx(&scratch.0, &stub.addr, || shield("100"));

            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(false),
                "an unrecordable index must fail closed, got: {resp}"
            );
            assert!(
                stub.shield_hits().is_empty(),
                "an index that could not be fsynced must never reach the chain"
            );
        }

        /// **The counter must be durable BEFORE the leaf is published, not after.**
        ///
        /// Pre-fix, `save_note_index` ran at the very end of the handler, after every
        /// part had already been POSTed. A crash (or power cut) in that window published
        /// permanent leaves whose indices the counter had no record of, so the next boot
        /// handed the same indices out again. The stub records the counter file exactly as
        /// it stood when each `/v1/shield` arrived; write-ahead means it is already past
        /// the whole reserved span by then.
        #[test]
        fn the_counter_is_durable_before_the_first_part_is_submitted() {
            let scratch = Scratch::new("writeahead");
            let stub = Stub::start(Vec::new(), scratch.counter_path());

            // 1100 decomposes greedily into [1000, 100] — two parts, two indices.
            let resp = with_ctx(&scratch.0, &stub.addr, || shield("1100"));

            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(true),
                "the happy path must still land: {resp}"
            );
            let hits = stub.shield_hits();
            assert_eq!(hits.len(), 2, "1100 is two denomination parts");
            for (n, h) in hits.iter().enumerate() {
                let seen: u64 = h
                    .counter_at_hit
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("<absent>")
                    .parse()
                    .unwrap_or_else(|_| {
                        panic!(
                            "part {n} was submitted while the counter file read {:?} — \
                             the index was published before it was recorded",
                            h.counter_at_hit
                        )
                    });
                assert!(
                    seen >= 2,
                    "part {n} was submitted with the counter at {seen}; the whole span of \
                     2 indices must already be durable before ANY part is published"
                );
            }
        }

        /// **An index whose commitment is already in the pool must be skipped.**
        ///
        /// This is the browser-divergence case made concrete. The wallet page keeps its
        /// own `localStorage['sigil-shielded-noteidx-'+addr]` counter, per-origin and
        /// erasable, and it is NOT this file. If the page already shielded value `V` at
        /// index 0, this counter — starting from its own 0 on a fresh box — derives the
        /// identical commitment, which the chain refuses forever. `/v1/shielded/has` —
        /// the chain's own `has_ever_held` — is the only shared truth between the two
        /// allocators, so it is what decides.
        #[test]
        fn an_index_the_browser_already_used_is_skipped() {
            let scratch = Scratch::new("collision");
            let taken = cm_for(0, 100);
            let stub = Stub::start(vec![taken.clone()], scratch.counter_path());

            let resp = with_ctx(&scratch.0, &stub.addr, || shield("100"));

            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(true),
                "it must still deposit, just at a free index: {resp}"
            );
            let hits = stub.shield_hits();
            assert_eq!(hits.len(), 1, "100 is a single denomination part");
            let sent: serde_json::Value = serde_json::from_str(&hits[0].body).expect("json body");
            let sent_cm = sent.get("cm").and_then(|v| v.as_str()).unwrap_or("");
            assert_ne!(
                sent_cm.to_lowercase(),
                taken.to_lowercase(),
                "re-published a commitment the pool already holds — the chain will reject \
                 this at apply time with DuplicateCommitment"
            );
            let landed_index = resp["landed"][0]["index"].as_u64().expect("landed index");
            assert_ne!(landed_index, 0, "index 0 was already taken by the browser");
        }

        /// The allocation rule on its own, with no node in the way: start at the counter,
        /// never hand out an index the pool has consumed, and never hand the same index to
        /// two parts of one deposit.
        #[test]
        fn choose_indices_starts_at_the_counter_and_skips_what_the_pool_holds() {
            let acct = ShieldedAccount::from_seed(seed());

            let never = |_: &str| Ok(false);
            let picked = choose_indices(&acct, 7, &[1000u128, 100u128], never).unwrap();
            assert_eq!(
                picked.iter().map(|(i, _, _)| *i).collect::<Vec<_>>(),
                vec![7, 8],
                "a clean pool allocates straight up from the counter"
            );

            // 7 and 8 already consumed for a 1000-unit part; 9 must be the first free one.
            let pool: std::collections::HashSet<String> =
                [cm_for(7, 1000), cm_for(8, 1000)].into_iter().collect();
            let picked =
                choose_indices(&acct, 7, &[1000u128], |cm| Ok(pool.contains(cm))).unwrap();
            assert_eq!(picked[0].0, 9);
            assert_eq!(picked[0].2, cm_for(9, 1000));

            // A duplicated denomination inside ONE deposit must still get two distinct
            // indices — otherwise the second part is a duplicate of the first.
            let picked = choose_indices(&acct, 0, &[100u128, 100u128], never).unwrap();
            assert_ne!(picked[0].0, picked[1].0);
            assert_ne!(picked[0].2, picked[1].2);
        }

        /// An unanswerable membership question must abort the whole allocation.
        #[test]
        fn choose_indices_propagates_an_unanswerable_membership_question() {
            let acct = ShieldedAccount::from_seed(seed());
            let err = choose_indices(&acct, 0, &[100u128], |_| {
                Err("node unreachable".to_string())
            })
            .unwrap_err();
            assert_eq!(err, "node unreachable");
        }

        /// Exhausting the scan must be an ERROR, not a wrap-around to something reused.
        #[test]
        fn choose_indices_fails_closed_when_every_scanned_index_is_taken() {
            let acct = ShieldedAccount::from_seed(seed());
            let pool: std::collections::HashSet<String> =
                (0..INDEX_SCAN_LIMIT).map(|i| cm_for(i, 100)).collect();
            let err =
                choose_indices(&acct, 0, &[100u128], |cm| Ok(pool.contains(cm))).unwrap_err();
            assert!(
                err.contains("no free note index"),
                "unexpected error text: {err}"
            );
        }
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
