// sigil-top/src/shield_setup.rs — `mine-rig`'s SIGIL_MINE_SEED auto-registration
// (2026-08-24, revised same day).
//
// THE POINT: a miner should not have to know shielded pools exist to contribute to
// one. Give `mine-rig` a seed instead of a bare address, and it derives both the
// wallet's normal signing key AND its shield spend key from that ONE seed, registers
// once (idempotent — re-registering the same key is a harmless no-op transaction),
// then mines to that wallet as normal. From then on every reward for this wallet
// mints as a private note instead of a transparent credit — see
// `sigil-node::coinbase::build_block_body_for`'s `ShieldedCoinbase` branch, which
// already does the right thing once a wallet is registered; nothing there changes.
//
// SEED FORMAT — REVISED: `SIGIL_MINE_SEED` was NOT a new env var. `miner_keypair()`
// (main.rs) already uses it — required, audit-gated — for the TUI's interactive [M]
// mining key: a raw 64-hex seed fed DIRECTLY into `Keypair::from_seed` (== 32 raw
// bytes as the Ed25519 secret, no hashing step; see `sigil-oauth::Keypair::from_seed`).
// The first version of this module used a DIFFERENT format under the SAME variable
// name — `sha3_256(arbitrary string)` — which would have derived a DIFFERENT wallet
// than [M]ine's path for the identical env var value. Fixed to match the established
// format exactly: whoever already has to set SIGIL_MINE_SEED to mine at all now gets
// shielded registration for the same wallet, for free, with nothing new to configure.
//
// WHY ONE SEED, TWO KEYS: the raw 32 seed bytes are used directly as the Ed25519
// signing key (`SigningKey::from_bytes`, matching `Keypair::from_seed` bit-for-bit).
// The shield spend key is `ShieldedAccount::from_seed` on those SAME 32 bytes — a
// completely different, internally domain-separated BLAKE3 derivation. Two
// independent keys from one seed to back up; knowing one reveals nothing about the
// other, because they diverge at the first hash.
//
// SECURITY NOTE ON INPUT: the seed is read from an env var, never a positional CLI
// argument — a CLI arg is visible to every user on the box via `ps aux`; an env var
// is only visible to root (the same trust boundary as running this process at all).

use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use sigil_shield::note_v1::to_wire;
use sigil_shield::wallet::ShieldedAccount;

/// Decode a 64-hex-char string to 32 raw bytes. Deliberately the SAME parsing
/// `hex_to_32` (main.rs) uses for `SIGIL_MINE_SEED` elsewhere — not reimplemented
/// with different edge-case behavior.
fn seed_bytes(seed_hex: &str) -> Option<[u8; 32]> {
    let s = seed_hex.trim().strip_prefix("0x").unwrap_or(seed_hex.trim());
    let v = hex::decode(s).ok()?;
    v.try_into().ok()
}

/// The wallet's Ed25519 signing key — byte-identical to what `miner_keypair()`
/// (main.rs, the TUI's [M]ine path) derives from the SAME `SIGIL_MINE_SEED` value.
/// `None` if `seed_hex` isn't a well-formed 64-hex seed.
pub fn wallet_signing_key(seed_hex: &str) -> Option<SigningKey> {
    Some(SigningKey::from_bytes(&seed_bytes(seed_hex)?))
}

/// What registration actually did, so the caller can print something honest — this
/// never silently swallows a failure.
pub enum RegisterOutcome {
    Registered { wallet_hex: String, txid: String },
    Failed { wallet_hex: String, reason: String },
    /// `seed_hex` wasn't a valid 64-hex seed — distinct from a network/server
    /// failure so the caller can print an actionable message instead of a vague one.
    BadSeed,
}

/// Derive both keys from `seed_hex` (a raw 64-hex seed, the SAME format
/// `SIGIL_MINE_SEED` already requires for [M]ine), sign a `RegisterShieldedAddress`
/// request, and submit it to `node_url`. Synchronous/blocking — this runs once,
/// before the mining loop starts, so there is no async runtime to thread through.
pub fn register_for_shielded_mining(node_url: &str, seed_hex: &str) -> RegisterOutcome {
    let Some(seed) = seed_bytes(seed_hex) else { return RegisterOutcome::BadSeed };
    let sk = SigningKey::from_bytes(&seed);
    let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
    let account = ShieldedAccount::from_seed(seed);
    let pk_shield_hex = hex::encode(to_wire(account.public_key()));
    // 2026-08-24: the note-delivery key, derived from the SAME seed via
    // `note_cipher::enc_identity_from_seed`'s own domain separation — a third
    // independent key from the one seed a miner already backs up. Without publishing
    // this alongside pk_shield, a miner could be paid privately by a third party but
    // would have no way to ever find that payment (only self-mined coinbase notes,
    // whose blinding is publicly re-derivable, would still be discoverable).
    let pk_encrypt_hex = sigil_shield::note_cipher::enc_identity_from_seed(&seed).public_hex();
    let fee: u128 = 0;
    // Same clock source the browser wallet's signer uses (Date.now()-equivalent) —
    // any strictly-increasing value works, this just needs to beat whatever this
    // wallet last used.
    let req_nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1);

    // POST-QUANTUM RAMP KEY (2026-08-27). Derived from the SAME wallet seed, so it is
    // recoverable from the seed phrase the user already has — never a second secret to
    // back up. That matters more here than usual: a registered key has NO removal path
    // (removing it would hand the attack straight back to an Ed25519 forger), so a key
    // that could be lost would mean permanent lockout from the shielded ramps.
    let (sqi_sk, sqi_pk) = flux_sqisign::keygen_from_seed(&seed);
    let sqi_pk_hex = hex::encode(&sqi_pk);
    // PROOF OF POSSESSION: sign the wallet↔key binding WITH the key being registered.
    // Without it, publishing a key is a hijack primitive — see the API-side
    // `verify_sqi_possession` for the full argument. Signed BEFORE the Ed25519 message is
    // built, because that message commits to this key.
    let pop_msg = format!("sigil-rpc/v1|shield-sqi-pop|{wallet_hex}|{sqi_pk_hex}");
    let sqi_pop = match flux_sqisign::sign(pop_msg.as_bytes(), &sqi_sk, &sqi_pk) {
        Ok(sig) => Some(hex::encode(sig)),
        Err(e) => {
            // Degrade to an Ed25519-only registration rather than failing outright: a
            // wallet that cannot produce the PQ half must still be able to register and
            // receive private rewards, exactly as before this feature existed.
            crate::tlog!("⚠ SQIsign proof-of-possession failed ({e}) — registering Ed25519-only");
            None
        }
    };
    let sqi_part = sqi_pop.as_ref().map(|_| format!("|sqi={sqi_pk_hex}")).unwrap_or_default();
    let msg = format!(
        "sigil-rpc/v1|shield-register|{wallet_hex}|{pk_shield_hex}|{pk_encrypt_hex}|{fee}|nonce={req_nonce}{sqi_part}"
    );
    let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());

    let mut body = serde_json::json!({
        "wallet": wallet_hex,
        "pk_shield": pk_shield_hex,
        "pk_encrypt": pk_encrypt_hex,
        "fee": fee.to_string(),
        "sig": sig,
        "req_nonce": req_nonce,
    });
    if let Some(pop) = &sqi_pop {
        body["pk_sqi"] = serde_json::Value::String(sqi_pk_hex.clone());
        body["sqi_pop"] = serde_json::Value::String(pop.clone());
    }

    // 2026-08-24: was "/api/v1/shielded/register" — sigil-api mirrors most routes
    // under both /v1/* and /api/v1/*, but the shielded endpoints were never added to
    // that mirror list (confirmed: /v1/shielded/register is live, /api/v1/shielded/
    // register 404s). Every SIGIL_MINE_SEED registration attempt since this feature
    // shipped (v7.1.74) silently failed at this URL and fell through to "mining
    // continues, but rewards stay transparent" — caught live while testing a real
    // end-to-end shielded send against the production API.
    let url = format!("{}/v1/shielded/register", node_url.trim_end_matches('/'));
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("http client: {e}") },
    };
    let resp = match client.post(&url).json(&body).send() {
        Ok(r) => r,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("request failed: {e}") },
    };
    let parsed: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("bad response: {e}") },
    };
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        let txid = parsed.get("txid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        RegisterOutcome::Registered { wallet_hex, txid }
    } else {
        let reason = parsed.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error").to_string();
        RegisterOutcome::Failed { wallet_hex, reason }
    }
}

/// How long to wait for a submitted registration to appear on-chain before resubmitting.
///
/// Registration is an ordinary shielded tx, so it only settles once its candidate crosses
/// `BraidConfig::final_depth` (512) — measured live at ~3.5 s/block that is roughly 30
/// minutes from submission to visible. 45 gives margin over that floor for reorg and
/// backlog variance without leaving a genuinely dropped registration unretried for hours.
const CONFIRM_WINDOW: Duration = Duration::from_secs(45 * 60);
/// How often to ask the chain whether the registration has landed yet.
const POLL_EVERY: Duration = Duration::from_secs(60);
/// Backoff after a submission is refused outright (bad node URL, node down, node
/// rejecting the tx) — long enough not to hammer, short enough to recover unattended.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(120);

/// How often a CONFIRMED registration is re-verified against the chain.
///
/// The keeper used to `return` the moment it saw its own key registered, on the
/// reasonable-sounding assumption that registration is permanent. It is not: chain state
/// can be replaced wholesale. When SIGIL was cut over from `sigil-g0` to `sigil-g1` on
/// 2026-08-26 every registration in the old state ceased to exist, and every rig already
/// running had long since exited this thread — so they kept mining, forever, with
/// TRANSPARENT rewards and no indication anything had changed. Measured on g1 the morning
/// after: one wallet registered, while four rigs belonging to a second wallet mined
/// unregistered for hours.
///
/// Staying alive and re-checking costs one cheap GET per interval and makes registration
/// self-healing across a genesis change, a state rollback, or any other way the entry can
/// vanish — which is what "automatic" has to mean if it is to survive contact with a
/// chain that can be reset.
const RECHECK_WHEN_REGISTERED: Duration = Duration::from_secs(10 * 60);

/// What the chain currently says about a wallet's shielded address.
pub enum RegistrationState {
    /// Registered, and the published shield key is the one THIS seed derives — block
    /// rewards will mint into notes this seed can actually spend.
    Ours,
    /// Registered to a DIFFERENT shield key. Rewards mint into notes this seed cannot
    /// spend, which is worse than being unregistered, so the keeper stops and says so
    /// loudly rather than fighting whoever holds the other seed.
    Foreign { pk_shield: String },
    /// Nothing published — rewards stay transparent until we register.
    Absent,
    /// Couldn't tell (node unreachable, unparseable answer). Deliberately distinct from
    /// `Absent`: resubmitting on an unknown state would burn a nonce on a registration
    /// that may already be queued or live.
    Unknown(String),
}

/// Ask the chain what shielded address `wallet_hex` currently publishes.
pub fn registration_state(
    node_url: &str,
    wallet_hex: &str,
    expect_pk_shield: &str,
) -> RegistrationState {
    let url = format!(
        "{}/v1/shielded/address?wallet={wallet_hex}",
        node_url.trim_end_matches('/')
    );
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return RegistrationState::Unknown(format!("http client: {e}")),
    };
    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => return RegistrationState::Unknown(format!("request failed: {e}")),
    };
    let parsed: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return RegistrationState::Unknown(format!("bad response: {e}")),
    };
    if parsed.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        // The node answers a genuinely unregistered wallet with a specific error rather
        // than an empty success, so this is a real "not registered", not a transport
        // problem. Anything else stays Unknown.
        let err = parsed.get("error").and_then(|v| v.as_str()).unwrap_or("");
        return if err.contains("has not registered") {
            RegistrationState::Absent
        } else {
            RegistrationState::Unknown(if err.is_empty() { "unrecognized response".into() } else { err.to_string() })
        };
    }
    match parsed.get("pk_shield").and_then(|v| v.as_str()) {
        Some(pk) if pk.eq_ignore_ascii_case(expect_pk_shield) => RegistrationState::Ours,
        Some(pk) => RegistrationState::Foreign { pk_shield: pk.to_string() },
        None => RegistrationState::Unknown("registered but no pk_shield in response".into()),
    }
}

/// The shield public key `seed_hex` derives — what [`registration_state`] compares against.
pub fn expected_pk_shield(seed_hex: &str) -> Option<String> {
    Some(hex::encode(to_wire(ShieldedAccount::from_seed(seed_bytes(seed_hex)?).public_key())))
}

/// Register this wallet for shielded mining and KEEP it registered, on a background
/// thread, without anyone having to notice or intervene.
///
/// Why a keeper and not the single blocking attempt this replaces: registration only
/// takes effect ~30 minutes after submission (see [`CONFIRM_WINDOW`]), and the old
/// one-shot call had no way to observe that. A submission refused by a node that was
/// briefly down, or accepted at the door and later dropped by the pending-pool's age
/// limit, printed one line about rewards "staying transparent until this succeeds" and
/// then nothing ever made it succeed. Measured on the live chain the day this was
/// written: five rigs, 204 MH/s, 1,749 blocks won — and zero registered wallets.
///
/// The loop is idempotent and cheap: it asks the chain first and returns immediately if
/// the wallet is already registered to this seed's key, so a node that restarts twenty
/// times a day submits nothing and burns no nonces.
///
/// Returns `None` only for a malformed seed; the caller has already validated it in every
/// current call site, so that arm is defensive rather than expected. `log` receives each
/// state change — pass whatever surfaces text in the caller's context (`println!` for a
/// headless rig, the TUI's mpsc sender for `[M]`).
pub fn spawn_registration_keeper<F>(
    node_url: &str,
    seed_hex: &str,
    log: F,
) -> Option<std::thread::JoinHandle<()>>
where
    F: Fn(String) + Send + 'static,
{
    let sk = wallet_signing_key(seed_hex)?;
    let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
    let pk_shield = expected_pk_shield(seed_hex)?;
    let node = node_url.to_string();
    let seed = seed_hex.to_string();

    Some(std::thread::spawn(move || {
        let short = wallet_hex.chars().take(8).collect::<String>();
        // Whether we have already told the operator this wallet is registered. Also the
        // memory that lets a later disappearance be reported as a LOSS rather than as a
        // first-time registration.
        let mut announced_registered = false;
        loop {
            match registration_state(&node, &wallet_hex, &pk_shield) {
                RegistrationState::Ours => {
                    // Announce it ONCE, then keep watching. Re-checking (rather than
                    // returning) is what makes this survive a chain reset — see
                    // `RECHECK_WHEN_REGISTERED`. Logging every ten minutes would be noise,
                    // so only the first confirmation, and any later LOSS, is reported.
                    if !announced_registered {
                        announced_registered = true;
                        log(format!(
                            "🔐 {short}… is registered for shielded mining — rewards mint as private notes"
                        ));
                    }
                    std::thread::sleep(RECHECK_WHEN_REGISTERED);
                    continue;
                }
                RegistrationState::Foreign { pk_shield: other } => {
                    log(format!(
                        "⚠ {short}… publishes a DIFFERENT shield key ({}…) than this seed derives. \
                         Rewards mint into notes this seed cannot spend. Mine with the seed that \
                         registered it, or re-register deliberately.",
                        other.chars().take(12).collect::<String>()
                    ));
                    return;
                }
                RegistrationState::Unknown(why) => {
                    log(format!("… shielded registration state unknown ({why}) — retrying"));
                    std::thread::sleep(RETRY_AFTER_FAILURE);
                    continue;
                }
                RegistrationState::Absent => {
                    // Absent AFTER we had already confirmed it means the entry did not
                    // merely never exist — it DISAPPEARED, which on this chain means the
                    // state was replaced under us (the g0→g1 cutover being the worked
                    // example). Say so, then fall through and re-register.
                    if announced_registered {
                        announced_registered = false;
                        log(format!(
                            "⚠ {short}… is no longer registered — the chain state appears to have \
                             been replaced. Re-registering so rewards go back to being private."
                        ));
                    }
                }
            }

            match register_for_shielded_mining(&node, &seed) {
                RegisterOutcome::Registered { txid, .. } => {
                    log(format!(
                        "🔐 registered {short}… for shielded mining (tx {}…) — takes ~30 min to \
                         settle; mining continues meanwhile",
                        txid.chars().take(10).collect::<String>()
                    ));
                }
                RegisterOutcome::Failed { reason, .. } => {
                    log(format!(
                        "⚠ shielded registration for {short}… was refused: {reason} — rewards stay \
                         transparent; retrying in {}s",
                        RETRY_AFTER_FAILURE.as_secs()
                    ));
                    std::thread::sleep(RETRY_AFTER_FAILURE);
                    continue;
                }
                // Unreachable in practice — `wallet_signing_key` already parsed this seed
                // before the thread was spawned. Handled rather than `unreachable!()` so a
                // future caller that skips that check gets a message instead of a panic
                // inside a detached thread nobody is watching.
                RegisterOutcome::BadSeed => {
                    log(format!("⚠ shielded registration for {short}… aborted: seed is not valid 64-hex"));
                    return;
                }
            }

            // Submitted. Watch for it to actually land — "accepted at the door" is not the
            // same as settled, and only the chain can tell us which happened.
            let deadline = Instant::now() + CONFIRM_WINDOW;
            let mut landed = false;
            while Instant::now() < deadline {
                std::thread::sleep(POLL_EVERY);
                if let RegistrationState::Ours =
                    registration_state(&node, &wallet_hex, &pk_shield)
                {
                    landed = true;
                    break;
                }
            }
            if landed {
                log(format!(
                    "✓ {short}… shielded registration confirmed on-chain — every reward from here \
                     on is a private note"
                ));
                // Confirmed, but NOT done: the top of the loop keeps re-verifying so the
                // registration is restored automatically if the chain state is ever
                // replaced. Returning here was the original bug — see
                // `RECHECK_WHEN_REGISTERED`.
                announced_registered = true;
                std::thread::sleep(RECHECK_WHEN_REGISTERED);
                continue;
            }
            log(format!(
                "⚠ {short}… shielded registration did not settle within {} min — resubmitting",
                CONFIRM_WINDOW.as_secs() / 60
            ));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_seed(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    #[test]
    fn expected_pk_shield_matches_what_registration_actually_publishes() {
        // `registration_state` compares the chain's `pk_shield` against
        // `expected_pk_shield`. If those two ever derived differently, the keeper would
        // read its OWN successful registration as `Foreign` and give up forever.
        let seed = valid_seed(0x7c);
        let account = ShieldedAccount::from_seed(seed_bytes(&seed).unwrap());
        let as_registered = hex::encode(to_wire(account.public_key()));
        assert_eq!(expected_pk_shield(&seed).unwrap(), as_registered);
    }

    #[test]
    fn expected_pk_shield_refuses_a_malformed_seed_instead_of_guessing() {
        assert!(expected_pk_shield("not-a-seed").is_none());
        assert!(expected_pk_shield("deadbeef").is_none());
    }

    #[test]
    fn a_registration_check_that_cannot_reach_the_node_is_unknown_not_absent() {
        // The distinction is load-bearing: `Absent` makes the keeper submit a new
        // registration, so misreading an unreachable node as "not registered" would burn
        // a nonce every retry on a wallet that may already be registered.
        let state = registration_state(
            "http://127.0.0.1:1", // reserved, nothing listens
            &hex::encode([0x11u8; 32]),
            &hex::encode([0x22u8; 32]),
        );
        assert!(matches!(state, RegistrationState::Unknown(_)));
    }

    #[test]
    fn rejects_malformed_seeds_instead_of_silently_deriving_something() {
        assert!(wallet_signing_key("not hex at all").is_none());
        assert!(wallet_signing_key("deadbeef").is_none()); // too short (4 bytes)
        let too_long = format!("{}ff", valid_seed(0x11)); // 33 bytes
        assert!(wallet_signing_key(&too_long).is_none());
    }

    #[test]
    fn same_seed_derives_the_same_wallet_and_shield_key_every_time() {
        let seed = valid_seed(0x42);
        let a = wallet_signing_key(&seed).unwrap();
        let b = wallet_signing_key(&seed).unwrap();
        assert_eq!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());

        let acct_a = ShieldedAccount::from_seed(seed_bytes(&seed).unwrap());
        let acct_b = ShieldedAccount::from_seed(seed_bytes(&seed).unwrap());
        assert_eq!(acct_a.public_key(), acct_b.public_key());
    }

    #[test]
    fn different_seeds_never_collide() {
        let seed1 = valid_seed(0x01);
        let seed2 = valid_seed(0x02);
        let a = wallet_signing_key(&seed1).unwrap();
        let b = wallet_signing_key(&seed2).unwrap();
        assert_ne!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());

        let acct_a = ShieldedAccount::from_seed(seed_bytes(&seed1).unwrap());
        let acct_b = ShieldedAccount::from_seed(seed_bytes(&seed2).unwrap());
        assert_ne!(acct_a.public_key(), acct_b.public_key());
    }

    /// THE COMPATIBILITY GATE this whole revision exists for: this module's wallet
    /// derivation must match `sigil-oauth::Keypair::from_seed` — the SAME
    /// `SIGIL_MINE_SEED` value must resolve to the SAME wallet whether it drives the
    /// TUI's [M]ine key or `mine-rig`'s auto-registration. `Keypair::from_seed` is
    /// `SigningKey::from_bytes(seed)` with no hashing step (verified by reading
    /// crates/sigil-oauth/src/lib.rs directly) — this test pins that this module does
    /// the identical thing, not a re-derivation that happens to look similar.
    #[test]
    fn wallet_derivation_matches_the_existing_sigil_mine_seed_convention() {
        let seed_hex = valid_seed(0x99);
        let seed = seed_bytes(&seed_hex).unwrap();

        // What sigil-oauth::Keypair::from_seed does, reproduced inline (that crate
        // isn't a dependency here, so this mirrors its exact one-line body rather
        // than importing it) — SigningKey::from_bytes(seed), nothing else.
        let expected = SigningKey::from_bytes(&seed);
        let actual = wallet_signing_key(&seed_hex).unwrap();
        assert_eq!(
            expected.verifying_key().to_bytes(),
            actual.verifying_key().to_bytes(),
            "SIGIL_MINE_SEED must resolve to the SAME wallet in both mining paths"
        );
    }

    /// The message this module signs must byte-for-byte match what the server's
    /// `verify_wallet_sig` in `sigil-api/src/shielded.rs` reconstructs, or every real
    /// registration would fail signature verification despite a correctly-generated
    /// signature — a silent, total outage for this feature. Pin the exact format here
    /// so a change on either side is caught immediately.
    #[test]
    fn signed_message_matches_the_server_side_canonical_format() {
        let seed_hex = valid_seed(0x77);
        let seed = seed_bytes(&seed_hex).unwrap();
        let sk = wallet_signing_key(&seed_hex).unwrap();
        let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
        let account = ShieldedAccount::from_seed(seed);
        let pk_shield_hex = hex::encode(to_wire(account.public_key()));
        let pk_encrypt_hex = sigil_shield::note_cipher::enc_identity_from_seed(&seed).public_hex();
        let fee: u128 = 0;
        let req_nonce = 42u64;
        let msg = format!(
            "sigil-rpc/v1|shield-register|{wallet_hex}|{pk_shield_hex}|{pk_encrypt_hex}|{fee}|nonce={req_nonce}"
        );
        let sig_bytes = sk.sign(msg.as_bytes()).to_bytes();

        // Reproduce the server's verify step directly (same crate ed25519_dalek is
        // used both sides) rather than trusting our own signing call blindly.
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let vk = VerifyingKey::from_bytes(&sk.verifying_key().to_bytes()).unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify(msg.as_bytes(), &sig).expect("must verify under the exact server-side message format");
    }
}

// ── SHIELDED BALANCE ────────────────────────────────────────────────────────────────
//
// Why this exists (operator-reported, 2026-08-27): "i can see mining personal hashrate in
// web ui from tui sigil top node. but no mining rewards are coming in. the balance dont
// raise."
//
// The balance was right and the rewards were real. A wallet that has registered a shield
// key is paid in NOTES, and a note's value is hidden — so `/v1/balance`, which reports the
// transparent domain, is frozen for that wallet FOREVER by construction. Measured at the
// time: 7.77 SIGIL transparent, unchanged for hours, while the pool this miner was being
// paid into went 63 -> 117 SIGIL in thirty minutes.
//
// Showing a shielded miner their transparent balance is not a small display inaccuracy; it
// is the single number they use to decide whether mining works, and it reads zero forever.
// sigil-top already holds the seed whenever SIGIL_MINE_SEED is set, so it can simply do
// what a wallet does: trial-decrypt the pool and add up what opens.

/// What this seed owns in the shielded pool, plus the pool-wide numbers worth showing next
/// to it.
#[derive(Debug, Clone, Default)]
pub struct ShieldedSnapshot {
    /// Spendable value this seed can open — the number a miner actually wants.
    pub balance: u128,
    /// Notes we can open and have located in the tree.
    pub owned: usize,
    /// Of those, ones the chain says are already spent.
    pub spent: usize,
    /// Live-epoch note count and the ceiling it is filling toward.
    pub pool_notes: usize,
    pub pool_capacity: usize,
    /// Total value locked pool-wide (everyone's, not ours).
    pub pool_locked: u128,
    /// Live pool generation; > 0 means the pool has rotated at least once.
    pub epoch: u32,
    /// How many sealed generations exist. Our notes may be in any of them.
    pub sealed_epochs: usize,
    /// Wallets that have published a shield key.
    pub registered: usize,
    /// Chain-wide spent-nullifier count.
    pub nullifiers: usize,
}

impl ShieldedSnapshot {
    /// Pool fill as a percentage of the live epoch's capacity.
    pub fn fill_pct(&self) -> f64 {
        if self.pool_capacity == 0 { return 0.0; }
        100.0 * self.pool_notes as f64 / self.pool_capacity as f64
    }
}

fn http_json(client: &reqwest::blocking::Client, url: &str) -> Option<serde_json::Value> {
    client.get(url).send().ok()?.json().ok()
}

fn wire32(h: &str) -> Option<[u8; 32]> {
    let raw = hex::decode(h).ok()?;
    (raw.len() == 32).then(|| {
        let mut b = [0u8; 32];
        b.copy_from_slice(&raw);
        b
    })
}

/// Rebuild this seed's shielded position from the chain alone.
///
/// Walks EVERY epoch, not just the live one. Rotation seals a full generation and opens a
/// fresh one; the sealed generations keep holding spendable notes, and their delivery
/// ciphertexts exist only in their own archive — so a scanner that looked at the live epoch
/// only would quietly under-report a miner's balance the moment the pool first rotated,
/// which is exactly the class of bug this whole function exists to fix.
pub fn scan_shielded(node_url: &str, seed_hex: &str) -> Option<ShieldedSnapshot> {
    let seed = seed_bytes(seed_hex)?;
    let account = ShieldedAccount::from_seed(seed);
    let enc_id = sigil_shield::note_cipher::enc_identity_from_seed(&seed);
    let base = node_url.trim_end_matches('/');
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;

    let anchor = http_json(&client, &format!("{base}/v1/shielded/anchor"))?;
    let mut snap = ShieldedSnapshot {
        pool_notes: anchor.get("notes").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        pool_capacity: anchor.get("capacity").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        pool_locked: anchor
            .get("value_locked")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u128>().ok())
            .unwrap_or(0),
        epoch: anchor.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        sealed_epochs: anchor
            .get("sealed_epochs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        registered: anchor.get("registered").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        nullifiers: anchor.get("nullifiers").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        ..Default::default()
    };

    // The spent set is public by design — it is the double-spend guard — and it is the only
    // way a wallet learns about a spend it did not make on this device.
    let spent: std::collections::BTreeSet<[u8; 32]> =
        http_json(&client, &format!("{base}/v1/shielded/nullifiers"))
            .and_then(|v| v.get("nullifiers").and_then(|n| n.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().and_then(wire32))
            .collect();

    let mut store = sigil_shield::wallet::NoteStore::new();
    for epoch in 0..=snap.epoch {
        // Omitting `?epoch=` serves the live generation, so this is also correct against a
        // node that predates the epoch API.
        let url = if epoch == snap.epoch {
            format!("{base}/v1/shielded/leaves")
        } else {
            format!("{base}/v1/shielded/leaves?epoch={epoch}")
        };
        let Some(page) = http_json(&client, &url) else { continue };
        if page.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let leaves: Vec<[u8; 32]> = page
            .get("leaves")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().and_then(wire32)).collect())
            .unwrap_or_default();
        let cts: Vec<sigil_shield::note_cipher::NoteCiphertext> = page
            .get("ciphertexts")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| sigil_shield::note_cipher::NoteCiphertext(s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Trial decryption IS the ownership proof: the AEAD tag fails for everyone else, so
        // a successful open means the note was addressed to us. Nothing on-chain marks a
        // ciphertext as ours, which is precisely why an observer cannot tell who was paid.
        store.scan_ciphertexts(&enc_id, &cts);
        store.scan_owned(&account, &leaves);
    }
    store.mark_spent(&account, &spent);

    snap.balance = store.balance();
    snap.owned = store.notes.iter().filter(|n| n.position.is_some()).count();
    snap.spent = store.notes.iter().filter(|n| n.spent).count();
    Some(snap)
}

/// The most recent scan, shared with the UI thread.
///
/// A process-global rather than another field threaded through `App`: the scanner is a
/// pure reader of published chain state with no interaction with anything else in the
/// program, and the UI wants it from two different tabs.
static LATEST: std::sync::OnceLock<std::sync::Mutex<Option<ShieldedSnapshot>>> =
    std::sync::OnceLock::new();

fn latest_cell() -> &'static std::sync::Mutex<Option<ShieldedSnapshot>> {
    LATEST.get_or_init(|| std::sync::Mutex::new(None))
}

/// The last completed shielded scan, if any. Cheap; safe to call every frame.
pub fn latest_shielded() -> Option<ShieldedSnapshot> {
    latest_cell().lock().ok().and_then(|g| g.clone())
}

/// How often the shielded position is re-scanned.
///
/// The scan pulls the whole leaf+ciphertext page and trial-decrypts it, so it is far from
/// free — but it is local, and a miner watching their balance wants it to move on a human
/// timescale, not a frame timescale.
const SHIELDED_SCAN_EVERY: Duration = Duration::from_secs(45);

/// Keep [`latest_shielded`] fresh in the background.
///
/// Spawned wherever a seed is available. Without a seed there is nothing to scan — the
/// pool is hiding values from everyone who cannot open them, which very much includes us.
pub fn spawn_shielded_scanner(node_url: &str, seed_hex: &str) -> Option<std::thread::JoinHandle<()>> {
    let _ = seed_bytes(seed_hex)?; // reject a malformed seed once, here, not every cycle
    let node = node_url.to_string();
    let seed = seed_hex.to_string();
    Some(std::thread::spawn(move || loop {
        if let Some(snap) = scan_shielded(&node, &seed) {
            if let Ok(mut g) = latest_cell().lock() {
                *g = Some(snap);
            }
        }
        std::thread::sleep(SHIELDED_SCAN_EVERY);
    }))
}

/// Is this wallet published in the shielded registry?
///
/// One cheap GET. Used by the no-seed mining path to tell the difference between "rewards
/// will be transparent" (true for an unregistered wallet) and "rewards are private and
/// this process cannot see them" (true for a registered one) — two situations that look
/// identical from a frozen balance, and only one of which means anything is wrong.
pub fn wallet_is_registered(node_url: &str, wallet_hex: &str) -> bool {
    let url = format!(
        "{}/v1/shielded/address?wallet={wallet_hex}",
        node_url.trim_end_matches('/')
    );
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return false;
    };
    http_json(&client, &url)
        .map(|v| v.get("pk_shield").and_then(|p| p.as_str()).is_some_and(|s| !s.is_empty()))
        .unwrap_or(false)
}
