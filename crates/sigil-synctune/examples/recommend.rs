//! Print the sweep's recommended sync config vs the conservative baseline.
//! Usage: `cargo run --example recommend [cores]`  (default 48 = epsilon)

use sigil_synctune::{model_eval, recommend_config, KnobSet};

fn main() {
    let cores: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(48);

    let base = KnobSet::baseline();
    let be = model_eval(&base, cores);
    let (cfg, e) = recommend_config(cores);

    println!("== sigil-synctune sweep (cores={cores}, target=100000 blk/s) ==");
    println!("baseline   {base:?}");
    println!("           -> sustained {:>7.0} blk/s, p99 {:>6.1} ms", be.sustained_blk_s, be.p99_ms);
    println!("recommended {cfg:?}");
    println!("           -> sustained {:>7.0} blk/s, p99 {:>6.1} ms", e.sustained_blk_s, e.p99_ms);
    println!("speedup vs baseline: {:.1}x", e.sustained_blk_s / be.sustained_blk_s);
}
