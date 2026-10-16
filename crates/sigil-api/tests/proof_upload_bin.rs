//! The binary proof upload (`/v1/shielded_send/bin`, `/v1/unshield/bin`), over a real socket.
//!
//! The claim this pins: the binary route is the JSON route with the proof carried as raw
//! bytes instead of hex — same door, same queue, same txid — at about half the upload.
//! A real STARK proof is sent both ways to two identically funded nodes; if a single
//! proof byte were mangled in transit the chokepoint would refuse it, so "ok" on the
//! binary route proves the bytes arrived intact.

use std::io::{Read, Write};
use std::sync::{Arc, RwLock};

use sigil_api::{decode_proof_upload, router, shielded::ShieldedSendRequest, AppState, PROOF_UPLOAD_MAGIC};
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_state::shielded::SHIELDED_FEE;
use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
use sigil_tx::apply_tx;

const MASTER: [u8; 32] = [0x11; 32];

fn sign_shield(sk: &ed25519_dalek::SigningKey, from: &str, amount: u128, cm: &str, fee: u128, nonce: u64) -> String {
    use ed25519_dalek::Signer;
    let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={nonce}");
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

/// A node whose pool holds exactly Alice's one 1,000,000-glyph note.
fn funded_node(alice: &ShieldedAccount, store: &mut NoteStore) -> (AppState, usize) {
    let st = AppState::new(
        Arc::new(sigil_narwhal_mempool::MempoolBackend::legacy()),
        Arc::new(RwLock::new(SigilState::new())),
    );
    let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let addr = sk.verifying_key().to_bytes();
    let mut state = st.state.write().unwrap();
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: MASTER },
                StateMutation::SetBalance { wallet: addr, token: NATIVE, amount: 1_000_000 },
            ],
        },
        1,
    )
    .unwrap();
    let (note_index, cm) = shield_note(alice, store, 1_000_000).unwrap();
    let (from, cm_hex) = (hex::encode(addr), hex::encode(cm));
    let sig = sign_shield(&sk, &from, 1_000_000, &cm_hex, 0, 1);
    st.shielded.submit_shield(&from, 1_000_000, &cm_hex, 0, &sig, 1).unwrap();
    let mut mutations = Vec::new();
    let mut landed = Vec::new();
    for signed in st.shielded.snapshot_for_mint() {
        mutations.extend(apply_tx(&state, &signed).unwrap().mutations);
        landed.push(signed.tx.hash());
    }
    commit_state_transition(&mut state, &StateTransition { at_height: 2, mutations }, 2).unwrap();
    st.shielded.confirm_applied(&landed);
    drop(state);
    (st, note_index as usize)
}

async fn serve(st: AppState) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router(st).into_make_service()).await.unwrap();
    });
    port
}

/// POST over a plain socket; returns the response body parsed as JSON.
fn post(port: u16, path: &str, content_type: &str, body: &[u8]) -> serde_json::Value {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp);
    let (head, body) = text.split_once("\r\n\r\n").expect("http response");
    // sigil-api answers chunked (since 2026-09-14); undo that the way a browser would.
    let json = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let (mut out, mut rest) = (String::new(), body);
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).expect("chunk size");
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            rest = &tail[n + 2..];
        }
        out
    } else {
        body.to_string()
    };
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("not JSON ({e}): {json}"))
}

fn frame(meta: &serde_json::Value, proof: &[u8]) -> Vec<u8> {
    let meta = serde_json::to_vec(meta).unwrap();
    let mut out = PROOF_UPLOAD_MAGIC.to_vec();
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(proof);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_route_queues_the_same_payment_as_the_json_route_at_half_the_upload() {
    let alice = ShieldedAccount::from_seed([0x5E; 32]);
    let bob = ShieldedAccount::from_seed([0xB0; 32]);

    // Two identically funded nodes, so each route gets a fresh door.
    let mut store_a = NoteStore::new();
    let (node_a, idx) = funded_node(&alice, &mut store_a);
    let mut store_b = NoteStore::new();
    let (node_b, _) = funded_node(&alice, &mut store_b);
    let pool = node_a.state.read().unwrap().shielded().padded_leaves(sigil_shield::note_v1::padding_leaf_wire);
    assert_eq!(
        pool,
        node_b.state.read().unwrap().shielded().padded_leaves(sigil_shield::note_v1::padding_leaf_wire),
    );
    assert_eq!(store_a.scan_owned(&alice, &pool), 1);
    let bundle = build_spend(
        &alice, &mut store_a, &pool, idx, SHIELDED_FEE as u64,
        &[(500_000, bob.public_key()), (500_000 - SHIELDED_FEE as u64, alice.public_key())],
    )
    .unwrap();

    let meta = serde_json::json!({
        "anchor": hex::encode(bundle.anchor),
        "nullifier": hex::encode(bundle.nullifier),
        "cm_outs": bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
        "fee": SHIELDED_FEE.to_string(),
    });
    let mut json_body = meta.clone();
    json_body["proof"] = serde_json::Value::String(hex::encode(&bundle.proof));
    let json_bytes = serde_json::to_vec(&json_body).unwrap();
    let bin_bytes = frame(&meta, &bundle.proof);

    let (pa, pb) = (serve(node_a.clone()).await, serve(node_b.clone()).await);
    let (jb, bb) = (json_bytes.clone(), bin_bytes.clone());
    let via_json = tokio::task::spawn_blocking(move || post(pa, "/v1/shielded_send", "application/json", &jb)).await.unwrap();
    let via_bin = tokio::task::spawn_blocking(move || post(pb, "/v1/shielded_send/bin", "application/octet-stream", &bb)).await.unwrap();

    assert_eq!(via_json["ok"], true, "json route: {via_json}");
    assert_eq!(via_bin["ok"], true, "binary route must accept the same valid proof: {via_bin}");
    assert_eq!(via_bin["txid"], via_json["txid"], "same payment, same txid");
    assert_eq!(node_a.shielded.pending_len(), 1);
    assert_eq!(node_b.shielded.pending_len(), 1);

    let ratio = bin_bytes.len() as f64 / json_bytes.len() as f64;
    eprintln!(
        "proof {} B · json upload {} B · binary upload {} B · ratio {ratio:.3}",
        bundle.proof.len(), json_bytes.len(), bin_bytes.len()
    );
    assert!(ratio < 0.55, "the binary upload must be about half the JSON one, got {ratio:.3}");

    // A single flipped proof byte must be refused at the door — the bytes are what count.
    let mut bad = bundle.proof.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x01;
    let (node_c, _) = funded_node(&alice, &mut NoteStore::new());
    let pc = serve(node_c.clone()).await;
    let bad_frame = frame(&meta, &bad);
    let refused = tokio::task::spawn_blocking(move || post(pc, "/v1/shielded_send/bin", "application/octet-stream", &bad_frame)).await.unwrap();
    assert_eq!(refused["ok"], false, "a mangled proof must be refused: {refused}");
    assert_eq!(node_c.shielded.pending_len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_binary_uploads_get_a_plain_error_and_queue_nothing() {
    let st = AppState::new(
        Arc::new(sigil_narwhal_mempool::MempoolBackend::legacy()),
        Arc::new(RwLock::new(SigilState::new())),
    );
    let port = serve(st.clone()).await;
    let meta = serde_json::json!({ "anchor": "aa".repeat(32), "nullifier": "bb".repeat(32),
        "cm_outs": ["11".repeat(32), "22".repeat(32)], "fee": "1" });
    let mut with_proof = meta.clone();
    with_proof["proof"] = serde_json::Value::String("00".into());
    let mut lying_len = frame(&meta, &[1, 2, 3]);
    lying_len[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty body", vec![]),
        ("wrong magic", b"SGP2\0\0\0\0{}".to_vec()),
        ("length past end", lying_len),
        ("no proof bytes", frame(&meta, &[])),
        ("metadata not JSON", { let mut f = PROOF_UPLOAD_MAGIC.to_vec(); f.extend_from_slice(&3u32.to_le_bytes()); f.extend_from_slice(b"xyz\x01"); f }),
        ("proof inside metadata", frame(&with_proof, &[1, 2, 3])),
        ("missing fields", frame(&serde_json::json!({ "anchor": "aa" }), &[1, 2, 3])),
    ];
    for (name, body) in cases {
        let r = tokio::task::spawn_blocking(move || post(port, "/v1/shielded_send/bin", "application/octet-stream", &body))
            .await
            .unwrap();
        assert_eq!(r["ok"], false, "{name}: {r}");
        assert!(r["error"].as_str().is_some_and(|e| !e.is_empty()), "{name}: {r}");
    }
    assert_eq!(st.shielded.pending_len(), 0);
}

#[test]
fn decoder_round_trips_the_request_and_keeps_the_proof_bytes_exact() {
    let meta = serde_json::json!({ "anchor": "aa", "nullifier": "bb", "cm_outs": ["cc"], "fee": "7",
        "note_ciphertexts": [null] });
    let proof: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    let (req, got) = decode_proof_upload::<ShieldedSendRequest>(&frame(&meta, &proof)).unwrap();
    assert_eq!(got, proof);
    assert_eq!((req.anchor.as_str(), req.nullifier.as_str(), req.fee), ("aa", "bb", 7));
    assert_eq!(req.cm_outs, vec!["cc".to_string()]);
    assert!(req.proof.is_empty());
}
