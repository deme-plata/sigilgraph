//! The vectors that make "same words → same wallet" a measurement:
//! * the 24 official BIP39 (Trezor) vectors — words ↔ entropy and the seed for password "TREZOR";
//! * v3 vectors computed by an INDEPENDENT JavaScript implementation (noble crypto, the code
//!   sigilgraph.org vendors) — `sigil-scratch/sigil-wallet-v3-1027/ref/v3-reference.mjs`;
//! * the v1 / v2 / raw-key addresses sigil-top already pins, which were computed with
//!   sigilgraph.org's own crypto modules.
use sha2::Digest as _;
use sigil_wallet_phrase::*;

fn address(secret: &[u8; 32]) -> String {
    hex::encode(ed25519_dalek::SigningKey::from_bytes(secret).verifying_key().to_bytes())
}

fn json(name: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
}

const ABOUT: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

#[test]
fn word_list_is_the_official_bip39_english_list() {
    let file = std::fs::read(format!("{}/assets/bip39-english.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert_eq!(hex::encode(sha2::Sha256::digest(&file)), "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda");
}

#[test]
fn official_bip39_vectors() {
    let v = json("bip39_trezor_vectors.json");
    let v = v.as_array().unwrap();
    assert_eq!(v.len(), 24);
    for x in v {
        let (ent, words, seed) = (x["entropy"].as_str().unwrap(), x["words"].as_str().unwrap(), x["seed_trezor"].as_str().unwrap());
        assert_eq!(bip39_words(&hex::decode(ent).unwrap()).unwrap(), words);
        assert_eq!(hex::encode(bip39_entropy(words).unwrap()), ent);
        assert_eq!(hex::encode(bip39_seed(words, "TREZOR").unwrap()), seed, "{words}");
    }
}

#[test]
fn v3_matches_the_independent_javascript_implementation() {
    let v = json("v3_vectors_js_reference.json");
    let v = v.as_array().unwrap();
    assert_eq!(v.len(), 7);
    for x in v {
        let (words, pw) = (x["words"].as_str().unwrap(), x["password"].as_str().unwrap());
        assert_eq!(hex::encode(bip39_seed(words, pw).unwrap()), x["seed64"].as_str().unwrap(), "seed {words:?} {pw:?}");
        let s = derive_v3(words, pw).unwrap();
        assert_eq!(hex::encode(s), x["secret"].as_str().unwrap(), "secret {words:?} {pw:?}");
        assert_eq!(address(&s), x["address"].as_str().unwrap());
    }
}

#[test]
fn legacy_wallets_open_exactly_as_before() {
    // sigil-top wallet_phrase.rs::same_phrase_same_wallet_as_sigilgraph_org (+ the six-word front page phrase)
    for (phrase, want, scheme) in [
        ("abandon ability able about above absent absorb abstract absurd abuse access accident",
         "f31437fff073dd8c2355d45050b038723e0babf273be0b5691bc6512f1139be9", Scheme::V2),
        ("xenon yield abandon ability able about", "92f0aa2e7844ea7c68c408f561b9dbe89a5cb09db03f73460086157e3f3b5b25", Scheme::V1),
        ("ash birch cedar delta ember fern", "dfc5ba9349367a88c6d243f033ef42f05cc6d6ebd8bf7d1fc416c73ec69d865f", Scheme::V1),
        ("0707070707070707070707070707070707070707070707070707070707070707",
         "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c", Scheme::RawSeed),
    ] {
        let c = legacy(phrase).unwrap();
        assert_eq!((c.scheme, address(&c.secret).as_str()), (scheme, want), "{phrase}");
        // ...and candidates() offers that same wallet (alone: none of these is checksum-valid BIP39)
        let cs = candidates(phrase, "").unwrap();
        assert_eq!(cs.len(), 1, "{phrase}");
        assert_eq!(cs[0], c);
    }
}

#[test]
fn a_password_always_means_v3() {
    let cs = candidates(ABOUT, "TREZOR").unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].scheme, Scheme::V3);
    assert_eq!(cs[0].secret, derive_v3(ABOUT, "TREZOR").unwrap());
    // a password with words that are not BIP39 is refused, never silently hashed as v2
    let v2_words = "abandon ability able about above absent absorb abstract absurd abuse access accident";
    assert_eq!(candidates(v2_words, "x"), Err(PhraseError::PasswordWithoutBip39Words(Box::new(PhraseError::Checksum))));
    assert!(matches!(candidates(&"07".repeat(32), "x"), Err(PhraseError::PasswordWithoutBip39Words(_))));
}

#[test]
fn bip39_words_without_password_offer_v3_then_v2() {
    let cs = candidates(ABOUT, "").unwrap();
    assert_eq!(cs.iter().map(|c| c.scheme).collect::<Vec<_>>(), [Scheme::V3, Scheme::V2]);
    assert_eq!(cs[0].secret, derive_v3(ABOUT, "").unwrap());
    assert_eq!(cs[1].secret, derive_v2(ABOUT).unwrap());
    // the escape hatches force one scheme
    assert_eq!(candidates(&format!("v2: {ABOUT}"), "").unwrap().iter().map(|c| c.scheme).collect::<Vec<_>>(), [Scheme::V2]);
    assert_eq!(candidates(&format!("V3:{ABOUT}"), "").unwrap().iter().map(|c| c.scheme).collect::<Vec<_>>(), [Scheme::V3]);
}

#[test]
fn a_v2_phrase_with_a_sigil_only_word_is_only_v2() {
    // "aether" is on the v2 list but not on BIP39's → cannot be a v3 phrase
    let p = "aether abandon ability able about above absent absorb abstract absurd abuse access";
    let cs = candidates(p, "").unwrap();
    assert_eq!(cs.iter().map(|c| c.scheme).collect::<Vec<_>>(), [Scheme::V2]);
}

#[test]
fn typos_are_named_not_hashed() {
    assert_eq!(bip39_entropy(&ABOUT.replace("about", "abandon")), Err(PhraseError::Checksum));
    assert_eq!(
        bip39_entropy(&ABOUT.replacen("abandon", "abandn", 1)),
        Err(PhraseError::UnknownWord { position: 1, word: "abandn".into() })
    );
    assert_eq!(bip39_entropy("abandon about"), Err(PhraseError::WordCount(2)));
    assert_eq!(bip39_entropy("   "), Err(PhraseError::Empty));
    // capitals and extra spaces are not typos
    assert_eq!(derive_v3(&format!("  {}  ", ABOUT.to_uppercase().replace(' ', "\t ")), "").unwrap(), derive_v3(ABOUT, "").unwrap());
}

#[test]
fn danish_letters_open_the_same_wallet_however_they_are_typed() {
    let composed = "Rødgrød med fløde — æøå ÆØÅ";
    let decomposed = "Rødgrød med fløde — æøa\u{30a} ÆØA\u{30a}";
    assert_ne!(composed, decomposed);
    assert_eq!(derive_v3(ABOUT, composed).unwrap(), derive_v3(ABOUT, decomposed).unwrap());
}

#[test]
fn password_whitespace_is_kept_and_flagged() {
    assert_ne!(derive_v3(ABOUT, "TREZOR").unwrap(), derive_v3(ABOUT, " TREZOR").unwrap());
    assert!(password_has_edge_whitespace(" TREZOR"));
    assert!(password_has_edge_whitespace("TREZOR\n"));
    assert!(!password_has_edge_whitespace("two words"));
    assert!(!password_has_edge_whitespace(""));
}

#[test]
fn generated_words_round_trip() {
    let w = generate_v3_words(&[0x5a; 16]);
    assert_eq!(w.split(' ').count(), 12);
    assert_eq!(bip39_entropy(&w).unwrap(), vec![0x5a; 16]);
    assert_ne!(generate_v3_words(&[0x5b; 16]), w);
}
