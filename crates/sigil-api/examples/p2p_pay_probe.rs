//! p2p_pay_probe <submit-url> <check-url> <keys.json>
//! One tiny private payment from the test wallet BACK TO ITSELF, POSTed only to <submit-url> (a follower).
//! <check-url> (the block-maker) is only READ. If the block-maker spends the nullifier, the payment
//! reached it over P2P (follower -> Dandelion++ relay -> TOPIC_TXS), never over HTTP. 2026-10-24.

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
        eprintln!("usage: p2p_pay_probe <submit-url (follower)> <check-url (block-maker, read only)> <keys.json>");
        std::process::exit(2);
    }
    let mut f = Node { url: args[1].trim_end_matches('/').to_string(), e7: 0 };
    let mut p = Node { url: args[2].trim_end_matches('/').to_string(), e7: 0 };
    let keys: Value = serde_json::from_str(&std::fs::read_to_string(&args[3]).expect("keys.json")).unwrap();
    let seed = <[u8; 32]>::try_from(hex::decode(keys["producer_seed"].as_str().expect("producer_seed")).unwrap()).unwrap();
    let alice = AccountV7::from_seed(seed);
    let alice_enc = alice.pk_enc_hex();
    let a = f.anchor();
    f.e7 = a["v7"]["epoch"].as_u64().unwrap_or(1);
    p.e7 = f.e7;
    println!("probe: submit → {} · check (read only) → {} · follower h={} block-maker h={}", f.url, p.url, f.height(), p.height());
    let mut ra = restore(&f, &alice);
    let s = unspent_slots(&ra.store, 1);
    println!("restored from the follower: {} notes, {} glyphs private", ra.store.notes.len(), ra.total);
    let Some(&slot) = s.first() else { println!("✗ no unspent private note to spend"); std::process::exit(3) };
    let v = ra.store.notes[slot].value;
    if v <= FEE + 2 { println!("✗ note {} glyphs cannot pay the fee {}", v, FEE); std::process::exit(3); }
    let o1 = (v - FEE) / 2;
    let o2 = v - FEE - o1;
    let t = Instant::now();
    let b = build_spend_v7(&alice, &mut ra.store, &f.leaves(), POOL_CAPACITY, slot, FEE, &[(o1, alice.owner_pk()), (o2, alice.owner_pk())]).expect("build");
    println!("1-in self-payment: note {} glyphs → {} + {} (fee {}), proof {} B in {:.0} ms", v, o1, o2, FEE, b.proof.len(), t.elapsed().as_secs_f64() * 1e3);
    let nf = scoped_nullifier_v7(f.e7 as u32, b.nullifier);
    let cts = seal_outs(&b, [&alice_enc, &alice_enc], "p2p probe 2026-10-24");
    let t0 = Instant::now();
    let r = send(&f, &b, &cts);
    let Some(h) = txid(&r) else { println!("✗ the follower refused it: {}", err(&r)); std::process::exit(1) };
    println!("✓ accepted by the follower in {:.0} ms · txid {}", t0.elapsed().as_secs_f64() * 1e3, h);
    let (mut on_p, mut on_f) = (None, None);
    while t0.elapsed() < Duration::from_secs(180) && (on_p.is_none() || on_f.is_none()) {
        if on_p.is_none() && p.nullifiers().contains(&nf) { on_p = Some(t0.elapsed()); println!("✓ block-maker has spent the nullifier after {:.1} s (it never received an HTTP request)", t0.elapsed().as_secs_f64()); }
        if on_f.is_none() && f.nullifiers().contains(&nf) { on_f = Some(t0.elapsed()); println!("✓ follower has it settled after {:.1} s", t0.elapsed().as_secs_f64()); }
        std::thread::sleep(Duration::from_millis(250));
    }
    if on_p.is_none() { println!("✗ the block-maker never spent it within 180 s — the P2P relay did not deliver it"); std::process::exit(1); }
    println!("RESULT: payment POSTed to the follower only, landed on the block-maker over P2P");
}
