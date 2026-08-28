//! Does anything OUTSIDE the trace openings still depend on the witness?
//!
//! v5 masks the trace, so trace query openings and the out-of-domain trace frame are
//! covered. A STARK proof also carries the constraint-composition commitment and its
//! openings, the composition OOD values H_i(z), and the DEEP/FRI layers. Those are
//! derived from the trace, but "derived" is not "covered" — so measure it.
//!
//! Method — a deterministic-channel hunt:
//!   1. Prove the SAME witness under N different mask seeds. Any byte position whose value
//!      is identical across all N does not depend on the mask. Call that the INVARIANT SET.
//!   2. Do the same for a SECOND witness.
//!   3. A byte that is invariant under BOTH witnesses but holds DIFFERENT values is a
//!      deterministic, witness-dependent channel — a leak the mask does not reach.
//!   4. Subtract the bytes explained by the public inputs, which are supposed to differ.
//!
//! A byte that varies with the seed is masked. A byte that is invariant and identical
//! across witnesses is structure (lengths, domain parameters, public inputs).

use sigil_shield::membership::CompressTree;
use sigil_shield::mimc::compress2;
use sigil_shield::spend_full_v5 as v5;
use winterfell::math::{fields::f64::BaseElement, StarkField};
use winterfell::Prover;

fn e(v: u64) -> BaseElement { BaseElement::new(v) }
fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, e(v5::PK_DOMAIN)) }
fn leaf(v: BaseElement, b: BaseElement, sk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk_of(sk))
}
fn pool_with(v: BaseElement, b: BaseElement, sk: BaseElement) -> (CompressTree, usize) {
    let mut lv: Vec<BaseElement> =
        (0..7).map(|i| sigil_shield::note_v1::padding_leaf(i as u64)).collect();
    lv.insert(3, leaf(v, b, sk));
    (CompressTree::new(lv), 3usize)
}

struct Witness { value: BaseElement, blinding: BaseElement, sk: BaseElement,
                 fee: BaseElement, outs: [(BaseElement, BaseElement, BaseElement); 2] }

/// N proofs of one witness under N seeds; returns (proofs, byte positions constant in all).
fn invariants(w: &Witness, n: usize) -> (Vec<Vec<u8>>, Vec<usize>) {
    let o = v5::v5_options();
    let (tree, pos) = pool_with(w.value, w.blinding, w.sk);
    let path = tree.path(pos);
    let mut proofs = Vec::new();
    for i in 0..n {
        let mut seed = [0u8; 32];
        seed[0] = i as u8; seed[1] = 0xA7;
        let tr = v5::build_spend_full_v5_trace_seeded(
            w.value, w.blinding, w.sk, w.fee, &w.outs, &path, &o, seed);
        proofs.push(v5::SpendFullV5Prover::new(o.clone()).prove(tr).expect("prove").to_bytes());
    }
    let len = proofs.iter().map(|p| p.len()).min().unwrap();
    let inv: Vec<usize> = (0..len)
        .filter(|&i| proofs.iter().all(|p| p[i] == proofs[0][i]))
        .collect();
    (proofs, inv)
}

fn main() {
    const N: usize = 12;
    let sk_a = e(0xDEAD);
    let sk_b = e(0xBEEF);
    let a = Witness { value: e(100), blinding: e(4242), sk: sk_a, fee: e(3),
        outs: [(e(50), e(777), pk_of(e(0xB0B))), (e(47), e(888), pk_of(sk_a))] };
    // A genuinely different witness: different amounts, keys, blindings.
    let b = Witness { value: e(100), blinding: e(9999), sk: sk_b, fee: e(3),
        outs: [(e(61), e(555), pk_of(e(0xC0C))), (e(36), e(222), pk_of(sk_b))] };

    println!("=== residual-channel probe: {N} seeds x 2 witnesses ===\n");
    let (pa, ia) = invariants(&a, N);
    let (pb, ib) = invariants(&b, N);
    let len = pa[0].len().min(pb[0].len());

    println!("proof length            : {} bytes", pa[0].len());
    println!("mask-invariant bytes (A): {} ({:.2}%)", ia.len(), 100.0 * ia.len() as f64 / pa[0].len() as f64);
    println!("mask-invariant bytes (B): {} ({:.2}%)", ib.len(), 100.0 * ib.len() as f64 / pb[0].len() as f64);

    // Bytes invariant under BOTH witnesses.
    let sa: std::collections::HashSet<usize> = ia.iter().copied().collect();
    let both: Vec<usize> = ib.iter().copied().filter(|i| sa.contains(i) && *i < len).collect();
    let differing: Vec<usize> = both.iter().copied().filter(|&i| pa[0][i] != pb[0][i]).collect();

    println!("\ninvariant under BOTH witnesses     : {}", both.len());
    println!("  ...and IDENTICAL across witnesses: {}  (structure — carries no witness info)",
             both.len() - differing.len());
    println!("  ...and DIFFERENT across witnesses: {}  <-- deterministic witness-dependent channel",
             differing.len());

    // What are those differing bytes? The public inputs legitimately differ between the two
    // witnesses (root, nullifier, output commitments), and they are PUBLIC by design.
    let (ta, pa_pos) = pool_with(a.value, a.blinding, a.sk);
    let (tb, pb_pos) = pool_with(b.value, b.blinding, b.sk);
    let cm = |v: BaseElement, bl: BaseElement, pk: BaseElement| compress2(compress2(v, bl), pk);
    let publics: Vec<(&str, BaseElement)> = vec![
        ("A root", ta.root()), ("B root", tb.root()),
        ("A nf", compress2(a.sk, BaseElement::new(pa_pos as u64))),
        ("B nf", compress2(b.sk, BaseElement::new(pb_pos as u64))),
        ("A cm_out0", cm(e(50), e(777), pk_of(e(0xB0B)))),
        ("A cm_out1", cm(e(47), e(888), pk_of(sk_a))),
        ("B cm_out0", cm(e(61), e(555), pk_of(e(0xC0C)))),
        ("B cm_out1", cm(e(36), e(222), pk_of(sk_b))),
    ];
    // Mark every byte covered by a public value's 8-byte encoding, anywhere it occurs.
    let mut explained = vec![false; len];
    for (_, v) in &publics {
        let needle = v.as_int().to_le_bytes();
        for p in [&pa[0], &pb[0]] {
            for s in 0..p.len().saturating_sub(8).min(len) {
                if p[s..s + 8] == needle { for k in s..(s + 8).min(len) { explained[k] = true; } }
            }
        }
    }
    let unexplained: Vec<usize> = differing.iter().copied().filter(|&i| !explained[i]).collect();
    println!("\n  of those, explained by PUBLIC inputs (root/nf/cm_outs): {}",
             differing.len() - unexplained.len());
    println!("  UNEXPLAINED witness-dependent bytes                    : {}", unexplained.len());

    if unexplained.is_empty() {
        println!("\n✅ No deterministic channel outside the public inputs.");
    } else {
        println!("\n⚠️  {} bytes carry witness-dependent, mask-invariant data.", unexplained.len());
        println!("   first 24 offsets: {:?}", &unexplained[..unexplained.len().min(24)]);
        // Do they cluster? A contiguous run points at one proof section.
        let mut runs = Vec::new();
        let (mut s, mut p) = (unexplained[0], unexplained[0]);
        for &i in &unexplained[1..] {
            if i != p + 1 { runs.push((s, p)); s = i; }
            p = i;
        }
        runs.push((s, p));
        println!("   contiguous runs: {} (largest {} bytes)", runs.len(),
                 runs.iter().map(|(a, b)| b - a + 1).max().unwrap());
        println!("   first runs: {:?}", &runs[..runs.len().min(8)]);
    }

    // Sanity: the two witnesses really do differ, and seeds really do move the proof.
    let d_seed = pa[0].iter().zip(pa[1].iter()).filter(|(x, y)| x != y).count();
    println!("\ncontrol — same witness, different seed : {d_seed}/{} bytes differ ({:.1}%)",
             pa[0].len(), 100.0 * d_seed as f64 / pa[0].len() as f64);
}
