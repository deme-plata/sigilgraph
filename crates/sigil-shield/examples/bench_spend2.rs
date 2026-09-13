//! Where does a phone spend its two minutes? Time the pieces of a 2-input private send
//! against a 14k-note pool, single-threaded, exactly as the Android core does them.
use sigil_shield::note_v1::{from_wire, padding_leaf, to_wire, Note};
use sigil_shield::wallet::{build_spend_2, NoteStore, OwnedNote, ShieldedAccount};
use winterfell::math::{fields::f64::BaseElement, FieldElement};
use std::time::Instant;

fn main() {
    let n_pool: usize = std::env::var("POOL").ok().and_then(|v| v.parse().ok()).unwrap_or(14_348);
    let capacity = 32_768usize;
    let seed = [7u8; 32];
    let account = ShieldedAccount::from_seed(seed);
    // two notes we own, at positions 100 and 200; the rest of the pool is noise
    let mine = [(100usize, 2_000_000_000u64, BaseElement::new(11)), (200usize, 3_000_000_000u64, BaseElement::new(22))];
    let t0 = Instant::now();
    let mut pool: Vec<[u8; 32]> = (0..n_pool).map(|i| to_wire(BaseElement::new(1_000_003 * (i as u64 + 1)))).collect();
    for (pos, v, b) in mine {
        let note = Note { value: BaseElement::new(v), blinding: b, spend_key: account.spend_key() };
        pool[pos] = to_wire(note.commitment());
    }
    println!("pool build ({n_pool} leaves): {:?}", t0.elapsed());
    let mut store = NoteStore::with_next_index(1 << 31);
    for (pos, v, b) in mine {
        store.notes.push(OwnedNote { index: None, value: v, blinding: b, position: Some(pos as u64), spent: false, memo: None });
    }
    let fee = 100_000u64;
    let amount = 1_900_000_000u64;
    let change = 2_000_000_000 + 3_000_000_000 - fee - amount;
    let outs = [(amount, BaseElement::new(999)), (change, account.public_key())];
    // pad to capacity like the core's pool_from_json does
    let t1 = Instant::now();
    let mut padded = pool.clone();
    for i in padded.len()..capacity { padded.push(to_wire(padding_leaf(i as u64))); }
    println!("pad to {capacity}: {:?}", t1.elapsed());
    let t2 = Instant::now();
    let bundle = build_spend_2(&account, &mut store, &padded, [0, 1], fee, &outs).expect("spend");
    println!("build_spend_2 (tree + 2 paths + STARK): {:?}  proof bytes {}", t2.elapsed(), bundle.proof.len());
    let _ = from_wire(&padded[0]);
    let _ = BaseElement::ZERO;
}
