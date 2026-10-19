//! sigil-g3 dress-rehearsal driver (step R, 2026-10-19): drives a LIVE sigil-r3 chain (a
//! g3-rules build under a throwaway network id) over HTTP with real v7 notes and real
//! STARKs, and checks every g3 rule from the outside.
//!
//! ```text
//! g3_r3_driver <producer-url> <follower-url> <keys.json>
//!   e.g. g3_r3_driver http://172.31.78.11:18181 http://172.31.78.12:18181 …/r3/keys/keys.json
//! ```
//!
//!   G0  pool shape: v1 = epoch 0 sealed empty, v7 = epoch 1; coinbase pays the producer publicly
//!   G1  a 64-bit (old-format) deposit                                → REFUSED
//!   G2  a v7 deposit with no envelope                                → REFUSED (chain rule)
//!   G3  the same deposit with an envelope whose view tag is removed  → REFUSED
//!   G4  the same deposit, tagged; then seven more                    → settle
//!   G5  restore Alice from her seed by view tags alone               → finds all 8, downloads only matches
//!   G6  1-in payment to Bob; first with one untagged output          → refused, then settles
//!   G7  2-in merge payment to Bob                                    → settles, both nullifiers spent
//!   G8  4-in wide merge payment to Bob                               → settles, all four spent
//!   G9  withdrawal with change; first without envelopes              → refused, then settles
//!   G10 fresh restores of Alice and Bob from their seeds             → exact balances + memos
//!   G11 every note on the chain carries a view tag (tags route census)
//!   G12 producer ≡ follower (pool anchor, notes, nullifiers, value locked at one height)
//!
//! The spends are built from a wallet RESTORED from the chain each time (tags → matched
//! envelopes → positions → spent set), the path a phone restored from 12 words takes.
//! Blocking `ureq`, no async. Exit 0 only if every step did what it must.

use std::collections::BTreeSet;
use std::io::Read;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sigil_shield::note_cipher::{is_tagged_envelope, may_be_ours, NoteCiphertext, TagView};
use sigil_shield::note_v1::to_wire;
use sigil_shield::wallet::{shield_note, NoteStore, ShieldedAccount};
use sigil_shield::wallet_v7::{
    build_spend_2_v7, build_spend_k_v7, build_spend_v7, scoped_nullifier_v7, seal_output_v7, shield_note_v7, AccountV7, NoteStoreV7,
    SpendBundleV7,
};
use sigil_state::shielded::{POOL_CAPACITY, SHIELDED_FEE};

const FEE: u64 = SHIELDED_FEE as u64;
/// One deposit: 0.1 SIGIL (a ramp denomination).
const D: u64 = 1_000_000_000;

struct Node {
    url: String,
    /// The live v7 epoch (1 on the g3 family: v1 keeps 0, closed and empty) — read from the anchor.
    e7: u64,
}

impl Node {
    fn get_raw(&self, path: &str) -> Vec<u8> {
        let url = format!("{}{}", self.url, path);
        let r = ureq::get(&url).timeout(Duration::from_secs(30)).call().unwrap_or_else(|e| panic!("GET {url}: {e}"));
        let mut b = Vec::new();
        r.into_reader().read_to_end(&mut b).expect("read body");
        b
    }
    fn get(&self, path: &str) -> Value {
        let url = format!("{}{}", self.url, path);
        let t0 = Instant::now();
        loop {
            match ureq::get(&url).timeout(Duration::from_secs(30)).call() {
                Ok(r) => return r.into_json().unwrap_or(Value::Null),
                Err(ureq::Error::Status(_, r)) => return r.into_json().unwrap_or(Value::Null),
                Err(e) if t0.elapsed() < Duration::from_secs(90) => {
                    eprintln!("  (waiting for {url}: {e})");
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(e) => panic!("GET {url}: {e}"),
            }
        }
    }
    fn post(&self, path: &str, body: Value) -> Value {
        let url = format!("{}{}", self.url, path);
        match ureq::post(&url).timeout(Duration::from_secs(60)).send_json(body) {
            Ok(r) => r.into_json().unwrap_or(Value::Null),
            Err(ureq::Error::Status(_, r)) => r.into_json().unwrap_or(Value::Null),
            Err(e) => panic!("POST {url}: {e}"),
        }
    }
    fn height(&self) -> u64 {
        self.get("/v1/integrity").get("height").and_then(Value::as_u64).unwrap_or(0)
    }
    fn balance(&self, addr: &str) -> u128 {
        let v = self.get(&format!("/v1/balance?wallet={addr}"));
        v["data"]["balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    fn anchor(&self) -> Value {
        self.get("/v1/shielded/anchor")
    }
    /// Real v7 leaves of the live generation (epoch 0 on g3), in position order.
    fn leaves(&self) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        loop {
            let v = self.get(&format!("/v1/shielded/leaves?epoch={}&ciphertexts=0&from={}", self.e7, out.len()));
            assert_eq!(v["generation"], "v7", "leaves route must serve the v7 generation on g3: {v}");
            let total = v["total"].as_u64().unwrap_or(0) as usize;
            for l in v["leaves"].as_array().cloned().unwrap_or_default() {
                out.push(<[u8; 32]>::try_from(hex::decode(l.as_str().unwrap()).unwrap()).unwrap());
            }
            if out.len() >= total {
                break;
            }
        }
        out
    }
    /// The envelope at one position only — what a tag-matching wallet downloads.
    fn ciphertext_at(&self, pos: u64) -> Option<String> {
        let v = self.get(&format!("/v1/shielded/leaves?epoch={}&ciphertexts=1&from={pos}&count=1", self.e7));
        v["ciphertexts"].as_array().and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string)
    }
    /// `/v1/shielded/tags`: SGT1 | epoch u32 | from u64 | total u64 | n u32 (28 B) | n × [flag, vt, eph_pk 32].
    fn tags(&self) -> (u64, Vec<[u8; 34]>, usize) {
        let b = self.get_raw("/v1/shielded/tags");
        assert_eq!(&b[..4], b"SGT1", "tags magic");
        let total = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let n = u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize;
        let recs = b[28..].chunks_exact(34).map(|c| <[u8; 34]>::try_from(c).unwrap()).collect::<Vec<_>>();
        assert_eq!(recs.len(), n, "tags record count");
        (total, recs, b.len())
    }
    fn status(&self, hash: &str) -> (String, String) {
        let v = self.get(&format!("/v1/transactions/{hash}"));
        let pick = |v: &Value| v.get("status").and_then(Value::as_str).map(str::to_string);
        let s = pick(&v).or_else(|| v.get("data").and_then(pick)).unwrap_or_default();
        let r = v.get("reason").or_else(|| v.get("data").and_then(|d| d.get("reason"))).and_then(Value::as_str).unwrap_or("").to_string();
        (s, r)
    }
    fn wait_applied(&self, hash: &str, what: &str, secs: u64) -> bool {
        let t0 = Instant::now();
        loop {
            let (s, r) = self.status(hash);
            if s == "applied" {
                println!("  ✓ {what} applied ({:.1} s, h={})", t0.elapsed().as_secs_f64(), self.height());
                return true;
            }
            if s == "rejected" {
                println!("  ✗ {what} REJECTED: {r}");
                return false;
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                println!("  ✗ {what}: still '{s}' after {secs} s");
                return false;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    fn nullifiers(&self) -> BTreeSet<[u8; 32]> {
        self.get("/v1/shielded/nullifiers")["nullifiers"].as_array().cloned().unwrap_or_default().iter()
            .map(|n| <[u8; 32]>::try_from(hex::decode(n.as_str().unwrap()).unwrap()).unwrap()).collect()
    }
}

static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Strictly increasing request nonce (ms clock, bumped when two calls share a millisecond).
fn nonce() -> u64 {
    use std::sync::atomic::Ordering::SeqCst;
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let next = t.max(NONCE.load(SeqCst) + 1);
    NONCE.store(next, SeqCst);
    next
}

fn check(cond: bool, what: &str) -> bool {
    println!("  {} {what}", if cond { "✓" } else { "✗" });
    cond
}

fn post_shield(n: &Node, sk: &SigningKey, from: &str, amount: u128, cm: [u8; 32], blinding: [u8; 32], pk: [u8; 32], ct: Option<String>) -> Value {
    let nonce = nonce();
    let cm_hex = hex::encode(cm);
    let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm_hex}|0|nonce={nonce}");
    n.post("/v1/shield", json!({
        "from": from, "amount": amount.to_string(), "cm": cm_hex, "fee": "0",
        "sig": hex::encode(sk.sign(msg.as_bytes()).to_bytes()), "req_nonce": nonce,
        "note_ciphertext": ct, "blinding": hex::encode(blinding), "pk_shield": hex::encode(pk),
    }))
}

fn txid(v: &Value) -> Option<String> {
    v.get("txid").and_then(Value::as_str).map(str::to_string)
}
fn err(v: &Value) -> String {
    v.get("error").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Remove the view tag from a sealed envelope (`…,"vt":"ab"}` → `…}`): an honest envelope a
/// pre-g3 wallet would have written.
fn untag(ct: &str) -> String {
    let i = ct.rfind(",\"vt\":\"").expect("tagged envelope");
    format!("{}}}", &ct[..i])
}

/// A wallet restored from its seed alone, the g3 way: download 34 B per note, fetch the
/// envelope only where the view tag matches, open it, find its leaf, net the spent set.
struct Restored {
    store: NoteStoreV7,
    candidates: usize,
    total: u64,
}
fn restore(n: &Node, acct: &AccountV7) -> Restored {
    let id = acct.enc_identity();
    let (total, recs, _) = n.tags();
    let mut cts = Vec::new();
    let mut candidates = 0;
    for (pos, r) in recs.iter().enumerate() {
        let view = TagView { eph_pk: r[2..34].try_into().unwrap(), vt: (r[0] == 2).then_some(r[1]) };
        if r[0] == 0 || !may_be_ours(&view, &id) {
            continue;
        }
        candidates += 1;
        if let Some(ct) = n.ciphertext_at(pos as u64) {
            cts.push(NoteCiphertext(ct));
        }
    }
    let mut store = NoteStoreV7::new();
    store.scan_ciphertexts(&id, &cts);
    store.scan_owned(acct, &n.leaves());
    store.mark_spent(acct, &n.nullifiers(), n.e7 as u32);
    Restored { store, candidates, total }
}
fn unspent_slots(s: &NoteStoreV7, k: usize) -> Vec<usize> {
    let mut v: Vec<usize> = s.notes.iter().enumerate().filter(|(_, n)| n.position.is_some() && !n.spent && n.value > 0).map(|(i, _)| i).collect();
    v.truncate(k);
    v
}

/// Seal each output to its owner (`recipients[i]` = pk_enc hex) with a memo.
fn seal_outs(b: &SpendBundleV7, recipients: [&str; 2], memo: &str) -> Vec<Option<String>> {
    b.out_preimages.iter().zip(recipients).map(|((v, bl), pk)| Some(seal_output_v7(*v, *bl, pk, memo).unwrap().0)).collect()
}

fn send(n: &Node, b: &SpendBundleV7, cts: &[Option<String>]) -> Value {
    n.post("/v1/shielded_send", json!({
        "anchor": hex::encode(b.anchor), "nullifier": hex::encode(b.nullifier),
        "extra_nullifiers": b.extra_nullifiers.iter().map(hex::encode).collect::<Vec<_>>(),
        "cm_outs": b.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(), "fee": SHIELDED_FEE.to_string(),
        "proof": hex::encode(&b.proof), "note_ciphertexts": cts,
    }))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: g3_r3_driver <producer-url> <follower-url> <keys.json>");
        std::process::exit(2);
    }
    let mut n = Node { url: args[1].trim_end_matches('/').to_string(), e7: 0 };
    let mut f = Node { url: args[2].trim_end_matches('/').to_string(), e7: 0 };
    let keys: Value = serde_json::from_str(&std::fs::read_to_string(&args[3]).expect("keys.json")).unwrap();
    let seed = <[u8; 32]>::try_from(hex::decode(keys["producer_seed"].as_str().expect("producer_seed")).unwrap()).unwrap();
    let sk = SigningKey::from_bytes(&seed);
    let from = hex::encode(sk.verifying_key().to_bytes());
    let alice = AccountV7::from_seed(seed);
    let bob = AccountV7::from_seed([0xB0; 32]);
    let (alice_enc, bob_enc) = (alice.pk_enc_hex(), bob.pk_enc_hex());
    let mut ok = true;
    println!("g3 r3 driver → {} (follower {}) · wallet {}… · h={}", n.url, f.url, &from[..12], n.height());

    // ── G0 pool shape + coinbase ───────────────────────────────────────────────────
    let a = n.anchor();
    println!("G0 anchor: v1_live={} v1 notes={} v7={}", a["v1_live"], a["notes"], a["v7"]);
    ok &= check(a["v1_live"] == false, "v1 generation closed from genesis");
    ok &= check(a["notes"].as_u64() == Some(0), "v1 pool empty");
    let sealed = a["sealed_epochs"].as_array().cloned().unwrap_or_default();
    ok &= check(sealed.len() == 1 && sealed[0]["epoch"] == 0 && sealed[0]["notes"] == 0, "v1 epoch 0 sealed empty at genesis");
    ok &= check(a["v7"]["epoch"].as_u64() == Some(1), "v7 generation open as epoch 1 (no number shared with v1)");
    n.e7 = a["v7"]["epoch"].as_u64().unwrap_or(1);
    f.e7 = n.e7;
    let empty_v1 = n.get("/v1/shielded/leaves?epoch=0&ciphertexts=0");
    ok &= check(empty_v1["total"] == 0 && empty_v1["generation"] == "v1", "asking for epoch 0 gets the empty v1 epoch, never v7 leaves");
    let need = 8 * D as u128;
    let t0 = Instant::now();
    while n.balance(&from) < need {
        if t0.elapsed() > Duration::from_secs(300) {
            println!("✗ coinbase never funded the producer (balance {})", n.balance(&from));
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("G0 coinbase paid the producer publicly: balance {} glyphs at h={}", n.balance(&from), n.height());

    // ── G1 old-format deposit ──────────────────────────────────────────────────────
    let v1 = ShieldedAccount::from_seed(seed);
    let mut v1store = NoteStore::new();
    let (i1, cm1) = shield_note(&v1, &mut v1store, D).unwrap();
    let slot = v1store.notes.iter().position(|x| x.index == Some(i1)).unwrap();
    let r = post_shield(&n, &sk, &from, D as u128, cm1, to_wire(v1store.notes[slot].blinding), to_wire(v1.public_key()), None);
    println!("G1 64-bit deposit → {}", if txid(&r).is_some() { "ACCEPTED".to_string() } else { err(&r) });
    ok &= check(txid(&r).is_none(), "old-format deposit refused");

    // ── G2–G4 one v7 deposit: no envelope, untagged envelope, tagged ───────────────
    let mut mine = NoteStoreV7::new();
    let dep = shield_note_v7(&alice, &mut mine, D).unwrap();
    let tagged = seal_output_v7(D, alice.blinding(dep.index), &alice_enc, "g3 deposit 1").unwrap().0;
    ok &= check(is_tagged_envelope(&tagged), "seal_note_v7 writes a view tag");
    let r = post_shield(&n, &sk, &from, D as u128, dep.cm, dep.blinding, dep.pk_shield, None);
    println!("G2 v7 deposit, no envelope → {}", if txid(&r).is_some() { "ACCEPTED".to_string() } else { err(&r) });
    ok &= check(txid(&r).is_none(), "v7 deposit without an envelope refused");
    let r = post_shield(&n, &sk, &from, D as u128, dep.cm, dep.blinding, dep.pk_shield, Some(untag(&tagged)));
    println!("G3 v7 deposit, untagged envelope → {}", if txid(&r).is_some() { "ACCEPTED".to_string() } else { err(&r) });
    ok &= check(txid(&r).is_none(), "v7 deposit with an untagged envelope refused");
    let r = post_shield(&n, &sk, &from, D as u128, dep.cm, dep.blinding, dep.pk_shield, Some(tagged));
    let Some(h) = txid(&r) else { println!("✗ G4 tagged deposit refused: {r}"); std::process::exit(1) };
    ok &= check(n.wait_applied(&h, "G4 tagged v7 deposit 1", 60), "tagged v7 deposit settles");
    let mut hs = Vec::new();
    for k in 2..=8 {
        let dep = shield_note_v7(&alice, &mut mine, D).unwrap();
        let ct = seal_output_v7(D, alice.blinding(dep.index), &alice_enc, &format!("g3 deposit {k}")).unwrap().0;
        let r = post_shield(&n, &sk, &from, D as u128, dep.cm, dep.blinding, dep.pk_shield, Some(ct));
        match txid(&r) {
            Some(h) => hs.push((k, h)),
            None => { println!("✗ G4 deposit {k} refused: {r}"); ok = false; }
        }
    }
    for (k, h) in &hs {
        ok &= n.wait_applied(h, &format!("G4 tagged v7 deposit {k}"), 60);
    }

    // ── G5 restore Alice by view tags ──────────────────────────────────────────────
    let t = Instant::now();
    let ra = restore(&n, &alice);
    let (_, _, tag_bytes) = n.tags();
    println!("G5 restore Alice: {} notes on chain, tags {} B, {} envelopes downloaded, {} notes found, balance {} ({:.0} ms)",
        ra.total, tag_bytes, ra.candidates, ra.store.notes.len(), ra.store.balance(), t.elapsed().as_secs_f64() * 1e3);
    ok &= check(ra.store.balance() == 8 * D as u128, "restored balance = 8 deposits");
    let rb = restore(&n, &bob);
    println!("    Bob before any payment: {} envelopes downloaded of {} (a stranger's tag matches 1 time in 256)", rb.candidates, rb.total);

    let mut expect_alice: u128 = 8 * D as u128;
    let mut expect_bob: u128 = 0;
    let cap = POOL_CAPACITY;

    // ── G6 1-in payment ────────────────────────────────────────────────────────────
    let mut ra = restore(&n, &alice);
    let s = unspent_slots(&ra.store, 1);
    let pay = 400_000_000u64;
    let t = Instant::now();
    let b = build_spend_v7(&alice, &mut ra.store, &n.leaves(), cap, s[0], FEE, &[(pay, bob.owner_pk()), (D - pay - FEE, alice.owner_pk())]).expect("G6 build");
    println!("G6 1-in proof: {} B in {:.0} ms", b.proof.len(), t.elapsed().as_secs_f64() * 1e3);
    let mut cts = seal_outs(&b, [&bob_enc, &alice_enc], "g3 pay 1-in");
    let good0 = cts[0].clone();
    cts[0] = Some(untag(cts[0].as_deref().unwrap()));
    let r = send(&n, &b, &cts);
    println!("    with one untagged output → {}", if txid(&r).is_some() { "ACCEPTED".to_string() } else { err(&r) });
    ok &= check(txid(&r).is_none(), "payment with an untagged output envelope refused");
    cts[0] = good0;
    let r = send(&n, &b, &cts);
    match txid(&r) {
        Some(h) => {
            ok &= check(n.wait_applied(&h, "G6 1-in payment", 90), "1-in payment settles");
            ok &= check(n.nullifiers().contains(&scoped_nullifier_v7(n.e7 as u32, b.nullifier)), "its nullifier is spent (scoped to the v7 epoch)");
            expect_alice -= (pay + FEE) as u128;
            expect_bob += pay as u128;
        }
        None => { println!("✗ G6 refused: {r}"); ok = false; }
    }

    // ── G7 2-in merge payment ──────────────────────────────────────────────────────
    let mut ra = restore(&n, &alice);
    let s = unspent_slots(&ra.store, 2);
    let pay = 1_500_000_000u64;
    let have: u64 = s.iter().map(|&i| ra.store.notes[i].value).sum();
    let t = Instant::now();
    let b = build_spend_2_v7(&alice, &mut ra.store, &n.leaves(), cap, [s[0], s[1]], FEE, &[(pay, bob.owner_pk()), (have - pay - FEE, alice.owner_pk())]).expect("G7 build");
    println!("G7 2-in proof: {} B in {:.0} ms", b.proof.len(), t.elapsed().as_secs_f64() * 1e3);
    let r = send(&n, &b, &seal_outs(&b, [&bob_enc, &alice_enc], "g3 pay 2-in"));
    match txid(&r) {
        Some(h) => {
            ok &= check(n.wait_applied(&h, "G7 2-in merge payment", 90), "2-in merge settles");
            let nfs = n.nullifiers();
            let sc = |x: &[u8; 32]| scoped_nullifier_v7(n.e7 as u32, *x);
            ok &= check(nfs.contains(&sc(&b.nullifier)) && b.extra_nullifiers.iter().all(|x| nfs.contains(&sc(x))), "both nullifiers spent");
            expect_alice -= (pay + FEE) as u128;
            expect_bob += pay as u128;
        }
        None => { println!("✗ G7 refused: {r}"); ok = false; }
    }

    // ── G8 4-in wide merge payment ─────────────────────────────────────────────────
    let mut ra = restore(&n, &alice);
    let s = unspent_slots(&ra.store, 4);
    let pay = 3_000_000_000u64;
    let have: u64 = s.iter().map(|&i| ra.store.notes[i].value).sum();
    if s.len() == 4 && have > pay + FEE {
        let t = Instant::now();
        let b = build_spend_k_v7(&alice, &mut ra.store, &n.leaves(), cap, &s, FEE, &[(pay, bob.owner_pk()), (have - pay - FEE, alice.owner_pk())]).expect("G8 build");
        println!("G8 4-in proof: {} B in {:.0} ms", b.proof.len(), t.elapsed().as_secs_f64() * 1e3);
        let r = send(&n, &b, &seal_outs(&b, [&bob_enc, &alice_enc], "g3 pay 4-in"));
        match txid(&r) {
            Some(h) => {
                ok &= check(n.wait_applied(&h, "G8 4-in wide merge payment", 90), "4-in wide merge settles");
                let nfs = n.nullifiers();
                let sc = |x: &[u8; 32]| scoped_nullifier_v7(n.e7 as u32, *x);
                ok &= check(nfs.contains(&sc(&b.nullifier)) && b.extra_nullifiers.len() == 3 && b.extra_nullifiers.iter().all(|x| nfs.contains(&sc(x))), "all four nullifiers spent");
                expect_alice -= (pay + FEE) as u128;
                expect_bob += pay as u128;
            }
            None => { println!("✗ G8 refused: {r}"); ok = false; }
        }
    } else {
        println!("✗ G8: only {} unspent notes ({} glyphs) for a 4-in merge", s.len(), have);
        ok = false;
    }

    // ── G9 withdrawal with change ──────────────────────────────────────────────────
    let mut ra = restore(&n, &alice);
    let s = unspent_slots(&ra.store, 1);
    let v = ra.store.notes[s[0]].value;
    let out = 500_000_000u64;
    let b = build_spend_v7(&alice, &mut ra.store, &n.leaves(), cap, s[0], out, &[(v - out - 200_000_000, alice.owner_pk()), (200_000_000, alice.owner_pk())]).expect("G9 build");
    let body = |cts: Vec<Option<String>>| json!({
        "to": from, "amount": out.to_string(), "anchor": hex::encode(b.anchor), "nullifier": hex::encode(b.nullifier),
        "cm_outs": b.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(), "proof": hex::encode(&b.proof), "fee": "0",
        "note_ciphertexts": cts,
    });
    let r = n.post("/v1/unshield", body(vec![]));
    println!("G9 withdrawal without change envelopes → {}", if txid(&r).is_some() { "ACCEPTED".to_string() } else { err(&r) });
    ok &= check(txid(&r).is_none(), "withdrawal without envelopes refused");
    let bal0 = n.balance(&from);
    let r = n.post("/v1/unshield", body(seal_outs(&b, [&alice_enc, &alice_enc], "g3 change")));
    match txid(&r) {
        Some(h) => {
            ok &= check(n.wait_applied(&h, "G9 withdrawal 0.05 SIGIL with change", 90), "withdrawal settles");
            ok &= check(n.balance(&from) >= bal0 + out as u128, "public balance rose by at least the withdrawal");
            expect_alice -= out as u128;
        }
        None => { println!("✗ G9 refused: {r}"); ok = false; }
    }

    // ── G10 fresh restores ─────────────────────────────────────────────────────────
    let ra = restore(&n, &alice);
    let rb = restore(&n, &bob);
    let memos: Vec<String> = rb.store.notes.iter().filter_map(|x| x.memo.clone()).collect();
    println!("G10 Alice restored: {} (expected {}) from {} envelopes of {}", ra.store.balance(), expect_alice, ra.candidates, ra.total);
    println!("    Bob restored:   {} (expected {}) from {} envelopes, memos {:?}", rb.store.balance(), expect_bob, rb.candidates, memos);
    ok &= check(ra.store.balance() == expect_alice, "Alice's balance restored exactly, change of the withdrawal included");
    ok &= check(rb.store.balance() == expect_bob, "Bob's balance restored exactly");
    ok &= check(memos.len() == 3, "Bob reads all three memos");

    // ── G11 tag census ─────────────────────────────────────────────────────────────
    let (total, recs, bytes) = n.tags();
    let tagged = recs.iter().filter(|r| r[0] == 2).count();
    println!("G11 tags route: {} notes, {} tagged, {} B ({} B/note)", total, tagged, bytes, if total > 0 { (bytes - 28) / total as usize } else { 0 });
    ok &= check(total as usize == recs.len() && tagged == recs.len() && total == 16, "every one of the 16 notes carries a view tag");

    // ── G12 producer ≡ follower ────────────────────────────────────────────────────
    let t0 = Instant::now();
    let same = loop {
        let (ap, af) = (n.anchor(), f.anchor());
        let key = |a: &Value| (a["v7"].clone(), a["value_locked"].clone(), a["nullifiers"].clone(), a["notes"].clone());
        if key(&ap) == key(&af) {
            println!("G12 follower ≡ producer: v7 {} · nullifiers {} · value_locked {}", ap["v7"], ap["nullifiers"], ap["value_locked"]);
            break true;
        }
        if t0.elapsed() > Duration::from_secs(60) {
            println!("G12 producer {ap}\n    follower {af}");
            break false;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    ok &= check(same, "follower holds the same pool as the producer");

    println!("END h={} · {}", n.height(), if ok { "ALL STEPS OK" } else { "SOME STEP FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
