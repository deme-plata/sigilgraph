//! Wide merge (2026-10-16): what does a K-input v7 merge cost? K = 2, 4, 8, 16 — prove, verify,
//! proof bytes, trace width — against the deployed 1-input and 2-input circuits. Single thread.
//! `fluxc run -p sigil-shield --release --example bench_v7_k [-- ROUNDS]`
use std::time::Instant;

use sigil_shield::note_v7::{padding_leaf, Digest4, NoteV7, PoolTreeV7};
use sigil_shield::spend_full_v7::{v7_options, OutputV7, DEPTH};
use sigil_shield::spend_full_v7_k::*;
use sigil_shield::zk_mask::HidingProver;
use winterfell::math::{fields::f64::BaseElement, StarkField};

fn median(mut v: Vec<f64>) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] }

fn main() {
    let rounds: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(3);
    let sk = [0xAAAAu64, 0xBBBB, 0xCCCC, 0xDDDD];
    let bob: Digest4 = NoteV7::new(0, [0; 4], [0xB0B; 4]).unwrap().owner_pk();
    println!("v7 wide merge, {rounds} rounds, single thread, hiding prover");
    println!("{:>3} {:>6} {:>10} {:>10} {:>10} {:>12}", "K", "width", "prove ms", "verify ms", "bytes", "bytes/input");
    for k in [2usize, 4, 8, 16] {
        let ns: Vec<NoteV7> = (0..k).map(|j| NoteV7::new(1_000_000 * (j as u64 + 1), [j as u64 + 1, 2, 3, 4], sk).unwrap()).collect();
        let ps: Vec<usize> = (0..k).map(|j| 300 + 1_877 * j).collect();
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        for (n, &p) in ns.iter().zip(&ps) { leaves[p] = n.commitment(); }
        let tree = PoolTreeV7::new(leaves).unwrap();
        let paths: Vec<_> = ps.iter().map(|&p| tree.path(p).unwrap()).collect();
        let total: u64 = ns.iter().map(|n| n.value.as_int()).sum();
        let outs = [OutputV7::new(3_000_000, [5, 6, 7, 8], bob), OutputV7::new(total - 3_000_000 - 7, [9, 10, 11, 12], ns[0].owner_pk())];
        let nfs: Vec<Digest4> = ns.iter().zip(&ps).map(|(n, &p)| n.nullifier(p as u64)).collect();
        let pi = public_inputs_for_k(tree.root(), &nfs, 7, [outs[0].commitment(), outs[1].commitment()]);
        let (mut pr, mut vr, mut bytes) = (vec![], vec![], 0);
        for _ in 0..rounds {
            let t = build_spend_full_v7k_trace(&ns, &paths, BaseElement::new(7), &outs, &v7_options());
            let t0 = Instant::now();
            let proof = SpendFullV7kProver::new(v7_options()).prove_hiding(t).expect("prove");
            pr.push(t0.elapsed().as_secs_f64() * 1e3);
            bytes = proof.to_bytes().len();
            let t0 = Instant::now();
            verify_spend_full_v7k(proof, pi.clone()).expect("verify");
            vr.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        println!("{:>3} {:>6} {:>10.1} {:>10.2} {:>10} {:>12}", k, width(k), median(pr), median(vr), bytes, bytes / k);
    }
}
