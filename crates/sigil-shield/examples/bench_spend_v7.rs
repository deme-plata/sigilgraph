//! Measure the v7 spend (1 input, 2 outputs): tree build, trace, prove (hiding), proof
//! size, verify. `fluxc run -p sigil-shield --profile release-fast --example bench_spend_v7`
//! Depth-15 pool (32,768 leaves, the real capacity), single-threaded.

use std::time::Instant;

use sigil_shield::note_v7::{padding_leaf, NoteV7, PoolTreeV7};
use sigil_shield::spend_full_v7::*;
use sigil_shield::zk_mask::HidingProver;
use winterfell::math::{fields::f64::BaseElement, StarkField};

fn main() {
    let note = NoteV7::new(5_000_000_000, [11, 22, 33, 44], [55, 66, 77, 88]).unwrap();
    let bob = NoteV7::new(0, [0; 4], [1, 2, 3, 4]).unwrap().owner_pk();
    let public_value = 100_000u64; // the fee
    let outs = [
        OutputV7::new(3_000_000_000, [101, 102, 103, 104], bob),
        OutputV7::new(5_000_000_000 - 3_000_000_000 - 100_000, [201, 202, 203, 204], note.owner_pk()),
    ];
    let pos = 14_348usize;

    let t0 = Instant::now();
    let mut leaves = vec![padding_leaf(); 1 << DEPTH];
    for i in 0..14_348usize {
        leaves[i] = NoteV7::new(i as u64 + 1, [i as u64; 4], [1, 2, 3, i as u64]).unwrap().commitment();
    }
    leaves[pos] = note.commitment();
    let tree = PoolTreeV7::new(leaves).unwrap();
    let path = tree.path(pos).unwrap();
    let t_tree = t0.elapsed();

    let t1 = Instant::now();
    let trace = build_spend_full_v7_trace(&note, &path, BaseElement::new(public_value), &outs, &v7_options());
    let t_trace = t1.elapsed();

    let t2 = Instant::now();
    let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(trace).expect("prove");
    let t_prove = t2.elapsed();
    let bytes = proof.to_bytes().len();

    let t3 = Instant::now();
    let pi = public_inputs_for(tree.root(), note.nullifier(pos as u64), public_value, [outs[0].commitment(), outs[1].commitment()]);
    verify_spend_full_v7(proof, pi).expect("verify");
    let t_verify = t3.elapsed();

    println!("v7 spend (1 input, {N_OUTS} outputs, depth {DEPTH}, width {WIDTH}, trace {TRACE_LEN} rows, {REAL_LEN} real)");
    println!("  tree   32,768 leaves (14,349 real)   {:>8.1} ms", t_tree.as_secs_f64() * 1e3);
    println!("  trace                                {:>8.1} ms", t_trace.as_secs_f64() * 1e3);
    println!("  prove  (hiding, scanned)             {:>8.1} ms", t_prove.as_secs_f64() * 1e3);
    println!("  proof                                {:>8} bytes", bytes);
    println!("  verify                               {:>8.2} ms", t_verify.as_secs_f64() * 1e3);
    println!("  root[0] = {:#018x}", tree.root().0[0].as_int());
}
