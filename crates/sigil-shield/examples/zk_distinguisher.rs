//! Statistical indistinguishability: can an observer recover a SECRET from the proof?
//!
//! # Why the test is shaped this way
//!
//! The obvious test — two witnesses with identical public inputs, are their proofs
//! distinguishable? — has no valid input pair in this scheme. Every hidden value is bound
//! to a public commitment (root pins the input note, nullifier pins the spend key, cm_outs
//! pin each output), so the publics DETERMINE the witness. Against an unbounded adversary
//! there is nothing to hide: it inverts the commitments from public data alone, proof or
//! no proof. Information-theoretic ZK is therefore impossible here by construction, and a
//! test that assumes it is testing a vacuous property.
//!
//! What we actually need is COMPUTATIONAL zero-knowledge: the proof must not make recovery
//! easier than the public data already does. That IS testable — as a recovery game.
//!
//! # The game
//!
//! Pick a secret with a small, guessable domain: the output amount. Generate many spends
//! whose hidden output amount is one of K classes, with everything else (blindings,
//! recipient keys, mask seeds) freshly random each time. Hand a distinguisher only the
//! proof bytes. If it can name the amount better than 1/K, the proof leaks it.
//!
//!   v4 is the POSITIVE CONTROL — it prints the amount 85 times, so recovery must be ~100%.
//!       If the harness cannot detect v4's known leak, it cannot be trusted on v5.
//!   v5 is the TEST — recovery should sit at chance.
//!
//! Emits a CSV of (version, label, features) for the analysis pass.

use sigil_shield::membership::CompressTree;
use sigil_shield::mimc::{compress2, mimc_options};
use sigil_shield::spend_full_v4 as v4;
use sigil_shield::spend_full_v5 as v5;
use winterfell::math::{fields::f64::BaseElement, StarkField};
use winterfell::Prover;

fn e(v: u64) -> BaseElement { BaseElement::new(v) }
fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, e(v4::PK_DOMAIN)) }
fn leaf(v: BaseElement, b: BaseElement, sk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk_of(sk))
}

/// Cheap deterministic PRNG so the whole run is reproducible from one seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0
    }
}

/// AMOUNT CLASSES — the secret the distinguisher must recover.
const AMOUNTS: [u64; 10] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
/// Proofs per class, per version.
const PER_CLASS: usize = 150;

fn main() {
    let mut rng = Rng(0x5161_5A5A_1234_9E37);
    let mut out = String::from("version,label,amount");
    for i in 0..256 { out.push_str(&format!(",h{i}")); }
    // A direct detector for the v4 failure mode: how often does each candidate amount
    // appear verbatim? This is the feature that must light up for v4 and stay dark for v5.
    for a in AMOUNTS { out.push_str(&format!(",hit{a}")); }
    out.push('\n');

    let o4 = mimc_options();
    let o5 = v5::v5_options();

    for (vi, version) in ["v4", "v5"].iter().enumerate() {
        for (label, &amt) in AMOUNTS.iter().enumerate() {
            for _ in 0..PER_CLASS {
                // Everything except the amount is re-randomised every proof, so the only
                // signal a classifier could latch onto is the amount itself.
                let value = e(200);
                let fee = e(3);
                let a0 = e(amt);
                let a1 = e(200 - 3 - amt); // conservation: fee + a0 + a1 == value
                let blinding = e(rng.next() >> 8);
                let sk = e(rng.next() >> 8);
                let b0 = e(rng.next() >> 8);
                let b1 = e(rng.next() >> 8);
                let rcpt = pk_of(e(rng.next() >> 8));
                let me = pk_of(sk);
                let outs = [(a0, b0, rcpt), (a1, b1, me)];

                let mut lv: Vec<BaseElement> =
                    (0..7).map(|i| sigil_shield::note_v1::padding_leaf(i as u64)).collect();
                lv.insert(3, leaf(value, blinding, sk));
                let tree = CompressTree::new(lv);
                let path = tree.path(3);

                let bytes = if vi == 0 {
                    let tr = v4::build_spend_full_v4_trace(value, blinding, sk, fee, &outs, &path);
                    v4::SpendFullV4Prover::new(o4.clone()).prove(tr).expect("v4").to_bytes()
                } else {
                    let mut seed = [0u8; 32];
                    for s in seed.iter_mut() { *s = (rng.next() & 0xff) as u8; }
                    let tr = v5::build_spend_full_v5_trace_seeded(
                        value, blinding, sk, fee, &outs, &path, &o5, seed);
                    v5::SpendFullV5Prover::new(o5.clone()).prove(tr).expect("v5").to_bytes()
                };

                let mut hist = [0u32; 256];
                for b in &bytes { hist[*b as usize] += 1; }
                out.push_str(version);
                out.push_str(&format!(",{label},{amt}"));
                for h in hist { out.push_str(&format!(",{h}")); }
                for cand in AMOUNTS {
                    let needle = BaseElement::new(cand).as_int().to_le_bytes();
                    let n = bytes.windows(8).filter(|w| *w == needle).count();
                    out.push_str(&format!(",{n}"));
                }
                out.push('\n');
            }
            eprintln!("{version} amount={amt} done");
        }
    }
    std::fs::write("/home/storage/zk-dist.csv", out).expect("write");
    println!("wrote /home/storage/zk-dist.csv  ({} proofs per version)", AMOUNTS.len() * PER_CLASS);
}
