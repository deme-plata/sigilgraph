//! Is v5's real region byte-identical to v4's trace? If not, the port broke something and
//! the verification failure is mine, not winterfell's.
use sigil_shield::membership::CompressTree;
use sigil_shield::mimc::{compress2, mimc_options};
use sigil_shield::spend_full_v4 as v4;
use sigil_shield::spend_full_v5 as v5;
use winterfell::math::{fields::f64::BaseElement, StarkField};
use winterfell::Trace;

fn e(v: u64) -> BaseElement { BaseElement::new(v) }
fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, e(v4::PK_DOMAIN)) }
fn leaf(v: BaseElement, b: BaseElement, sk: BaseElement) -> BaseElement {
    compress2(compress2(v, b), pk_of(sk))
}

fn main() {
    let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
    let bob = pk_of(e(0xB0B));
    let me = pk_of(sk);
    let mut leaves: Vec<BaseElement> =
        (0..7).map(|i| sigil_shield::note_v1::padding_leaf(i as u64)).collect();
    leaves.insert(3, leaf(value, blinding, sk));
    let tree = CompressTree::new(leaves);
    let path = tree.path(3);
    let outs = [(e(50), e(777), bob), (e(47), e(888), me)];

    let t4 = v4::build_spend_full_v4_trace(value, blinding, sk, e(3), &outs, &path);
    let t5 = v5::build_spend_full_v5_trace_seeded(
        value, blinding, sk, e(3), &outs, &path, &v5::v5_options(), [0u8; 32]);

    println!("v4: {} rows x {} cols", t4.main_segment().num_rows(), t4.main_segment().num_cols());
    println!("v5: {} rows x {} cols", t5.main_segment().num_rows(), t5.main_segment().num_cols());

    let real = t4.main_segment().num_rows();
    let mut diffs = 0usize;
    let mut first_diff = None;
    for r in 0..real {
        for c in 0..t4.main_segment().num_cols() {
            let (a, b) = (t4.main_segment().get(c, r), t5.main_segment().get(c, r));
            if a != b {
                diffs += 1;
                if first_diff.is_none() { first_diff = Some((r, c, a.as_int(), b.as_int())); }
            }
        }
    }
    println!("\nreal-region cells compared: {}", real * t4.main_segment().num_cols());
    println!("differing cells: {diffs}");
    match first_diff {
        None => println!("✅ v5's real region is IDENTICAL to v4's — the port is clean."),
        Some((r, c, a, b)) => println!("❌ first difference at row {r}, col {c}: v4={a} v5={b}"),
    }
}
