//! View tags end to end (1M-users step 2): a wallet finds exactly its own notes by reading
//! `/v1/shielded/tags` (34 B per note) and fetching full ciphertexts only for matches with
//! `/v1/shielded/leaves?from=&count=` — never downloading the whole pool.

use std::io::{Read, Write};
use std::sync::{Arc, RwLock};

use sigil_api::{router, AppState, SHIELDED_TAGS_MAGIC, SHIELDED_TAG_RECORD_LEN};
use sigil_shield::note_cipher::{
    enc_identity_from_seed, may_be_ours, seal_note, try_open_note, NoteCiphertext, NotePlaintext,
    ShieldedAddress, TagView,
};
use sigil_state::shielded::DENOMINATIONS;
use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
use winterfell::math::fields::f64::BaseElement;

const W: [u8; 32] = [0x77; 32];

fn addr(seed: u8) -> ShieldedAddress {
    ShieldedAddress::new(BaseElement::new(seed as u64), &enc_identity_from_seed(&[seed; 32]).public_hex())
}

/// A pool of `n` deposits; note i is sealed to Bob when `bob(i)`, else to a stranger, and
/// every 50th position carries no ciphertext and every 70th an untagged (pre-tag) one.
fn node(n: usize, bob: impl Fn(usize) -> bool) -> (AppState, Vec<usize>) {
    let st = AppState::new(
        Arc::new(sigil_narwhal_mempool::MempoolBackend::legacy()),
        Arc::new(RwLock::new(SigilState::new())),
    );
    let d = DENOMINATIONS[0];
    let mut muts = vec![
        StateMutation::SetMasterWallet { wallet: [0x11; 32] },
        StateMutation::SetBalance { wallet: W, token: NATIVE, amount: d * n as u128 },
    ];
    let mut bobs = Vec::new();
    for i in 0..n {
        let to = if bob(i) { bobs.push(i); addr(2) } else { addr(3 + (i % 200) as u8) };
        let pt = NotePlaintext::new(i as u64 + 1, BaseElement::new(i as u64 + 7));
        let ct = if i % 50 == 49 {
            None
        } else if i % 70 == 69 {
            let pk = flux_swarm_secret::parse_pubkey_hex(&to.pk_enc).unwrap();
            Some(serde_json::to_string(&flux_swarm_secret::seal(b"legacy-untagged", &pk)).unwrap())
        } else {
            Some(seal_note(&pt, &to).unwrap().0)
        };
        let mut cm = [0u8; 32];
        cm[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
        muts.push(StateMutation::Shield { from: W, amount: d, cm, note_ciphertext: ct });
    }
    commit_state_transition(&mut st.state.write().unwrap(), &StateTransition { at_height: 1, mutations: muts }, 1)
        .unwrap();
    (st, bobs)
}

async fn serve(st: AppState) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, router(st).into_make_service()).await.unwrap() });
    port
}

/// GET → (content-type, de-chunked body bytes).
fn get(port: u16, path: &str) -> (String, Vec<u8>) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut r = Vec::new();
    s.read_to_end(&mut r).unwrap();
    let he = r.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&r[..he]).to_ascii_lowercase();
    let mut body = r[he + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        let (mut out, mut i) = (Vec::new(), 0);
        loop {
            let e = i + body[i..].windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&body[i..e]).unwrap().trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&body[e + 2..e + 2 + n]);
            i = e + 2 + n + 2;
        }
        body = out;
    }
    let ct = head.lines().find_map(|l| l.strip_prefix("content-type: ")).unwrap_or("").to_string();
    (ct, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wallet_finds_exactly_its_notes_from_tags_and_a_few_fetches() {
    let n = 3000;
    let (st, bobs) = node(n, |i| i % 97 == 5);
    let port = serve(st).await;
    let bob_id = enc_identity_from_seed(&[2; 32]);

    let (ct, tags) = tokio::task::spawn_blocking(move || get(port, "/v1/shielded/tags")).await.unwrap();
    assert_eq!(ct, "application/octet-stream");
    assert_eq!(&tags[..4], &SHIELDED_TAGS_MAGIC);
    // header: magic 0..4 · epoch 4..8 · from 8..16 · total 16..24 · n 24..28 · records from 28
    let total = u64::from_le_bytes(tags[16..24].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(tags[24..28].try_into().unwrap()) as usize;
    assert_eq!((total, count), (n, n));
    assert_eq!(tags.len(), 28 + n * SHIELDED_TAG_RECORD_LEN);

    // Scan: which positions must be fetched?
    let mut fetch = Vec::new();
    let mut kinds = [0usize; 4];
    for i in 0..n {
        let r = &tags[28 + i * SHIELDED_TAG_RECORD_LEN..28 + (i + 1) * SHIELDED_TAG_RECORD_LEN];
        kinds[r[0] as usize] += 1;
        let view = TagView { eph_pk: r[2..34].try_into().unwrap(), vt: (r[0] == 2).then_some(r[1]) };
        if r[0] == 1 || r[0] == 3 || (r[0] == 2 && may_be_ours(&view, &bob_id)) {
            fetch.push(i);
        }
    }
    assert_eq!(kinds[0], n / 50, "positions without a ciphertext");
    assert!(kinds[1] > 0 && kinds[2] > 0 && kinds[3] == 0, "{kinds:?}");

    // Fetch each candidate through the leaves route with from/count, open what is ours.
    let mut found = Vec::new();
    for &i in &fetch {
        let (_, body) = tokio::task::spawn_blocking(move || get(port, &format!("/v1/shielded/leaves?from={i}&count=1")))
            .await
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(j["leaves"].as_array().unwrap().len(), 1, "count=1 serves one leaf");
        if let Some(c) = j["ciphertexts"][0].as_str() {
            if try_open_note(&NoteCiphertext(c.to_string()), &bob_id).is_ok() {
                found.push(i);
            }
        }
    }
    let tagged_bobs: Vec<usize> = bobs.iter().copied().filter(|i| i % 50 != 49 && i % 70 != 69).collect();
    assert_eq!(found, tagged_bobs, "every one of Bob's notes, and nothing else");

    // The point: Bob downloaded 34 B per note plus a handful of envelopes, not the pool.
    let (_, full) = tokio::task::spawn_blocking(move || get(port, "/v1/shielded/leaves")).await.unwrap();
    let tagged_candidates = fetch.len() - kinds[1];
    eprintln!(
        "notes {n} · tags reply {} B · full leaves reply {} B ({:.1}×) · fetched {} ({} untagged + {} tag matches, {} of them Bob's)",
        tags.len(), full.len(), full.len() as f64 / tags.len() as f64,
        fetch.len(), kinds[1], tagged_candidates, found.len()
    );
    // strangers match 1/256 by chance: expect ~11 of ~2,900 tagged; 40 is far above that
    assert!(tagged_candidates <= found.len() + 40, "tag false positives too high: {tagged_candidates}");
    assert!(tags.len() * 20 < full.len(), "the tag scan must be far smaller than the full download");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tags_paging_and_unknown_epoch() {
    let (st, _) = node(10, |_| false);
    let port = serve(st).await;
    let (_, page) = tokio::task::spawn_blocking(move || get(port, "/v1/shielded/tags?from=4&count=3")).await.unwrap();
    assert_eq!(u32::from_le_bytes(page[4..8].try_into().unwrap()), 0, "live epoch 0");
    assert_eq!(u64::from_le_bytes(page[8..16].try_into().unwrap()), 4, "from echoed");
    assert_eq!(u32::from_le_bytes(page[24..28].try_into().unwrap()), 3, "count honoured");
    assert_eq!(page.len(), 28 + 3 * SHIELDED_TAG_RECORD_LEN);
    let (_, past) = tokio::task::spawn_blocking(move || get(port, "/v1/shielded/tags?from=99")).await.unwrap();
    assert_eq!(u32::from_le_bytes(past[24..28].try_into().unwrap()), 0, "past the end = empty, not an error");
    let (ct, bad) = tokio::task::spawn_blocking(move || get(port, "/v1/shielded/tags?epoch=7")).await.unwrap();
    assert!(ct.starts_with("application/json"));
    let j: serde_json::Value = serde_json::from_slice(&bad).unwrap();
    assert_eq!(j["ok"], false);
}
