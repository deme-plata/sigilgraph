// THROWAWAY live-test driver (2026-08-24) — not part of the product, not committed
// with the fix. Drives a real shielded send against the live production API using
// throwaway seeds, to prove the note-delivery feature works end to end for real.
//
// Usage:
//   cargo run --example live_shielded_test -- keys <seed_hex>
//   cargo run --example live_shielded_test -- spend <miner_seed_hex> <recipient_seed_hex> <node_url>
//   cargo run --example live_shielded_test -- scan <recipient_seed_hex> <node_url>
//   cargo run --example live_shielded_test -- transparent-probe <seed_hex> <node_url>

use ed25519_dalek::{Signer, SigningKey};
use sigil_shield::note_cipher::{enc_identity_from_seed, seal_note, try_open_note, NotePlaintext, NoteCiphertext, ShieldedAddress};
use sigil_shield::note_v1::{from_wire, padding_leaf_wire, to_wire, coinbase_commitment_wire};
use sigil_shield::wallet::{build_spend, NoteStore, ShieldedAccount};

fn seed_bytes(s: &str) -> [u8; 32] {
    let v = hex::decode(s.trim()).expect("hex");
    v.try_into().expect("32 bytes")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("keys") => {
            let seed = seed_bytes(&args[2]);
            let sk = SigningKey::from_bytes(&seed);
            let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
            let acct = ShieldedAccount::from_seed(seed);
            let pk_shield_hex = hex::encode(to_wire(acct.public_key()));
            let pk_encrypt_hex = enc_identity_from_seed(&seed).public_hex();
            println!("wallet={wallet_hex}");
            println!("pk_shield={pk_shield_hex}");
            println!("pk_encrypt={pk_encrypt_hex}");
        }
        Some("transparent-probe") => {
            let seed = seed_bytes(&args[2]);
            let node = &args[3];
            let sk = SigningKey::from_bytes(&seed);
            let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
            let to_hex = wallet_hex.clone(); // send to self, amount doesn't matter, we expect rejection
            let amount: u128 = 1;
            let fee: u128 = 0;
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            // best-effort canonical message guess mirroring send::submit's pattern used elsewhere
            let msg = format!("sigil-rpc/v1|send|{wallet_hex}|{to_hex}|{amount}|{fee}|nonce={nonce}");
            let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
            let body = serde_json::json!({
                "from": wallet_hex, "to": to_hex, "amount": amount,
                "token": "SIGIL", "fee": fee.to_string(), "sig": sig, "req_nonce": nonce,
            });
            let client = reqwest::blocking::Client::new();
            let resp = client.post(format!("{node}/v1/send")).json(&body).send().expect("send req");
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            println!("STATUS={status}");
            println!("BODY={text}");
        }
        Some("spend") => {
            let miner_seed = seed_bytes(&args[2]);
            let recipient_seed = seed_bytes(&args[3]);
            let node = &args[4];
            let client = reqwest::blocking::Client::new();

            let miner = ShieldedAccount::from_seed(miner_seed);
            let miner_pk_wire = to_wire(miner.public_key());
            let recipient = ShieldedAccount::from_seed(recipient_seed);
            let recipient_enc = enc_identity_from_seed(&recipient_seed);
            let recipient_addr = ShieldedAddress::new(recipient.public_key(), &recipient_enc.public_hex());

            // Fetch live pool leaves + ciphertexts.
            let leaves_resp: serde_json::Value = client.get(format!("{node}/v1/shielded/leaves")).send().unwrap().json().unwrap();
            let leaves: Vec<[u8;32]> = leaves_resp["leaves"].as_array().unwrap().iter()
                .map(|v| { let b = hex::decode(v.as_str().unwrap()).unwrap(); let mut a=[0u8;32]; a.copy_from_slice(&b); a })
                .collect();
            println!("pool_leaves_count={}", leaves.len());

            // Find which leaf/height is OUR coinbase note by scanning recent heights.
            let status: serde_json::Value = client.get(format!("{node}/v1/supply")).send().unwrap().json().unwrap();
            println!("supply_probe={status}");

            // We don't know our exact mint height directly; scan a window of recent heights
            // (last 200) for a coinbase commitment matching our pk_shield.
            let net_status: serde_json::Value = client.get(format!("{node}/api/v1/status")).send().ok()
                .and_then(|r| r.json().ok()).unwrap_or(serde_json::json!({}));
            let cur_height = net_status.get("current_height").and_then(|v| v.as_u64())
                .or_else(|| net_status.get("height").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            println!("current_height_probe={cur_height}");

            let mut found: Option<(usize, u64, u128)> = None; // (position, height, amount)
            'outer: for (pos, cm) in leaves.iter().enumerate() {
                // try candidate amounts across denominations for a window of heights;
                // this is O(heights * denominations) but the window + ladder are both small.
                for h in cur_height.saturating_sub(400)..=cur_height.saturating_add(5) {
                    for amt in sigil_state_shielded_denominations() {
                        if let Some(expect) = coinbase_commitment_wire(h, &miner_pk_wire, amt) {
                            if expect == *cm {
                                found = Some((pos, h, amt));
                                break 'outer;
                            }
                        }
                    }
                }
            }
            let Some((position, height, amount)) = found else {
                println!("NO_COINBASE_NOTE_FOUND_YET");
                std::process::exit(2);
            };
            println!("found_note position={position} height={height} amount={amount}");

            // Build the note store with this ONE known note.
            let mut store = NoteStore::new();
            store.receive(amount as u64, sigil_shield::note_v1::coinbase_blinding(height, miner.public_key()));
            let pool = padded_leaves(&leaves);
            let scanned = store.scan_owned(&miner, &pool);
            println!("scanned_owned={scanned}");
            if scanned == 0 { println!("SCAN_FAILED"); std::process::exit(3); }

            // Spend: send `send_amt` to recipient, rest (minus fee) back to self as change.
            let fee: u64 = sigil_state_shielded_fee();
            let send_amt: u64 = 1u64.min(amount as u64); // smallest possible real transfer
            let change: u64 = (amount as u64).saturating_sub(fee).saturating_sub(send_amt);
            println!("plan: amount={amount} fee={fee} send_amt={send_amt} change={change}");
            if (amount as u64) < fee + send_amt {
                println!("NOTE_TOO_SMALL_FOR_FEE");
                std::process::exit(4);
            }

            let outs = [(send_amt, recipient_addr.shield_key().unwrap()), (change, miner.public_key())];
            let bundle = build_spend(&miner, &mut store, &pool, 0usize, fee, &outs).expect("build_spend");
            println!("bundle.anchor={}", hex::encode(bundle.anchor));
            println!("bundle.nullifier={}", hex::encode(bundle.nullifier));

            // Seal recipient's output (index 0) ciphertext.
            let (rv, rb) = bundle.out_preimages[0];
            let ct: NoteCiphertext = seal_note(&NotePlaintext{ value: rv, blinding: rb }, &recipient_addr).unwrap();

            let submit_body = serde_json::json!({
                "anchor": hex::encode(bundle.anchor),
                "nullifier": hex::encode(bundle.nullifier),
                "cm_outs": bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
                "fee": (fee as u128).to_string(),
                "proof": hex::encode(&bundle.proof),
                "note_ciphertexts": [ct.0, serde_json::Value::Null],
            });
            let resp = client.post(format!("{node}/v1/shielded_send")).json(&submit_body).send().unwrap();
            let st = resp.status();
            let body: serde_json::Value = resp.json().unwrap_or(serde_json::json!({"raw":"unparseable"}));
            println!("SUBMIT_STATUS={st}");
            println!("SUBMIT_BODY={body}");
        }
        Some("scan") => {
            let recipient_seed = seed_bytes(&args[2]);
            let node = &args[3];
            let client = reqwest::blocking::Client::new();
            let recipient = ShieldedAccount::from_seed(recipient_seed);
            let recipient_enc = enc_identity_from_seed(&recipient_seed);

            let leaves_resp: serde_json::Value = client.get(format!("{node}/v1/shielded/leaves")).send().unwrap().json().unwrap();
            let leaves: Vec<[u8;32]> = leaves_resp["leaves"].as_array().unwrap().iter()
                .map(|v| { let b = hex::decode(v.as_str().unwrap()).unwrap(); let mut a=[0u8;32]; a.copy_from_slice(&b); a })
                .collect();
            let cts_raw = leaves_resp["ciphertexts"].as_array().cloned().unwrap_or_default();
            println!("pool_leaves_count={} ciphertexts_count={}", leaves.len(), cts_raw.len());

            let mut store = NoteStore::new();
            let mut found_any = 0;
            for c in &cts_raw {
                if c.is_null() { continue; }
                let json_str = c.as_str().unwrap_or_default();
                if json_str.is_empty() { continue; }
                let nc = NoteCiphertext(json_str.to_string());
                if let Ok(pt) = try_open_note(&nc, &recipient_enc) {
                    if store.receive(pt.value, pt.blinding) { found_any += 1; }
                }
            }
            println!("newly_discovered={found_any}");
            let pool = padded_leaves(&leaves);
            let located = store.scan_owned(&recipient, &pool);
            println!("located={located}");
            println!("balance={}", store.balance());
            for n in &store.notes {
                println!("note value={} position={:?} spent={}", n.value, n.position, n.spent);
            }
        }
        Some("register") => manual_register(&args[2], &args[3]),
        _ => eprintln!("usage: keys|transparent-probe|spend|scan|register ..."),
    }
}

fn padded_leaves(leaves: &[[u8;32]]) -> Vec<[u8;32]> {
    const CAP: usize = 1 << 15;
    let mut v = leaves.to_vec();
    for i in v.len()..CAP { v.push(padding_leaf_wire(i as u64)); }
    v
}

fn sigil_state_shielded_denominations() -> Vec<u128> {
    vec![1,2,5,10,20,50,100,200,500,1_000,2_000,5_000,10_000,20_000,50_000,100_000,
         200_000,500_000,1_000_000,2_000_000,5_000_000,10_000_000,20_000_000,50_000_000,
         100_000_000,200_000_000,500_000_000]
}
fn sigil_state_shielded_fee() -> u64 { 1_000 }

#[allow(dead_code)]
fn _unused(_: fn([u8;32]) -> Option<sigil_shield::note_v1::NoteError>) { let _ = from_wire; }

#[allow(dead_code)]
fn manual_register(seed_hex: &str, node: &str) {
    let seed = seed_bytes(seed_hex);
    let sk = SigningKey::from_bytes(&seed);
    let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
    let acct = ShieldedAccount::from_seed(seed);
    let pk_shield_hex = hex::encode(to_wire(acct.public_key()));
    let pk_encrypt_hex = enc_identity_from_seed(&seed).public_hex();
    let fee: u128 = 0;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let msg = format!("sigil-rpc/v1|shield-register|{wallet_hex}|{pk_shield_hex}|{pk_encrypt_hex}|{fee}|nonce={nonce}");
    let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
    let body = serde_json::json!({"wallet": wallet_hex, "pk_shield": pk_shield_hex, "pk_encrypt": pk_encrypt_hex, "fee": fee.to_string(), "sig": sig, "req_nonce": nonce});
    let client = reqwest::blocking::Client::new();
    let resp = client.post(format!("{node}/v1/shielded/register")).json(&body).send().unwrap();
    println!("STATUS={}", resp.status());
    println!("BODY={}", resp.text().unwrap());
    println!("wallet_full={wallet_hex}");
}
