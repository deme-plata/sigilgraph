//! One wallet everywhere — the SAME recovery phrase opens the same SIGIL wallet in sigil-top,
//! on sigilgraph.org and in the phone app (Viktor, 2026-10-09: "ja byg det og gem sætningen i
//! sigil-top").
//!
//! Before this, sigil-top quietly created its OWN random 64-hex key the first time it ran
//! without a wallet, and mined to it. A user who had made a wallet on sigilgraph.org then had
//! two wallets with two keys, and [W] opened the second one by address only ("This browser
//! holds NO recovery phrase for this wallet").
//!
//! The derivation is a line-for-line port of the phone's `sigil-mobile-core::derive`, which in
//! turn mirrors `schemeFor()` + `sigilDeriveAuto()` in the browser:
//!
//! * a bare 64-hex string (optionally `0x`) is a RAW seed — how every pre-10.4.22 sigil-top
//!   wallet, mine-rig and MCP wallet is addressed;
//! * `v1:` / `v2:` prefixes force a scheme and are stripped; `mm2:` (a MetaMask-derived wallet)
//!   is v2 and KEEPS its prefix in the hashed material;
//! * otherwise twelve or more words are v2 = Argon2id(phrase, "sigilgraph-wallet-v2", t=3,
//!   m=16 MiB, p=1), fewer are v1 = SHA3-256(phrase).
//!
//! The phrase is only trimmed — never lowercased or re-spaced — exactly like both other clients.
//! The address is the hex Ed25519 public key of the 32-byte secret. The tests pin addresses that
//! were computed with sigilgraph.org's OWN vendored crypto modules (sigil-argon2.mjs,
//! sigil-ed25519.mjs, sigil-sha3.mjs), so "same phrase → same wallet" is measured, not assumed.
//!
//! Storage: the derived secret stays where every existing reader looks for it
//! (`<home>/.flux/sigil-wallet-seed`, 64-hex), and the phrase itself goes beside it
//! (`<home>/.flux/sigil-wallet-phrase`). Both are owner-only (0600) on Unix; on Windows they live
//! in the user's own profile folder (see `adopt_windows_home`), which only that user can read.
//! Nothing here ever deletes a key: a replaced wallet's files are renamed `*.replaced-<ts>`.

use sha3::{Digest, Sha3_256};

/// The 2,159-word list — byte-identical to the phone's `assets/wordlist.json` and to the
/// `WL` array on sigilgraph.org's `enter-sigil.html` (compared 2026-10-09).
const WORDLIST_JSON: &str = include_str!("../assets/wordlist.json");

const V2_SALT: &[u8] = b"sigilgraph-wallet-v2";
const V2_T_COST: u32 = 3;
const V2_M_COST_KIB: u32 = 16 * 1024;
const V2_PARALLELISM: u32 = 1;

fn wordlist() -> &'static [String] {
    static W: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    W.get_or_init(|| serde_json::from_str(WORDLIST_JSON).expect("embedded wordlist.json is valid"))
}

fn is_hex64(s: &str) -> bool {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `(is_v2, material)` — mirrors the phone's `scheme_for` and the browser's `schemeFor`.
fn scheme_for(t: &str) -> (bool, String) {
    if t.len() >= 3 && t[..3].eq_ignore_ascii_case("v1:") {
        return (false, t[3..].trim().to_string());
    }
    if t.len() >= 3 && t[..3].eq_ignore_ascii_case("v2:") {
        return (true, t[3..].trim().to_string());
    }
    if t.len() >= 4 && t[..4].eq_ignore_ascii_case("mm2:") {
        return (true, t.to_string());
    }
    (t.split_whitespace().count() >= 12, t.to_string())
}

/// The 32-byte secret a phrase (or a raw 64-hex seed) opens.
pub(crate) fn derive_secret(phrase: &str) -> Result<[u8; 32], String> {
    let t = phrase.trim();
    let mut secret = [0u8; 32];
    if is_hex64(t) {
        let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
        hex::decode_to_slice(h, &mut secret).map_err(|e| format!("bad hex seed: {e}"))?;
        return Ok(secret);
    }
    let (v2, material) = scheme_for(t);
    if material.is_empty() {
        return Err("the recovery phrase is empty".into());
    }
    if v2 {
        let params = argon2::Params::new(V2_M_COST_KIB, V2_T_COST, V2_PARALLELISM, Some(32))
            .map_err(|e| format!("argon2 params: {e}"))?;
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(material.as_bytes(), V2_SALT, &mut secret)
            .map_err(|e| format!("argon2: {e}"))?;
    } else {
        secret.copy_from_slice(&Sha3_256::digest(material.as_bytes()));
    }
    Ok(secret)
}

/// The wallet address (hex Ed25519 public key) of a secret — the same key the miner signs with.
pub(crate) fn address_of(secret: &[u8; 32]) -> String {
    sigil_oauth::Keypair::from_seed(secret).pubkey_hex()
}

/// Twelve fresh words from the shared list (132.9 bits), with rejection sampling exactly like
/// sigilgraph.org's `createWallet()` so no word is even slightly favoured.
pub(crate) fn generate_phrase() -> String {
    let wl = wordlist();
    let n = wl.len() as u64;
    let limit = (0x1_0000_0000u64 / n) * n;
    let mut words: Vec<&str> = Vec::with_capacity(12);
    while words.len() < 12 {
        let pool = sigil_oauth::random_seed();
        for c in pool.chunks_exact(4) {
            let r = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64;
            if r < limit && words.len() < 12 {
                words.push(&wl[(r % n) as usize]);
            }
        }
    }
    words.join(" ")
}

/// Is every word of a typed phrase in the list? (`None` = not a word phrase at all, e.g. a
/// 64-hex seed or a prefixed/MetaMask phrase, which is checked by derivation instead.)
pub(crate) fn unknown_words(phrase: &str) -> Option<Vec<String>> {
    let t = phrase.trim();
    if is_hex64(t) || t.contains(':') {
        return None;
    }
    let wl = wordlist();
    Some(t.split_whitespace().filter(|w| !wl.iter().any(|x| x == w)).map(str::to_string).collect())
}

pub(crate) fn seed_path_in(home: &str) -> String { format!("{home}/.flux/sigil-wallet-seed") }
pub(crate) fn phrase_path_in(home: &str) -> String { format!("{home}/.flux/sigil-wallet-phrase") }

/// The phrase this install keeps, if any.
pub(crate) fn stored_phrase_in(home: &str) -> Option<String> {
    std::fs::read_to_string(phrase_path_in(home)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn write_private(path: &str, contents: &str) -> Result<(), String> {
    use std::io::Write;
    let tmp = format!("{path}.new");
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp).map_err(|e| format!("cannot write {tmp}: {e}"))?;
    f.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot move {tmp} into place: {e}"))
}

/// What `install_in` did.
#[derive(Debug, PartialEq)]
pub(crate) struct Installed {
    pub address: String,
    /// The address of the wallet this replaced (its files kept as `*.replaced-<ts>`).
    pub replaced: Option<String>,
}

/// Make `phrase` this install's wallet: the derived secret goes to the seed file every reader
/// already uses, the phrase beside it. An existing, DIFFERENT wallet is never deleted — its
/// files are renamed `*.replaced-<ts>` and its address is returned so the caller can say where
/// its coins are. Installing the same wallet again only (re)writes the phrase.
pub(crate) fn install_in(home: &str, phrase: &str) -> Result<Installed, String> {
    let phrase = phrase.trim();
    let secret = derive_secret(phrase)?;
    let address = address_of(&secret);
    let seed_hex = hex::encode(secret);
    std::fs::create_dir_all(format!("{home}/.flux")).map_err(|e| format!("cannot create {home}/.flux: {e}"))?;
    let (seed_p, phrase_p) = (seed_path_in(home), phrase_path_in(home));

    let mut replaced = None;
    if let Ok(old) = std::fs::read_to_string(&seed_p) {
        let old = old.trim().to_string();
        let mut old_secret = [0u8; 32];
        let same = hex::decode_to_slice(&old, &mut old_secret).is_ok() && old_secret == secret;
        if !same {
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if hex::decode_to_slice(&old, &mut old_secret).is_ok() {
                replaced = Some(address_of(&old_secret));
            }
            std::fs::rename(&seed_p, format!("{seed_p}.replaced-{ts}")).map_err(|e| format!("cannot keep the old key aside: {e}"))?;
            if std::path::Path::new(&phrase_p).exists() {
                std::fs::rename(&phrase_p, format!("{phrase_p}.replaced-{ts}")).map_err(|e| format!("cannot keep the old phrase aside: {e}"))?;
            }
        }
    }
    write_private(&seed_p, &seed_hex)?;
    write_private(&phrase_p, phrase)?;
    Ok(Installed { address, replaced })
}

/// Windows has no `HOME`. sigil-top's home was `HOME` with a `/root` fallback, so on Windows every
/// key, note index and setting landed in `C:\root\.flux` — a folder any user of the PC can read.
/// From 10.4.22 the home is the user's own profile folder. A legacy `C:\root\.flux` is copied
/// over file by file (nothing in the new place is overwritten, nothing in the old place is
/// removed), so an existing install keeps its wallet. Returns the files copied.
pub(crate) fn migrate_legacy_home(legacy_flux: &std::path::Path, new_flux: &std::path::Path) -> std::io::Result<usize> {
    fn walk(from: &std::path::Path, to: &std::path::Path, n: &mut usize) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if e.file_type()?.is_dir() {
                walk(&src, &dst, n)?;
            } else if !dst.exists() {
                std::fs::copy(&src, &dst)?;
                *n += 1;
            }
        }
        Ok(())
    }
    let mut n = 0;
    if legacy_flux.is_dir() {
        walk(legacy_flux, new_flux, &mut n)?;
    }
    Ok(n)
}

/// Call first thing in `main()`: on Windows with no `HOME`, make the user's profile folder the
/// home (and bring a legacy `C:\root\.flux` along). No-op everywhere else.
pub(crate) fn adopt_windows_home() {
    #[cfg(windows)]
    {
        if std::env::var_os("HOME").is_some() {
            return;
        }
        let Some(profile) = std::env::var_os("USERPROFILE") else { return };
        let profile = std::path::PathBuf::from(profile);
        let legacy = std::path::PathBuf::from("/root").join(".flux");
        let copied = migrate_legacy_home(&legacy, &profile.join(".flux")).unwrap_or(0);
        std::env::set_var("HOME", &profile);
        crate::boot_trace(&format!("home = {} (Windows profile; {copied} file(s) brought over from {})",
            profile.display(), legacy.display()));
    }
}

// ── the `sigil-top wallet` command and the first-run choice ────────────────────────────

/// What to tell the user about a phrase BEFORE installing it (empty = nothing to say).
///
/// 2026-10-14 (Viktor: every new wallet is 12 words, keep it secure). A phrase with words outside
/// the list is not refused any more: the sigilgraph.org front page made 6-word phrases from its own
/// 50-word list, and those wallets are real. The owner sees the address and decides. A phrase of
/// fewer than 12 words is a weak wallet whoever made it, so that is said plainly.
pub(crate) fn phrase_warnings(phrase: &str) -> Vec<String> {
    let t = phrase.trim();
    let mut out = Vec::new();
    if let Some(bad) = unknown_words(t) {
        if !bad.is_empty() {
            out.push(format!(
                "Not in the SIGIL word list: {}. Phrases from the old sigilgraph.org front page look like this; a typo would open a different, empty wallet. Check that the address below is the one you know.",
                bad.join(", ")
            ));
        }
    }
    let (v2, material) = scheme_for(t);
    let words = material.split_whitespace().count();
    if !is_hex64(t) && !v2 && !t.to_ascii_lowercase().starts_with("mm2:") && words < 12 {
        out.push(format!(
            "Short phrase ({words} words): anyone can guess it. Move your coins to a new 12-word wallet (`sigil-top wallet new --replace`, then send from the old one)."
        ));
    }
    out
}

fn ask(prompt: &str) -> String {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

fn show_words(phrase: &str) {
    println!();
    println!("  ┌─ YOUR RECOVERY PHRASE ─────────────────────────────────────────────┐");
    for (i, chunk) in phrase.split_whitespace().collect::<Vec<_>>().chunks(4).enumerate() {
        let line: Vec<String> = chunk.iter().enumerate().map(|(j, w)| format!("{:>2}. {w:<10}", i * 4 + j + 1)).collect();
        println!("  │  {:<66}│", line.join(" "));
    }
    println!("  └────────────────────────────────────────────────────────────────────┘");
    println!("  Write these words down. They open THIS wallet on https://sigilgraph.org and in the phone");
    println!("  app, and they are the only way back in if this computer is lost. sigil-top keeps a copy");
    println!("  (`sigil-top wallet phrase` shows it).");
    println!();
}

/// Import a phrase from the terminal. `confirm_replace` asks before replacing a different wallet.
fn import_interactive(home: &str) -> Result<Installed, String> {
    let phrase = ask("  Recovery phrase from sigilgraph.org or the phone app (12 words, or a 64-character key): ");
    if phrase.is_empty() {
        return Err("no phrase entered — nothing changed".into());
    }
    let warnings = phrase_warnings(&phrase);
    for w in &warnings {
        println!("  ⚠ {w}");
    }
    let address = address_of(&derive_secret(&phrase)?);
    println!("  This phrase opens wallet {address}");
    if !warnings.is_empty() && ask("  Type yes to use this wallet anyway: ") != "yes" {
        return Err("not installed — nothing changed".into());
    }
    if let Ok(old) = std::fs::read_to_string(seed_path_in(home)) {
        let mut s = [0u8; 32];
        if hex::decode_to_slice(old.trim(), &mut s).is_ok() && address_of(&s) != address {
            println!("  It REPLACES this install's wallet {} — that key is kept aside on disk, not deleted,", address_of(&s));
            println!("  and any coins in it stay in it (log in with its old key to move them).");
            if ask("  Type yes to switch: ") != "yes" {
                return Err("not switched — nothing changed".into());
            }
        }
    }
    install_in(home, &phrase)
}

fn new_wallet(home: &str) -> Result<Installed, String> {
    let phrase = generate_phrase();
    let got = install_in(home, &phrase)?;
    show_words(&phrase);
    Ok(got)
}

fn after_install(got: &Installed) {
    crate::set_mine_wallet(&got.address);
    println!("  ✓ wallet {} is this install's wallet — mining, [W] and sending all use it.", got.address);
    if let Some(old) = &got.replaced {
        println!("  The previous wallet {old} is kept aside (*.replaced-* in ~/.flux), with its coins.");
    }
    println!("  If sigil-top is running, restart it so the miner picks this up.");
    publish_receiving_key();
}

/// 2026-10-15: a new or imported wallet publishes its private receiving key right away, so it
/// can be paid before sigil-top is ever started (the boot keeper would only do it later).
/// Asks the chain first; a wallet already registered to this key costs nothing.
#[cfg(feature = "shield-register")]
fn publish_receiving_key() {
    use crate::shield_setup::{self as ss, RegisterOutcome, RegistrationState};
    // The file just written — not `miner_seed`, which prefers an env var naming another wallet.
    let Some(seed_hex) = std::fs::read_to_string(seed_path_in(&crate::flux_home())).ok()
        .and_then(|h| derive_secret(h.trim()).ok()).map(hex::encode) else { return };
    let (Some(sk), Some(pk)) = (ss::wallet_signing_key(&seed_hex), ss::expected_pk_shield(&seed_hex)) else { return };
    let wallet = hex::encode(sk.verifying_key().to_bytes());
    let node = crate::top_core::money_node_url();
    match ss::registration_state(&node, &wallet, &pk) {
        RegistrationState::Ours => println!("  ✓ it can receive private payments (receiving key already on the chain)."),
        RegistrationState::Foreign { .. } => println!("  ⚠ the chain holds a DIFFERENT receiving key for this address — payments to it could not be spent here."),
        RegistrationState::Unknown(why) => println!("  … could not reach the network ({why}); sigil-top publishes the receiving key when it starts."),
        // OldGeneration: the chain holds this seed's key of the other note generation — republish
        // for the current one, exactly as the boot keeper does (shield_setup.rs).
        RegistrationState::Absent | RegistrationState::OldGeneration { .. } => match ss::register_for_shielded_mining(&node, &seed_hex) {
            RegisterOutcome::Registered { .. } => println!("  ✓ receiving key published — this wallet can be paid privately within about a minute."),
            RegisterOutcome::Failed { reason, .. } => println!("  ⚠ publishing the receiving key failed ({reason}); sigil-top retries when it starts."),
            RegisterOutcome::BadSeed => {}
        },
    }
}
#[cfg(not(feature = "shield-register"))]
fn publish_receiving_key() {}

/// `sigil-top wallet [show | phrase | import | new [--replace]]`
pub(crate) fn run_cli(args: &[String]) {
    let home = crate::flux_home();
    let has_wallet = std::path::Path::new(&seed_path_in(&home)).exists();
    match args.first().map(|s| s.as_str()).unwrap_or("show") {
        "show" => {
            match std::fs::read_to_string(seed_path_in(&home)).ok().and_then(|h| derive_secret(&h).ok()) {
                Some(secret) => {
                    println!("\n  wallet   {}", address_of(&secret));
                    println!("  key      {} (only your user can read it)", seed_path_in(&home));
                    match stored_phrase_in(&home) {
                        Some(_) => println!("  phrase   kept by sigil-top — `sigil-top wallet phrase` shows it"),
                        None => println!("  phrase   none (made before 10.4.22) — its 64-character key is its phrase: `sigil-top wallet phrase`"),
                    }
                    println!("  The same phrase opens this wallet on https://sigilgraph.org and in the phone app.\n");
                }
                None => println!("\n  no wallet yet — `sigil-top wallet import` (use your sigilgraph.org / phone wallet) or `sigil-top wallet new`\n"),
            }
        }
        "phrase" => match (stored_phrase_in(&home), std::fs::read_to_string(seed_path_in(&home))) {
            (Some(p), _) => {
                println!("\n  ⚠ anyone who sees these words can spend this wallet.");
                show_words(&p);
            }
            (None, Ok(key)) => {
                println!("\n  ⚠ anyone who sees this key can spend this wallet.");
                println!("  This wallet was made before 10.4.22 and has no word phrase. Its key IS its phrase —");
                println!("  paste it where the 12 words go on https://sigilgraph.org or in the phone app:\n");
                println!("     {}\n", key.trim());
            }
            _ => println!("\n  no wallet yet — `sigil-top wallet import` or `sigil-top wallet new`\n"),
        },
        "import" => match import_interactive(&home) {
            Ok(got) => after_install(&got),
            Err(e) => eprintln!("  ✗ {e}"),
        },
        "new" => {
            if has_wallet && !args.iter().any(|a| a == "--replace") {
                eprintln!("  ✗ this install already has a wallet (`sigil-top wallet show`). `sigil-top wallet new --replace`");
                eprintln!("    makes a new one and keeps the old key aside; nothing is ever deleted.");
                return;
            }
            match new_wallet(&home) {
                Ok(got) => after_install(&got),
                Err(e) => eprintln!("  ✗ {e}"),
            }
        }
        other => eprintln!("  unknown `wallet {other}` — use: show | phrase | import | new [--replace]"),
    }
}

/// Before the dashboard starts on an install with NO wallet: ask, instead of silently creating
/// a second, unrelated wallet. Only on a real terminal and only when nothing names a wallet
/// already (`SIGIL_MINE_SEED`, `SIGIL_MINE_WALLET`, a chosen mining wallet, a seed file);
/// otherwise the old path stands (a wallet is created automatically — now with a phrase).
pub(crate) fn first_run_choice() {
    use std::io::IsTerminal;
    let home = crate::flux_home();
    let named = std::env::var("SIGIL_MINE_SEED").is_ok()
        || std::env::var("SIGIL_MINE_WALLET").is_ok()
        || std::path::Path::new(&seed_path_in(&home)).exists()
        || std::path::Path::new(&crate::mine_wallet_path()).exists();
    if named || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return;
    }
    println!("\n  SIGIL wallet — this install has none yet.\n");
    println!("   [1] I already have a SIGIL wallet (made on sigilgraph.org or in the phone app)");
    println!("   [2] Make a new wallet (12 words — the same words also open it on sigilgraph.org and the phone)\n");
    let res = match ask("  Choose 1 or 2 [2]: ").as_str() {
        "1" => import_interactive(&home),
        _ => new_wallet(&home),
    };
    match res {
        Ok(got) => {
            after_install(&got);
            let _ = ask("  Press Enter to open the dashboard… ");
        }
        Err(e) => {
            eprintln!("  ✗ {e}");
            let _ = ask("  (a wallet will be made for you; `sigil-top wallet import` switches to yours later) Enter… ");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Computed 2026-10-09 with sigilgraph.org's OWN modules (sigil-argon2.mjs, sigil-ed25519.mjs,
    /// sigil-sha3.mjs, sigil-sha512.mjs) — the browser wallet's derivation, not a re-implementation.
    #[test]
    fn same_phrase_same_wallet_as_sigilgraph_org() {
        let cases = [
            ("abandon ability able about above absent absorb abstract absurd abuse access accident",
             "f31437fff073dd8c2355d45050b038723e0babf273be0b5691bc6512f1139be9"), // v2, 12 words
            ("xenon yield abandon ability able about",
             "92f0aa2e7844ea7c68c408f561b9dbe89a5cb09db03f73460086157e3f3b5b25"), // v1, 6 words
            ("0707070707070707070707070707070707070707070707070707070707070707",
             "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"), // raw 64-hex seed
        ];
        for (phrase, want) in cases {
            assert_eq!(address_of(&derive_secret(phrase).unwrap()), want, "{phrase}");
        }
        // only trimmed: surrounding whitespace does not change the wallet
        assert_eq!(address_of(&derive_secret("  0x0707070707070707070707070707070707070707070707070707070707070707 \n").unwrap()),
                   "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c");
        // a forced scheme prefix: v2: on the 12 words is the same wallet
        assert_eq!(address_of(&derive_secret("v2:abandon ability able about above absent absorb abstract absurd abuse access accident").unwrap()),
                   "f31437fff073dd8c2355d45050b038723e0babf273be0b5691bc6512f1139be9");
    }

    /// The old front page's 6-word phrases are real wallets: warned about, never refused, and
    /// they open the same address the site's own crypto gives (vector measured 2026-10-14).
    #[test]
    fn front_page_six_word_phrase_warns_and_opens_its_address() {
        let p = "ash birch cedar delta ember fern";
        let w = phrase_warnings(p);
        assert!(w.iter().any(|m| m.starts_with("Short phrase (6 words)")), "{w:?}");
        assert_eq!(
            address_of(&derive_secret(p).unwrap()),
            "dfc5ba9349367a88c6d243f033ef42f05cc6d6ebd8bf7d1fc416c73ec69d865f"
        );
    }

    #[test]
    fn twelve_listed_words_and_hex_keys_get_no_warning() {
        assert!(phrase_warnings(&generate_phrase()).is_empty());
        assert!(phrase_warnings(&"ab".repeat(32)).is_empty());
    }

    #[test]
    fn unknown_word_is_a_warning_not_a_refusal() {
        let mut words: Vec<String> = generate_phrase().split(' ').map(str::to_string).collect();
        words[3] = "zzzqqq".into();
        let w = phrase_warnings(&words.join(" "));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("zzzqqq"));
        assert!(derive_secret(&words.join(" ")).is_ok());
    }

    #[test]
    fn generated_phrases_are_twelve_listed_words_and_differ() {
        let (a, b) = (generate_phrase(), generate_phrase());
        assert_ne!(a, b);
        assert_eq!(a.split_whitespace().count(), 12);
        assert_eq!(unknown_words(&a), Some(vec![]));
        assert!(derive_secret(&a).is_ok());
        assert_eq!(unknown_words("abandon notaword"), Some(vec!["notaword".to_string()]));
        assert_eq!(unknown_words(&"07".repeat(32)), None);
    }

    fn tmp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("sigil-wallet-phrase-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn install_keeps_the_phrase_and_never_deletes_a_replaced_wallet() {
        let home = tmp("install");
        // an old, auto-created raw-seed wallet
        std::fs::create_dir_all(format!("{home}/.flux")).unwrap();
        std::fs::write(seed_path_in(&home), "07".repeat(32)).unwrap();
        let words = "abandon ability able about above absent absorb abstract absurd abuse access accident";
        let got = install_in(&home, words).unwrap();
        assert_eq!(got.address, "f31437fff073dd8c2355d45050b038723e0babf273be0b5691bc6512f1139be9");
        assert_eq!(got.replaced.as_deref(), Some("ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"));
        assert_eq!(stored_phrase_in(&home).as_deref(), Some(words));
        // the seed file now holds the derived secret, which is what the miner reads
        let seed = std::fs::read_to_string(seed_path_in(&home)).unwrap();
        assert_eq!(address_of(&derive_secret(&seed).unwrap()), got.address);
        // the old key is still on disk
        let kept: Vec<_> = std::fs::read_dir(format!("{home}/.flux")).unwrap()
            .filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("sigil-wallet-seed.replaced-")).collect();
        assert_eq!(kept.len(), 1);
        // installing the same wallet again replaces nothing
        assert_eq!(install_in(&home, words).unwrap().replaced, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [seed_path_in(&home), phrase_path_in(&home)] {
                assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{p}");
            }
        }
    }

    #[test]
    fn legacy_windows_home_is_copied_without_overwriting() {
        let (old, new) = (tmp("legacy"), tmp("profile"));
        let (old_f, new_f) = (std::path::Path::new(&old).join(".flux"), std::path::Path::new(&new).join(".flux"));
        std::fs::create_dir_all(old_f.join("notes")).unwrap();
        std::fs::write(old_f.join("sigil-wallet-seed"), "old-seed").unwrap();
        std::fs::write(old_f.join("notes").join("idx"), "7").unwrap();
        std::fs::create_dir_all(&new_f).unwrap();
        std::fs::write(new_f.join("keep-me"), "new").unwrap();
        assert_eq!(migrate_legacy_home(&old_f, &new_f).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(new_f.join("sigil-wallet-seed")).unwrap(), "old-seed");
        assert_eq!(std::fs::read_to_string(new_f.join("notes").join("idx")).unwrap(), "7");
        assert_eq!(std::fs::read_to_string(new_f.join("keep-me")).unwrap(), "new");
        // a second run copies nothing and the legacy files are untouched
        assert_eq!(migrate_legacy_home(&old_f, &new_f).unwrap(), 0);
        assert!(old_f.join("sigil-wallet-seed").exists());
    }
}
