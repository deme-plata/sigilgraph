//! sigil-tip-emit — turn a live `{height, roots}` into a verifiable `TipProof`,
//! with optional SQIsign Level-5 signing and DNS-anchor (`v=sigil1`) emission.
//!
//! Modes:
//!   sigil-tip-emit                stdin {height,roots} -> Blake3Fingerprint TipProof JSON (default)
//!   sigil-tip-emit --sqisign      stdin {height,roots} -> SqiSignBlob TipProof JSON (producer-signed)
//!   sigil-tip-emit --anchor       stdin {height,roots} -> `v=sigil1` DNS TXT (SQIsign-signed, self-verified)
//!   sigil-tip-emit keygen         generate + persist the producer SQIsign L5 key, print pk_hex + key_id
//!   sigil-tip-emit pubkey         print the persisted producer pk_hex + key_id (for verifier pinning)
//!
//! Key file: `SIGIL_TIP_KEY_PATH` (default `~/.sigil-tip-key.json`) = `{"sk_hex","pk_hex"}` (chmod 600).
//! `key_id` = first 16 hex chars of BLAKE3(pk) — the `k=` field a verifier matches against the
//! pinned producer key it fetched out of band.

use std::io::{Read, Write};

use base64::Engine;
use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;

#[derive(serde::Deserialize)]
struct Input {
    height: u64,
    roots: StateRoots,
}

fn key_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SIGIL_TIP_KEY_PATH") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    std::path::PathBuf::from(home).join(".sigil-tip-key.json")
}

/// 16-hex-char producer key fingerprint = BLAKE3(pk)[..8].
fn key_id(pk: &[u8]) -> String {
    hex::encode(&blake3::hash(pk).as_bytes()[..8])
}

fn load_key() -> Result<(Vec<u8>, Vec<u8>), String> {
    let p = key_path();
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("read key {}: {e}", p.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("parse key: {e}"))?;
    let sk = hex::decode(v["sk_hex"].as_str().ok_or("key file missing sk_hex")?)
        .map_err(|e| format!("sk_hex: {e}"))?;
    let pk = hex::decode(v["pk_hex"].as_str().ok_or("key file missing pk_hex")?)
        .map_err(|e| format!("pk_hex: {e}"))?;
    Ok((sk, pk))
}

fn read_input() -> Input {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        eprintln!("sigil-tip-emit: empty stdin (expected {{height, roots}})");
        std::process::exit(2);
    }
    match serde_json::from_str(&buf) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("sigil-tip-emit: bad input json: {e}");
            std::process::exit(2);
        }
    }
}

fn die(msg: String) -> ! {
    eprintln!("sigil-tip-emit: {msg}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(|s| s.as_str()).unwrap_or("");

    match mode {
        "keygen" => {
            let p = key_path();
            if p.exists() {
                let (_, pk) = load_key().unwrap_or_else(|e| die(e));
                eprintln!("key already exists at {} (idempotent)", p.display());
                println!("{{\"pk_hex\":\"{}\",\"key_id\":\"{}\"}}", hex::encode(&pk), key_id(&pk));
                return;
            }
            let (sk, pk) = flux_sqisign::keygen();
            let json = format!(
                "{{\"sk_hex\":\"{}\",\"pk_hex\":\"{}\"}}",
                hex::encode(&sk),
                hex::encode(&pk)
            );
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&p, json).unwrap_or_else(|e| die(format!("write key: {e}")));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
            eprintln!("generated SQIsign L5 producer key → {}", p.display());
            println!("{{\"pk_hex\":\"{}\",\"key_id\":\"{}\"}}", hex::encode(&pk), key_id(&pk));
        }
        "pubkey" => {
            let (_, pk) = load_key().unwrap_or_else(|e| die(e));
            println!("{{\"pk_hex\":\"{}\",\"key_id\":\"{}\"}}", hex::encode(&pk), key_id(&pk));
        }
        "--sqisign" => {
            let input = read_input();
            let (sk, pk) = load_key()
                .unwrap_or_else(|e| die(format!("no producer key ({e}); run `sigil-tip-emit keygen`")));
            let proof = TipProof::new_sqisign(input.height, input.roots, &sk, &pk)
                .unwrap_or_else(|e| die(format!("sqisign sign: {e:?}")));
            std::io::stdout().write_all(&proof.encode_json()).expect("write proof");
        }
        "--anchor" => {
            let input = read_input();
            let (sk, pk) = load_key()
                .unwrap_or_else(|e| die(format!("no producer key ({e}); run `sigil-tip-emit keygen`")));
            let proof = TipProof::new_sqisign(input.height, input.roots, &sk, &pk)
                .unwrap_or_else(|e| die(format!("sqisign sign: {e:?}")));
            // Self-verify the signature before publishing the anchor — never emit a record
            // that won't verify under the very key we just signed with.
            proof
                .verify_sqisign(sigil_tip_proof::NETWORK_ID_BYTES, &pk)
                .unwrap_or_else(|e| die(format!("self-verify failed: {e:?}")));
            let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&proof.signature);
            // `d=` is BLAKE3 over the canonical signing bytes (height+roots+network_id).
            let digest = proof.fingerprint();
            let txt = sigil_dns_anchor::encode_tip(proof.height, &digest, &sig_b64, &key_id(&pk));
            // Round-trip through the decoder so we only ever print a structurally-valid record.
            if let Err(e) = sigil_dns_anchor::decode(&txt) {
                die(format!("anchor failed self-decode: {e}"));
            }
            println!("{txt}");
        }
        _ => {
            // default: Blake3Fingerprint (backward compatible with the existing pipeline)
            let input = read_input();
            let proof = TipProof::new_blake3(input.height, input.roots);
            std::io::stdout().write_all(&proof.encode_json()).expect("write proof");
        }
    }
}
