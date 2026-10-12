//! v7 rehearsal driver (M5 soak): drives a LIVE sigil-r1 node over HTTP through the v7
//! activation gate, with real notes and real STARKs, and reports what settled.
//!
//! ```text
//! v7_r1_driver <node-url> <keys.json> <gate-height>
//!   e.g. v7_r1_driver http://172.31.77.11:18181 /home/storage/sigil-rehearsal/keys/keys.json 1000
//! ```
//!
//! BEFORE the gate (must be reached before block `gate`):
//!   A1  a 64-bit deposit, then a private payment out of it (v1 proof)       → settles
//!   A2  a second 64-bit deposit, left unspent                                 → settles
//! AFTER the gate (the pool reports `v7`):
//!   B1  a 64-bit deposit                                                     → REFUSED at the door
//!   B2  the note from A2 spent with a v1 proof against the live v1 anchor    → settles (no confiscation)
//!   B3  a 256-bit deposit with its v7 opening                                → settles into the v7 tree
//!   B4  a v7 STARK spend against the pool's own v7 anchor                    → settles, nullifier scoped
//!
//! The wallet is the producer's own (its signing seed from keys.json), because the coinbase
//! pays it. Blocking `ureq`, no async. Exit code 0 only if every step did what it must.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sigil_shield::note_v1::{padding_leaf_wire, to_wire};
use sigil_shield::note_v7::{padding_leaf, Digest4, NoteV7, PoolTreeV7};
use sigil_shield::spend_full_v7::{build_spend_full_v7_trace_seeded_u64, v7_options, OutputV7, SpendFullV7Prover};
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_shield::zk_mask::HidingProver;
use sigil_state::shielded::{scoped_nullifier, POOL_CAPACITY, SHIELDED_FEE};

struct Node {
    url: String,
}

impl Node {
    fn get(&self, path: &str) -> Value {
        let url = format!("{}{}", self.url, path);
        let t0 = Instant::now();
        loop {
            match ureq::get(&url).timeout(Duration::from_secs(30)).call() {
                Ok(r) => return r.into_json().unwrap_or(Value::Null),
                Err(ureq::Error::Status(_, r)) => return r.into_json().unwrap_or(Value::Null),
                // a node still booting: wait for its API rather than giving up
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
        fn find(v: &Value) -> Option<u128> {
            match v {
                Value::Object(m) => {
                    if let Some(b) = m.get("balance") {
                        if let Some(s) = b.as_str() { return s.parse().ok(); }
                        if let Some(n) = b.as_u64() { return Some(n as u128); }
                    }
                    m.values().find_map(find)
                }
                _ => None,
            }
        }
        find(&self.get(&format!("/v1/balance?wallet={addr}"))).unwrap_or(0)
    }
    fn anchor(&self) -> Value {
        self.get("/v1/shielded/anchor")
    }
    fn leaves(&self, epoch: u64) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        loop {
            let v = self.get(&format!("/v1/shielded/leaves?epoch={epoch}&ciphertexts=0&from={}", out.len()));
            let total = v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            for l in v.get("leaves").and_then(Value::as_array).cloned().unwrap_or_default() {
                let b = hex::decode(l.as_str().unwrap()).unwrap();
                out.push(<[u8; 32]>::try_from(b).unwrap());
            }
            if out.len() >= total { break; }
        }
        out
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
            if s == "applied" { println!("  ✓ {what} applied ({:.1} s, h={})", t0.elapsed().as_secs_f64(), self.height()); return true; }
            if s == "rejected" { println!("  ✗ {what} REJECTED: {r}"); return false; }
            if t0.elapsed() > Duration::from_secs(secs) { println!("  ✗ {what}: still '{s}' after {secs} s"); return false; }
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    fn nullifiers(&self) -> Vec<[u8; 32]> {
        self.get("/v1/shielded/nullifiers")
            .get("nullifiers").and_then(Value::as_array).cloned().unwrap_or_default()
            .iter().map(|n| <[u8; 32]>::try_from(hex::decode(n.as_str().unwrap()).unwrap()).unwrap()).collect()
    }
}

fn nonce() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn sign_shield(sk: &SigningKey, from: &str, amount: u128, cm: &str, fee: u128, nonce: u64) -> String {
    let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={nonce}");
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

fn post_shield(n: &Node, sk: &SigningKey, from: &str, amount: u128, cm: [u8; 32], blinding: [u8; 32], pk: [u8; 32]) -> Value {
    let nonce = nonce();
    let cm_hex = hex::encode(cm);
    n.post("/v1/shield", json!({
        "from": from, "amount": amount.to_string(), "cm": cm_hex, "fee": "0",
        "sig": sign_shield(sk, from, amount, &cm_hex, 0, nonce), "req_nonce": nonce,
        "note_ciphertext": null, "blinding": hex::encode(blinding), "pk_shield": hex::encode(pk),
    }))
}

fn txid(v: &Value) -> Option<String> {
    v.get("txid").and_then(Value::as_str).map(str::to_string)
}

/// The store SLOT holding the note created at derivation `index` (slots shift as change
/// notes are inserted; `shield_note` returns the index, `build_spend` wants the slot).
fn slot_of(store: &NoteStore, index: u64) -> usize {
    store.notes.iter().position(|n| n.index == Some(index)).expect("note in store")
}

fn v1_pool(n: &Node, epoch: u64) -> Vec<[u8; 32]> {
    let mut leaves = n.leaves(epoch);
    for i in leaves.len()..POOL_CAPACITY {
        leaves.push(padding_leaf_wire(i as u64));
    }
    leaves
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 { eprintln!("usage: v7_r1_driver <node-url> <keys.json> <gate-height>"); std::process::exit(2); }
    let n = Node { url: args[1].trim_end_matches('/').to_string() };
    let keys: Value = serde_json::from_str(&std::fs::read_to_string(&args[2]).expect("keys.json")).unwrap();
    let gate: u64 = args[3].parse().unwrap();
    let seed_hex = keys["producer_seed"].as_str().expect("producer_seed");
    let seed = <[u8; 32]>::try_from(hex::decode(seed_hex).unwrap()).unwrap();
    let sk = SigningKey::from_bytes(&seed);
    let from = hex::encode(sk.verifying_key().to_bytes());
    let alice = ShieldedAccount::from_seed(seed);
    let bob = ShieldedAccount::from_seed([0xB0; 32]);
    let mut store = NoteStore::new();
    let mut ok = true;
    println!("v7 r1 driver → {} · wallet {}… · gate {gate} · h={}", n.url, &from[..12], n.height());

    // ── A: before the gate ───────────────────────────────────────────────────────
    let need = 1_000_000u128 + 2_000_000;
    let t0 = Instant::now();
    while n.balance(&from) < need {
        if t0.elapsed() > Duration::from_secs(240) { println!("✗ wallet never funded by coinbase (balance {})", n.balance(&from)); std::process::exit(1); }
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("A0 funded: balance {} at h={}", n.balance(&from), n.height());

    let (i1, cm1) = shield_note(&alice, &mut store, 1_000_000).unwrap();
    let b1 = to_wire(store.notes[slot_of(&store, i1)].blinding);
    let r = post_shield(&n, &sk, &from, 1_000_000, cm1, b1, to_wire(alice.public_key()));
    let Some(h) = txid(&r) else { println!("✗ A1 deposit refused: {r}"); std::process::exit(1); };
    ok &= n.wait_applied(&h, "A1 v1 deposit 1,000,000", 60);
    let live_epoch = n.anchor()["epoch"].as_u64().unwrap_or(0);
    store.scan_owned(&alice, &v1_pool(&n, live_epoch));
    let s1 = slot_of(&store, i1);
    let bundle = build_spend(&alice, &mut store, &v1_pool(&n, live_epoch), s1, SHIELDED_FEE as u64,
        &[(500_000, bob.public_key()), (400_000, alice.public_key())]).expect("A1 spend");
    let r = n.post("/v1/shielded_send", json!({
        "anchor": hex::encode(bundle.anchor), "nullifier": hex::encode(bundle.nullifier), "extra_nullifiers": [],
        "cm_outs": bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(), "fee": SHIELDED_FEE.to_string(),
        "proof": hex::encode(&bundle.proof), "note_ciphertexts": [],
    }));
    let Some(h) = txid(&r) else { println!("✗ A1 send refused: {r}"); std::process::exit(1); };
    ok &= n.wait_applied(&h, "A1 v1 private payment", 60);
    ok &= n.nullifiers().contains(&bundle.nullifier);

    let (i2, cm2) = shield_note(&alice, &mut store, 2_000_000).unwrap();
    let b2 = to_wire(store.notes[slot_of(&store, i2)].blinding);
    let r = post_shield(&n, &sk, &from, 2_000_000, cm2, b2, to_wire(alice.public_key()));
    let Some(h) = txid(&r) else { println!("✗ A2 deposit refused: {r}"); std::process::exit(1); };
    ok &= n.wait_applied(&h, "A2 v1 deposit 2,000,000 (kept)", 60);
    let h_a = n.height();
    println!("A done at h={h_a} ({})", if h_a < gate { "before the gate ✓" } else { "AFTER the gate ✗ — rerun with a later gate" });
    ok &= h_a < gate;

    // ── B: after the gate ────────────────────────────────────────────────────────
    while n.height() < gate + 2 { std::thread::sleep(Duration::from_millis(500)); }
    let a = n.anchor();
    let v7 = a.get("v7").cloned().unwrap_or(Value::Null);
    println!("B0 h={} v7={} v1_live={}", n.height(), v7, a["v1_live"]);
    if v7.is_null() { println!("✗ the pool did not open v7 at the gate"); std::process::exit(1); }
    let v7_epoch = v7["epoch"].as_u64().unwrap();

    let (i3, cm3) = shield_note(&alice, &mut store, 1_000_000).unwrap();
    let b3 = to_wire(store.notes[slot_of(&store, i3)].blinding);
    let r = post_shield(&n, &sk, &from, 1_000_000, cm3, b3, to_wire(alice.public_key()));
    let refused = txid(&r).is_none() && r["error"].as_str().unwrap_or("").contains("old format");
    println!("B1 late v1 deposit: {}", if refused { "REFUSED at the door ✓" } else { "NOT refused ✗" });
    ok &= refused;

    let live_epoch = a["epoch"].as_u64().unwrap_or(0);
    store.scan_owned(&alice, &v1_pool(&n, live_epoch));
    let s2 = slot_of(&store, i2);
    let bundle = build_spend(&alice, &mut store, &v1_pool(&n, live_epoch), s2, SHIELDED_FEE as u64,
        &[(700_000, bob.public_key()), (2_000_000 - 700_000 - SHIELDED_FEE as u64, alice.public_key())]).expect("B2 spend");
    let r = n.post("/v1/shielded_send", json!({
        "anchor": hex::encode(bundle.anchor), "nullifier": hex::encode(bundle.nullifier), "extra_nullifiers": [],
        "cm_outs": bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(), "fee": SHIELDED_FEE.to_string(),
        "proof": hex::encode(&bundle.proof), "note_ciphertexts": [],
    }));
    match txid(&r) {
        Some(h) => { ok &= n.wait_applied(&h, "B2 v1 note spent AFTER the gate", 60); ok &= n.nullifiers().contains(&bundle.nullifier); }
        None => { println!("✗ B2 v1 send refused: {r}"); ok = false; }
    }

    let note7 = NoteV7::new(10_000_000, [0x1111, 0x2222, 0x3333, 0x4444], [0xA1, 0xA2, 0xA3, 0xA4]).unwrap();
    let cm7 = note7.commitment().to_wire();
    let r = post_shield(&n, &sk, &from, 10_000_000, cm7, Digest4(note7.blinding).to_wire(), note7.owner_pk().to_wire());
    let Some(h) = txid(&r) else { println!("✗ B3 v7 deposit refused: {r}"); std::process::exit(1); };
    ok &= n.wait_applied(&h, "B3 v7 deposit 10,000,000", 60);
    let v7_leaves = n.leaves(v7_epoch);
    let pos = v7_leaves.iter().position(|c| *c == cm7).expect("our v7 commitment in the v7 epoch");
    let mut leaves: Vec<Digest4> = v7_leaves.iter().map(|c| Digest4::from_wire(c).unwrap()).collect();
    leaves.resize(POOL_CAPACITY, padding_leaf());
    let tree = PoolTreeV7::new(leaves).unwrap();
    let anchor7 = n.anchor()["v7"]["anchor"].as_str().unwrap().to_string();
    assert_eq!(hex::encode(tree.root().to_wire()), anchor7, "node and driver agree on the v7 anchor");
    let bob7 = NoteV7::new(0, [0; 4], [0xB0, 0xB1, 0xB2, 0xB3]).unwrap().owner_pk();
    let outs = [OutputV7::new(4_000_000, [0x51, 0x52, 0x53, 0x54], bob7), OutputV7::new(10_000_000 - 4_000_000 - SHIELDED_FEE as u64, [0x61, 0x62, 0x63, 0x64], note7.owner_pk())];
    let t = Instant::now();
    let trace = build_spend_full_v7_trace_seeded_u64(&note7, &tree.path(pos).unwrap(), SHIELDED_FEE as u64, &outs, &v7_options(), [0x77; 32]);
    let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(trace).expect("prove v7").to_bytes();
    println!("B4 v7 proof: {} bytes in {:.0} ms", proof.len(), t.elapsed().as_secs_f64() * 1e3);
    let nf7 = note7.nullifier(pos as u64).to_wire();
    let r = n.post("/v1/shielded_send", json!({
        "anchor": anchor7, "nullifier": hex::encode(nf7), "extra_nullifiers": [],
        "cm_outs": outs.iter().map(|o| hex::encode(o.commitment().to_wire())).collect::<Vec<_>>(), "fee": SHIELDED_FEE.to_string(),
        "proof": hex::encode(&proof), "note_ciphertexts": [],
    }));
    match txid(&r) {
        Some(h) => {
            ok &= n.wait_applied(&h, "B4 v7 spend", 90);
            let scoped = scoped_nullifier(v7_epoch as u32, nf7);
            let nfs = n.nullifiers();
            let spent = nfs.contains(&scoped);
            println!("  v7 nullifier scoped to epoch {v7_epoch}: {}", if spent { "spent ✓" } else { "NOT in the spent set ✗" });
            ok &= spent;
        }
        None => { println!("✗ B4 v7 send refused: {r}"); ok = false; }
    }
    let a = n.anchor();
    println!("END h={} v1 notes={} v7={} value_locked={}", n.height(), a["notes"], a["v7"], a["value_locked"]);
    println!("{}", if ok { "ALL STEPS OK" } else { "SOME STEP FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
