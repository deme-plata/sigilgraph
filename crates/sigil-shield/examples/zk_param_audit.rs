//! MEASUREMENT ONLY — v4 (leaking) vs v5 (zk-masked), same witness, same options.

use sigil_shield::membership::CompressTree;
use sigil_shield::mimc::{compress2, mimc_options};
use sigil_shield::spend_full_v4 as v4;
use sigil_shield::spend_full_v5 as v5;
use sigil_shield::zk_mask::{padded_len, scan_proof_for_secrets, zk_rows_for, HidingProver, Secret};
use winterfell::crypto::hashers::Blake3_256;
use winterfell::math::{fields::f64::BaseElement, StarkField};
use winterfell::{FieldExtension, ProofOptions, Prover};

fn e(v: u64) -> BaseElement { BaseElement::new(v) }
fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, e(v4::PK_DOMAIN)) }
fn leaf(v: BaseElement, b: BaseElement, sk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk_of(sk))
}
fn cm_out(v: BaseElement, b: BaseElement, pk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk)
}
fn pool_with(v: BaseElement, b: BaseElement, sk: BaseElement) -> (CompressTree, usize) {
    let mut leaves: Vec<BaseElement> =
        (0..7).map(|i| sigil_shield::note_v1::padding_leaf(i as u64)).collect();
    leaves.insert(3, leaf(v, b, sk));
    (CompressTree::new(leaves), 3usize)
}
fn median(mut x: Vec<f64>) -> f64 { x.sort_by(|a, b| a.partial_cmp(b).unwrap()); x[x.len() / 2] }

fn main() {
    let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
    let bob = pk_of(e(0xB0B));
    let me = pk_of(sk);
    let (tree, pos) = pool_with(value, blinding, sk);
    let path = tree.path(pos);
    let fee = e(3);
    let (a, b) = (e(50), e(47));
    let outs = [(a, e(777), bob), (b, e(888), me)];
    let opts = mimc_options();

    let secrets: Vec<Secret> = vec![
        ("recipient pk (bob)", bob), ("output amount A", a), ("output amount B", b),
        ("sender pk (me)", me), ("inner commitment", compress2(value, blinding)),
        ("spend key sk", sk), ("input value", value), ("input blinding", blinding),
    ];
    // CONTROLS — never in the trace. Small integers are the risky case: as little-endian
    // u64 they are one nonzero byte followed by seven zeros, a pattern any proof contains
    // by accident. A control hit rate matching a secret's means "coincidence", not "leak".
    let controls: Vec<Secret> = vec![
        ("control 49", e(49)), ("control 51", e(51)), ("control 48", e(48)),
        ("control 46", e(46)), ("control 4242424242", e(4242424242)),
        ("control 0xC0FFEE", e(0xC0FFEE)),
    ];

    println!("=== SIGIL shielded spend — v4 vs v5 (zk-masked) ===");
    println!("real trace rows      : {}", 4 * 64);
    println!("v5 padded trace rows : {}", padded_len(4 * 64, &opts));
    println!("openings to hide from: {} (queries {} + margin)", zk_rows_for(&opts), opts.num_queries());

    // ---------- v4 ----------
    let t = std::time::Instant::now();
    let p4 = v4::SpendFullV4Prover::new(opts.clone())
        .prove(v4::build_spend_full_v4_trace(value, blinding, sk, fee, &outs, &path))
        .expect("v4 prove");
    let v4_ms = t.elapsed().as_secs_f64() * 1000.0;
    let b4 = p4.to_bytes();
    let l4 = scan_proof_for_secrets(&b4, &secrets);
    let (c4, pv4) = (p4.security_level::<Blake3_256<BaseElement>>(true),
                     p4.security_level::<Blake3_256<BaseElement>>(false));
    let pi4 = v4::SpendFullV4PublicInputs {
        root: tree.root(), nf: compress2(sk, BaseElement::new(pos as u64)), fee,
        cm_outs: [cm_out(a, e(777), bob), cm_out(b, e(888), me)],
    };
    let ok4 = v4::verify_spend_full_v4(p4, pi4).is_ok();

    // ---------- v5 ----------
    let o5 = v5::v5_options();
    let prover5 = v5::SpendFullV5Prover::new(o5.clone());
    let t = std::time::Instant::now();
    let tr5 = v5::build_spend_full_v5_trace(value, blinding, sk, fee, &outs, &path, &o5);
    let p5 = prover5.prove(tr5).expect("v5 prove");
    let v5_ms = t.elapsed().as_secs_f64() * 1000.0;
    let b5 = p5.to_bytes();
    let l5 = scan_proof_for_secrets(&b5, &secrets);
    let (c5, pv5) = (p5.security_level::<Blake3_256<BaseElement>>(true),
                     p5.security_level::<Blake3_256<BaseElement>>(false));
    let pi5 = v5::SpendFullV5PublicInputs {
        root: tree.root(), nf: compress2(sk, BaseElement::new(pos as u64)), fee,
        cm_outs: [cm_out(a, e(777), bob), cm_out(b, e(888), me)],
    };
    let t = std::time::Instant::now();
    let r5 = v5::verify_spend_full_v5(p5, pi5);
    let v5_verify = t.elapsed().as_secs_f64() * 1000.0;
    if let Err(ref e) = r5 { println!("\n!! v5 VERIFY ERROR: {e:?}"); }
    let ok5 = r5.is_ok();

    // ---------- the leak table ----------
    println!("\n--- witness leakage (occurrences of the secret, verbatim, in the proof) ---");
    println!("{:<24} {:>10} {:>10}", "secret", "v4", "v5");
    for (name, val) in &secrets {
        let f = |ls: &Vec<sigil_shield::zk_mask::Leak>| {
            ls.iter().find(|l| l.value == val.as_int()).map(|l| l.occurrences).unwrap_or(0)
        };
        let (x, y) = (f(&l4), f(&l5));
        println!("{name:<24} {:>10} {:>10} {}", x, y,
                 if x > 0 && y == 0 { "<-- fixed" } else if y > 0 { "<-- STILL LEAKS" } else { "" });
    }

    println!("\n--- NEGATIVE CONTROLS (never in the witness) ---");
    for (name, val) in &controls {
        let f = |ls: &Vec<sigil_shield::zk_mask::Leak>| {
            ls.iter().find(|l| l.value == val.as_int()).map(|l| l.occurrences).unwrap_or(0)
        };
        let c4 = f(&scan_proof_for_secrets(&b4, &controls));
        let c5 = f(&scan_proof_for_secrets(&b5, &controls));
        println!("{name:<24} {c4:>10} {c5:>10}");
    }

    println!("\n--- cost ---");
    println!("{:<14} {:>10} {:>8} {:>8} {:>10} {:>9}", "", "bytes", "conj", "prov", "prove_ms", "verifies");
    println!("{:<14} {:>10} {:>8} {:>8} {:>10.1} {:>9}", "v4", b4.len(), c4, pv4, v4_ms, ok4);
    println!("{:<14} {:>10} {:>8} {:>8} {:>10.1} {:>9}", "v5 (zk)", b5.len(), c5, pv5, v5_ms, ok5);
    println!("{:<14} {:>9.2}x {:>8} {:>8} {:>9.2}x", "ratio",
             b5.len() as f64 / b4.len() as f64, "", "", v5_ms / v4_ms);
    println!("v5 verify: {v5_verify:.2} ms");

    // ---------- does the same spend produce different proofs? ----------
    let mk = |seed: [u8; 32]| {
        let t = v5::build_spend_full_v5_trace_seeded(value, blinding, sk, fee, &outs, &path, &opts, seed);
        v5::SpendFullV5Prover::new(opts.clone()).prove(t).expect("prove").to_bytes()
    };
    let (x, y) = (mk([1u8; 32]), mk([2u8; 32]));
    let diff = x.iter().zip(y.iter()).filter(|(p, q)| p != q).count();
    println!("\nunlinkability: two proofs of the SAME spend differ in {diff}/{} bytes ({:.1}%)",
             x.len(), 100.0 * diff as f64 / x.len() as f64);

    // ---------- prove_hiding: the production path ----------
    println!("\n--- HidingProver::prove_hiding (the path a wallet takes) ---");
    match v5::SpendFullV5Prover::new(o5.clone())
        .prove_hiding(v5::build_spend_full_v5_trace(value, blinding, sk, fee, &outs, &path, &o5))
    {
        Ok(p) => println!("  ✅ returned a hiding proof, {} bytes", p.to_bytes().len()),
        Err(err) => println!("  ⛔ REFUSED: {err}"),
    }

    // ---------- the gate refuses a leaking prover ----------
    println!("\n--- does the gate actually bite? prove v4's trace shape through v5's checker ---");
    match v5::SpendFullV5Prover::new(ProofOptions::new(84, 8, 16, FieldExtension::Quadratic, 8, 31))
        .prove_hiding(v5::build_spend_full_v5_trace_seeded(
            value, blinding, sk, fee, &outs, &path, &opts, [0u8; 32]))
    {
        Ok(_) => println!("  masked trace accepted (expected)"),
        Err(err) => println!("  REFUSED: {err}"),
    }

    // ---------- query sweep on v5 ----------
    println!("\n=== v5 ProofOptions sweep (grinding fixed at 16) ===");
    println!("{:<26} {:>9} {:>6} {:>6} {:>10} {:>10} {:>6}", "config", "bytes", "conj", "prov", "prove_ms", "verify_ms", "leaks");
    for (name, o) in [
        ("q=84 b=8  quad", ProofOptions::new(84, 8, 16, FieldExtension::Quadratic, 8, 31)),
        ("q=64 b=8  quad", ProofOptions::new(64, 8, 16, FieldExtension::Quadratic, 8, 31)),
        ("q=42 b=8  quad", ProofOptions::new(42, 8, 16, FieldExtension::Quadratic, 8, 31)),
    ] {
        let (mut pv, mut vv) = (Vec::new(), Vec::new());
        let (mut sz, mut c, mut pr, mut lk) = (0usize, 0u32, 0u32, 0usize);
        for _ in 0..5 {
            let tr = v5::build_spend_full_v5_trace(value, blinding, sk, fee, &outs, &path, &o);
            let t = std::time::Instant::now();
            let p = match v5::SpendFullV5Prover::new(o.clone()).prove(tr) {
                Ok(p) => p, Err(e) => { println!("{name:<26} PROVE FAILED {e:?}"); break; }
            };
            pv.push(t.elapsed().as_secs_f64() * 1000.0);
            let bytes = p.to_bytes();
            sz = bytes.len();
            lk = scan_proof_for_secrets(&bytes, &secrets).len();
            c = p.security_level::<Blake3_256<BaseElement>>(true);
            pr = p.security_level::<Blake3_256<BaseElement>>(false);
            let pi = v5::SpendFullV5PublicInputs {
                root: tree.root(), nf: compress2(sk, BaseElement::new(pos as u64)), fee,
                cm_outs: [cm_out(a, e(777), bob), cm_out(b, e(888), me)],
            };
            let t = std::time::Instant::now();
            let ok = v5::verify_spend_full_v5(p, pi).is_ok();
            vv.push(t.elapsed().as_secs_f64() * 1000.0);
            if !ok { println!("{name:<26} ❌ DID NOT VERIFY"); }
        }
        if pv.is_empty() { continue; }
        println!("{name:<26} {sz:>9} {c:>6} {pr:>6} {:>10.1} {:>10.2} {lk:>6}", median(pv), median(vv));
    }
}
