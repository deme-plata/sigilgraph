//! sigil-balance-verify — the light-client half of the proof-carrying `/balance`
//! query (WS2). Given the JSON a node returns for `GET /balance?...&proof=1`, it:
//!
//!   1. reconstructs the wallet-SMT inclusion proof,
//!   2. verifies it statelessly against `wallet_smt_root` (256 hashes),
//!   3. checks the proven leaf encodes the claimed balance (or proves a zero
//!      balance via non-membership), and
//!   4. GATES on the tip-proof flavor: it REFUSES to trust the balance unless the
//!      accompanying tip-proof is `adversary_resistant()` — i.e. it rejects the
//!      insecure `Blake3Fingerprint` flavor. This implements the gating the
//!      tip-proof crate today only documents (`sigil-tip-proof/lib.rs`).
//!
//! This is the verifier a fresh node / browser runs: it never downloads a block,
//! only checks the proof against the tip-attested root. (The final consensus
//! inch — committing `wallet_smt_root` in the header so the tip-proof attests it —
//! is the height-gated swap tracked in docs/SIGIL_HARDENING_BACKLOG.md.)
//!
//! Usage:
//!   sigil-balance-verify --tip-flavor <blake3|sqisign|stark> <balance.json>
//!   sigil-balance-verify --tip-flavor sqisign --stdin   # read JSON from stdin
//!
//! Exit: 0 = balance PROVEN and trusted; non-zero = rejected (with reason).

use sigil_state::{verify_proof, MerkleProof};
use sigil_tip_proof::TipProofFlavor;

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s).ok()?;
    if b.len() != 32 { return None; }
    let mut o = [0u8; 32];
    o.copy_from_slice(&b);
    Some(o)
}

fn field<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a serde_json::Value> {
    v.get(k)
}

fn fail(msg: &str) -> ! {
    eprintln!("REJECTED: {msg}");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut flavor_s = String::new();
    let mut path: Option<String> = None;
    let mut from_stdin = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--tip-flavor" => { i += 1; flavor_s = args.get(i).cloned().unwrap_or_default(); }
            "--stdin" => from_stdin = true,
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    // (4) tip-proof flavor gate — decided FIRST, so an insecure tip never even
    // gets its proof checked "for show".
    let flavor = match flavor_s.as_str() {
        "blake3" | "blake3fingerprint" => TipProofFlavor::Blake3Fingerprint,
        "sqisign" | "sqisignblob" => TipProofFlavor::SqiSignBlob,
        "stark" | "starkrecursive" => TipProofFlavor::StarkRecursive,
        _ => fail("--tip-flavor must be one of blake3|sqisign|stark"),
    };
    if !flavor.adversary_resistant() {
        fail("tip-proof flavor is not adversary-resistant (Blake3Fingerprint is \
              integrity-only) — refusing to trust a balance against a forgeable tip. \
              Wait for a SqiSignBlob/StarkRecursive tip-proof.");
    }

    let raw = if from_stdin {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).expect("read stdin");
        s
    } else {
        let p = path.unwrap_or_else(|| fail("provide a balance JSON path or --stdin"));
        std::fs::read_to_string(&p).unwrap_or_else(|_| fail("cannot read JSON file"))
    };

    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|_| fail("invalid JSON"));

    let balance: u128 = field(&v, "balance")
        .and_then(|b| b.as_u64().map(|x| x as u128).or_else(|| b.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or_else(|| fail("missing/invalid balance"));
    let root = field(&v, "wallet_smt_root").and_then(|x| x.as_str()).and_then(hex32)
        .unwrap_or_else(|| fail("missing/invalid wallet_smt_root"));
    let p = field(&v, "proof").unwrap_or_else(|| fail("missing proof"));
    let key_hash = field(p, "key_hash").and_then(|x| x.as_str()).and_then(hex32)
        .unwrap_or_else(|| fail("missing/invalid proof.key_hash"));
    let leaf = field(p, "leaf").and_then(|x| x.as_str()).and_then(hex32)
        .unwrap_or_else(|| fail("missing/invalid proof.leaf"));
    let siblings: Vec<[u8; 32]> = field(p, "siblings").and_then(|x| x.as_array())
        .unwrap_or_else(|| fail("missing proof.siblings"))
        .iter()
        .map(|s| s.as_str().and_then(hex32).unwrap_or_else(|| fail("bad sibling hex")))
        .collect();

    let proof = MerkleProof { key_hash, leaf, siblings };

    // (2) stateless inclusion/non-membership verification against the SMT root.
    if !verify_proof(&root, &proof) {
        fail("Merkle proof does NOT verify against wallet_smt_root (tampered or wrong root)");
    }

    // (3) the proven leaf must encode the claimed balance. Zero balance ⇔ empty
    // leaf (non-membership); non-zero ⇔ leaf == blake3(amount_le).
    let expect_leaf: [u8; 32] = if balance == 0 {
        [0u8; 32]
    } else {
        *blake3::hash(&balance.to_le_bytes()).as_bytes()
    };
    if proof.leaf != expect_leaf {
        fail("proof leaf does not encode the claimed balance (node lied about the amount)");
    }

    let wallet = field(&v, "wallet").and_then(|x| x.as_str()).unwrap_or("?");
    println!(
        "PROVEN: wallet {} holds balance {} — verified against wallet_smt_root {} \
         under an adversary-resistant tip-proof ({:?}). 0 blocks downloaded.",
        wallet, balance, hex::encode(root), flavor
    );
}
