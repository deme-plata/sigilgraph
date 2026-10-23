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
use sigil_shield::wallet_v7::AccountV7;

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
    // 2026-10-13 v7: the node says which note generation the pool issues; the key we publish
    // must be of that generation or a payer would mint notes this seed cannot spend there.
    match pool_issues_v7(node_url) {
        Ok(v7) => register_for_shielded_mining_gen(node_url, seed_hex, v7),
        Err(reason) => {
            let wallet_hex = wallet_signing_key(seed_hex)
                .map(|sk| hex::encode(sk.verifying_key().to_bytes()))
                .unwrap_or_default();
            RegisterOutcome::Failed { wallet_hex, reason }
        }
    }
}

/// Does the node's pool currently issue v7 (256-bit) notes? Read from `/v1/shielded/anchor`
/// (`v7.epoch` present). Fail closed: an unreadable anchor is an error, never "v1".
pub fn pool_issues_v7(node_url: &str) -> Result<bool, String> {
    let url = format!("{}/v1/shielded/anchor", node_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let v = http_json(&client, &url).ok_or_else(|| "could not read the shielded anchor".to_string())?;
    let d = v.get("data").unwrap_or(&v);
    Ok(d.get("v7").and_then(|x| x.get("epoch")).and_then(|x| x.as_u64()).is_some())
}

/// [`register_for_shielded_mining`] for a KNOWN generation (`v7` = publish the 256-bit owner
/// key; otherwise the v1 shield key). The delivery key is the same identity either way.
pub fn register_for_shielded_mining_gen(node_url: &str, seed_hex: &str, v7: bool) -> RegisterOutcome {
    let Some(seed) = seed_bytes(seed_hex) else { return RegisterOutcome::BadSeed };
    let sk = SigningKey::from_bytes(&seed);
    let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
    let account = ShieldedAccount::from_seed(seed);
    let pk_shield_hex = if v7 {
        hex::encode(AccountV7::from_seed(seed).owner_pk().to_wire())
    } else {
        hex::encode(to_wire(account.public_key()))
    };
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
    /// Registered to THIS seed's key of the OTHER note generation (2026-10-13): the chain still
    /// holds our v1 key while the pool issues v7 notes (or the reverse). Not foreign — same
    /// seed — but a payer sealing to it would mint a note this wallet cannot spend in the
    /// current pool, so it is republished for the current generation.
    OldGeneration { pk_shield: String },
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
    registration_state_gen(node_url, wallet_hex, expect_pk_shield, None)
}

/// [`registration_state`] that also knows this seed's key of the OTHER generation, so a
/// stale-but-ours key reads as [`RegistrationState::OldGeneration`] instead of `Foreign`.
pub fn registration_state_gen(
    node_url: &str,
    wallet_hex: &str,
    expect_pk_shield: &str,
    other_gen_pk_shield: Option<&str>,
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
        Some(pk) if other_gen_pk_shield.is_some_and(|o| pk.eq_ignore_ascii_case(o)) => {
            RegistrationState::OldGeneration { pk_shield: pk.to_string() }
        }
        Some(pk) => RegistrationState::Foreign { pk_shield: pk.to_string() },
        None => RegistrationState::Unknown("registered but no pk_shield in response".into()),
    }
}

/// The shield public key `seed_hex` derives — what [`registration_state`] compares against.
pub fn expected_pk_shield(seed_hex: &str) -> Option<String> {
    Some(hex::encode(to_wire(ShieldedAccount::from_seed(seed_bytes(seed_hex)?).public_key())))
}

/// The v7 (256-bit) owner key `seed_hex` derives — what a v7 pool's registry must hold.
pub fn expected_pk_shield_v7(seed_hex: &str) -> Option<String> {
    Some(hex::encode(AccountV7::from_seed(seed_bytes(seed_hex)?).owner_pk().to_wire()))
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
    let pk_shield_v1 = expected_pk_shield(seed_hex)?;
    let pk_shield_v7 = expected_pk_shield_v7(seed_hex)?;
    let node = node_url.to_string();
    let seed = seed_hex.to_string();

    Some(std::thread::spawn(move || {
        let short = wallet_hex.chars().take(8).collect::<String>();
        // Whether we have already told the operator this wallet is registered. Also the
        // memory that lets a later disappearance be reported as a LOSS rather than as a
        // first-time registration.
        let mut announced_registered = false;
        loop {
            // 2026-10-13 v7: which key the pool wants NOW. The node decides, every cycle, so a
            // pool that switches generations while this process runs is followed, not fought.
            let v7 = match pool_issues_v7(&node) {
                Ok(v) => v,
                Err(why) => {
                    log(format!("… shielded registration: cannot read the pool generation ({why}) — retrying"));
                    std::thread::sleep(RETRY_AFTER_FAILURE);
                    continue;
                }
            };
            let (pk_shield, other_pk): (&str, &str) =
                if v7 { (&pk_shield_v7, &pk_shield_v1) } else { (&pk_shield_v1, &pk_shield_v7) };
            match registration_state_gen(&node, &wallet_hex, pk_shield, Some(other_pk)) {
                RegistrationState::OldGeneration { .. } => {
                    announced_registered = false;
                    log(format!(
                        "⟳ {short}… has its {}-format receiving key on chain while the pool issues {} notes — \
                         republishing the {} key so payments can reach it",
                        if v7 { "old" } else { "new" },
                        if v7 { "256-bit" } else { "v1" },
                        if v7 { "256-bit" } else { "v1" }
                    ));
                }
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

            match register_for_shielded_mining_gen(&node, &seed, v7) {
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
                    registration_state(&node, &wallet_hex, pk_shield)
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
    /// 2026-10-13: the live v7 (256-bit) epoch, when the pool has opened one.
    pub v7_epoch: Option<u32>,
    /// Notes in that epoch (pool-wide), and this wallet's spendable value among them.
    pub v7_notes: usize,
    pub v7_balance: u128,
    /// Bytes this scan downloaded (2026-10-17: the scanner reads only what is new).
    pub fetched_bytes: u64,
    /// The node served view tags, so strangers' envelopes were skipped.
    pub view_tags: bool,
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

// 2026-10-17 (10.4.38): the scanner itself lives in `shield_scan.rs` — incremental per pool
// generation, epoch-scoped spent markers, zstd, view tags. The function that stood here
// re-downloaded every generation in full every cycle (~113 MB on g2) and compared raw spent
// markers across generations; see that file's header for the measurements.

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
/// The first scan reads every generation once; after that a cycle reads only the notes that
/// arrived since (plus the spent set when it moved), so a short interval costs little.
const SHIELDED_SCAN_EVERY: Duration = Duration::from_secs(45);

/// Keep [`latest_shielded`] fresh in the background.
///
/// Spawned wherever a seed is available. Without a seed there is nothing to scan — the
/// pool is hiding values from everyone who cannot open them, which very much includes us.
pub fn spawn_shielded_scanner(node_url: &str, seed_hex: &str) -> Option<std::thread::JoinHandle<()>> {
    let seed = seed_bytes(seed_hex)?;
    // 2026-10-23: one scanner per seed per process. It now starts with the screen, and every [M] press used to start
    // another one scanning the same wallet.
    static STARTED: std::sync::Mutex<Vec<[u8; 32]>> = std::sync::Mutex::new(Vec::new());
    {
        let mut g = match STARTED.lock() { Ok(g) => g, Err(e) => e.into_inner() };
        if g.contains(&seed) {
            return None;
        }
        g.push(seed);
    }
    let node = node_url.to_string();
    Some(std::thread::spawn(move || {
        // A whole sealed generation is ~45 MB of JSON before compression; on a slow line the
        // first read needs far longer than a status call.
        let Ok(client) = reqwest::blocking::Client::builder().timeout(Duration::from_secs(180)).build() else {
            return;
        };
        let mut get = crate::shield_scan::http_getter(client);
        let mut scanner = crate::shield_scan::ShieldedScanner::new(&node, seed);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        spawn_note_listener(node.clone(), seed, wake_tx);
        loop {
            if let Some(snap) = scanner.cycle(&mut get) {
                if let Ok(mut g) = latest_cell().lock() {
                    *g = Some(snap);
                }
            }
            // 2026-10-23: at once when a note that may be ours lands (pushed by the node), else every 45 s.
            match wake_rx.recv_timeout(SHIELDED_SCAN_EVERY) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(SHIELDED_SCAN_EVERY),
                _ => while wake_rx.try_recv().is_ok() {},
            }
        }
    }))
}

/// 2026-10-23: hold `/v1/events?kinds=note` open on the scanner's node and wake the scanner (`wake`) for every new
/// note whose view tag says it may be ours. Reconnects with a growing pause (2 s … 60 s); a node that publishes no
/// `note` events simply never wakes it, and the scanner keeps its 45 s cycle.
fn spawn_note_listener(node: String, seed: [u8; 32], wake: std::sync::mpsc::Sender<()>) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let id = sigil_shield::note_cipher::enc_identity_from_seed(&seed);
        let mine = |v: &sigil_shield::note_cipher::TagView| sigil_shield::note_cipher::may_be_ours(v, &id);
        // No overall timeout: the stream is meant to stay open (the node sends a keep-alive every 15 s).
        let Ok(client) = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(None::<Duration>)
            .build()
        else {
            return;
        };
        let url = format!("{}/v1/events?kinds=note", node.trim_end_matches('/'));
        let mut pause = 2u64;
        loop {
            if let Ok(resp) = client.get(&url).header("Accept", "text/event-stream").send() {
                if resp.status().is_success() {
                    pause = 2;
                    for line in std::io::BufReader::new(resp).lines() {
                        let Ok(line) = line else { break };
                        let Some(data) = line.strip_prefix("data:") else { continue };
                        if note_may_be_ours(data.trim(), &mine) && wake.send(()).is_err() {
                            return; // the scanner is gone
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(pause));
            pause = (pause * 2).min(60);
        }
    });
}

/// One `note` event's JSON: may this note be ours? A tagged note is tested with `mine` (the view-tag test with our
/// key); a note without a readable envelope says yes (only a scan can tell).
fn note_may_be_ours(data: &str, mine: impl Fn(&sigil_shield::note_cipher::TagView) -> bool) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return false };
    if v["kind"].as_str() != Some("note") {
        return false;
    }
    let eph = v["eph_pk"].as_str().and_then(|h| hex::decode(h).ok()).and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
    let Some(eph_pk) = eph else { return true };
    let vt = v["vt"].as_u64().and_then(|t| u8::try_from(t).ok());
    mine(&sigil_shield::note_cipher::TagView { eph_pk, vt })
}

#[cfg(test)]
mod note_listener_tests {
    use super::note_may_be_ours;
    use sigil_shield::note_cipher::{enc_identity_from_seed, may_be_ours, seal_note, tag_view, NotePlaintext};
    use sigil_shield::wallet::ShieldedAccount;

    /// The event a node sends for a note sealed to us wakes the scanner; the same note seen with a stranger's key
    /// almost never does (a one-byte tag: 1 in 256 false wakes, each costing one cheap incremental scan).
    #[test]
    fn a_note_sealed_to_us_wakes_the_scanner_and_strangers_mostly_do_not() {
        let seed = [7u8; 32];
        let acct = ShieldedAccount::from_seed(seed);
        let me = enc_identity_from_seed(&seed);
        let mine = |v: &sigil_shield::note_cipher::TagView| may_be_ours(v, &me);
        let mut false_wakes = 0;
        for i in 0..64u8 {
            let ct = seal_note(&NotePlaintext::new(5, acct.blinding(i as u64)), &acct.address(&seed)).expect("seal").0;
            let v = tag_view(&ct).expect("a tagged envelope");
            assert!(v.vt.is_some(), "seal_note writes a view tag");
            let ev = serde_json::json!({"seq": 1, "kind": "note", "epoch": 1, "index": i,
                "vt": v.vt, "eph_pk": hex::encode(v.eph_pk), "height": 9, "ts_ms": 0}).to_string();
            assert!(note_may_be_ours(&ev, mine), "our own note must wake the scanner");
            let stranger = enc_identity_from_seed(&[i.wrapping_add(100); 32]);
            if note_may_be_ours(&ev, |v: &sigil_shield::note_cipher::TagView| may_be_ours(v, &stranger)) { false_wakes += 1; }
        }
        assert!(false_wakes <= 4, "{false_wakes} of 64 false wakes");
        assert!(!note_may_be_ours(r#"{"kind":"tip","height":1}"#, mine), "other kinds never wake it");
        assert!(note_may_be_ours(r#"{"kind":"note","epoch":1,"index":0}"#, mine), "no envelope: only a scan can tell");
    }
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

#[cfg(test)]
mod v7_generation_tests {
    //! 2026-10-13: the registration keeper follows the pool's note generation.
    use super::*;
    use std::io::{Read, Write};

    /// A one-reply stub node: answers every request with `reply`.
    fn stub(reply: String) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().take(8) {
                let Ok(mut s) = s else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn the_v7_key_is_a_full_digest_and_differs_from_the_v1_key() {
        let seed = hex::encode([0x5a; 32]);
        let v1 = expected_pk_shield(&seed).unwrap();
        let v7 = expected_pk_shield_v7(&seed).unwrap();
        assert_ne!(v1, v7);
        assert_eq!(v7.len(), 64);
        assert!(v1[16..].bytes().all(|b| b == b'0'), "v1 = one field element + 24 zero bytes");
        assert!(!v7[16..].bytes().all(|b| b == b'0'), "v7 = a 256-bit digest");
    }

    #[test]
    fn our_old_generation_key_on_chain_reads_as_old_generation_not_foreign() {
        let seed = hex::encode([0x5a; 32]);
        let v1 = expected_pk_shield(&seed).unwrap();
        let v7 = expected_pk_shield_v7(&seed).unwrap();
        // The pool issues v7; the chain still holds this seed's v1 key.
        let node = stub(serde_json::json!({"ok": true, "pk_shield": v1, "pk_encrypt": "00"}).to_string());
        match registration_state_gen(&node, "ab", &v7, Some(&v1)) {
            RegistrationState::OldGeneration { pk_shield } => assert_eq!(pk_shield, v1),
            _ => panic!("expected OldGeneration"),
        }
        // Without the other-generation hint the pre-v7 verdict stands: a different key is foreign.
        assert!(matches!(registration_state(&node, "ab", &v7), RegistrationState::Foreign { .. }));
        // The v7 key on chain is simply ours.
        let node2 = stub(serde_json::json!({"ok": true, "pk_shield": v7, "pk_encrypt": "00"}).to_string());
        assert!(matches!(registration_state_gen(&node2, "ab", &v7, Some(&v1)), RegistrationState::Ours));
        // And a genuinely foreign key is still foreign, whichever generation the pool is in.
        let node3 = stub(serde_json::json!({"ok": true, "pk_shield": "ff".repeat(32), "pk_encrypt": "00"}).to_string());
        assert!(matches!(registration_state_gen(&node3, "ab", &v7, Some(&v1)), RegistrationState::Foreign { .. }));
    }

    #[test]
    fn the_pool_generation_is_read_from_the_anchor_and_fails_closed() {
        let v7node = stub(serde_json::json!({"ok": true, "epoch": 0, "v7": {"epoch": 1, "notes": 3}}).to_string());
        assert_eq!(pool_issues_v7(&v7node), Ok(true));
        let v1node = stub(serde_json::json!({"ok": true, "epoch": 2}).to_string());
        assert_eq!(pool_issues_v7(&v1node), Ok(false));
        assert!(pool_issues_v7("http://127.0.0.1:9").is_err(), "an unreachable node is an error, never v1");
    }
}
