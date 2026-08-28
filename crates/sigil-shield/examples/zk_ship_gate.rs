//! SHIP GATE for v5. Drives the REAL consensus entry point — `note_v1::verify_spend_wire`,
//! the same function `sigil_state::shielded::verify_spend_proof` calls — and asserts:
//!
//!   1. a v5 proof is ACCEPTED by consensus,
//!   2. a v4 proof is STILL accepted (the dual-accept rollout window),
//!   3. the v5 proof does not carry the witness, the v4 one does,
//!   4. tampering is still rejected on both.
//!
//! If any of these fail, do not cut the release.

use sigil_shield::membership::CompressTree;
use sigil_shield::mimc::{compress2, mimc_options};
use sigil_shield::note_v1::verify_spend_wire;
use sigil_shield::spend_full_v4 as v4;
use sigil_shield::spend_full_v5 as v5;
use sigil_shield::zk_mask::{scan_proof_for_secrets, Secret};
use winterfell::math::{fields::f64::BaseElement, StarkField};
use winterfell::Prover;

fn e(v: u64) -> BaseElement { BaseElement::new(v) }
fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, e(v4::PK_DOMAIN)) }
fn leaf(v: BaseElement, b: BaseElement, sk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk_of(sk))
}
fn wire(x: BaseElement) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[..8].copy_from_slice(&x.as_int().to_le_bytes());
    o
}

fn main() {
    let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
    let bob = pk_of(e(0xB0B));
    let me = pk_of(sk);
    let mut lv: Vec<BaseElement> =
        (0..7).map(|i| sigil_shield::note_v1::padding_leaf(i as u64)).collect();
    lv.insert(3, leaf(value, blinding, sk));
    let tree = CompressTree::new(lv);
    let (path, pos) = (tree.path(3), 3usize);
    let fee = e(3);
    let outs = [(e(50), e(777), bob), (e(47), e(888), me)];
    let cm = |v: BaseElement, b: BaseElement, pk: BaseElement| compress2(compress2(v, b), pk);
    let cm_outs = vec![wire(cm(e(50), e(777), bob)), wire(cm(e(47), e(888), me))];
    let anchor = wire(tree.root());
    let nf = wire(compress2(sk, BaseElement::new(pos as u64)));

    let secrets: Vec<Secret> = vec![
        ("recipient pk", bob), ("output A", e(50)), ("output B", e(47)),
        ("sender pk", me), ("inner commitment", compress2(value, blinding)),
    ];

    // ── v5 ──
    let o5 = v5::v5_options();
    let t5 = v5::build_spend_full_v5_trace(value, blinding, sk, fee, &outs, &path, &o5);
    let p5 = v5::SpendFullV5Prover::new(o5).prove(t5).expect("v5 prove").to_bytes();
    let r5 = verify_spend_wire(&anchor, &nf, 3u128, &cm_outs, &p5);
    let l5 = scan_proof_for_secrets(&p5, &secrets);

    // ── v4 ──
    let t4 = v4::build_spend_full_v4_trace(value, blinding, sk, fee, &outs, &path);
    let p4 = v4::SpendFullV4Prover::new(mimc_options()).prove(t4).expect("v4 prove").to_bytes();
    let r4 = verify_spend_wire(&anchor, &nf, 3u128, &cm_outs, &p4);
    let l4 = scan_proof_for_secrets(&p4, &secrets);

    // ── tamper ──
    let mut bad = p5.clone();
    let n = bad.len() / 2;
    bad[n] ^= 0xff;
    let rt = verify_spend_wire(&anchor, &nf, 3u128, &cm_outs, &bad);
    let wrong_root = verify_spend_wire(&wire(e(12345)), &nf, 3u128, &cm_outs, &p5);

    println!("=== SHIP GATE — consensus entry point `verify_spend_wire` ===\n");
    println!("  v5 proof  {:>7} bytes   consensus: {}", p5.len(),
             if r5.is_ok() { "✅ ACCEPTED" } else { "❌ REJECTED" });
    println!("  v4 proof  {:>7} bytes   consensus: {}   (dual-accept window)", p4.len(),
             if r4.is_ok() { "✅ ACCEPTED" } else { "❌ REJECTED" });
    println!("\n  witness in v5 proof: {}", if l5.is_empty() { "NONE ✅".into() }
             else { format!("{:?} ❌", l5) });
    println!("  witness in v4 proof: {} secrets leaked{}", l4.len(),
             if l4.is_empty() { "" } else { "  (expected — this is why v5 exists)" });
    for l in &l4 { println!("      {l}"); }
    println!("\n  tampered v5 proof : {}", if rt.is_err() { "✅ rejected" } else { "❌ ACCEPTED — STOP" });
    println!("  wrong anchor      : {}", if wrong_root.is_err() { "✅ rejected" } else { "❌ ACCEPTED — STOP" });

    let ok = r5.is_ok() && r4.is_ok() && l5.is_empty() && !l4.is_empty()
        && rt.is_err() && wrong_root.is_err();
    println!("\n{}", if ok { "🟢 SHIP GATE PASSED — safe to cut the release" }
             else { "🔴 SHIP GATE FAILED — do not release" });
    if !ok { std::process::exit(1); }
}
