use sigil_chronos::backfill_catchup::{run_catchup, tune, producer_serve_load};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let gap: u64  = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let prod: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(1200);
    for n in [1u64, 3] {
        println!("── {} node(s), gap {}, production {} blk/s ──", n, gap, prod);
        println!("  SHIPPED (drain 5000ms): {}", run_catchup(gap, prod, n, 512, 250, 5000).summary());
        match tune(gap, prod, n) {
            Some(r) => println!("  TUNED (cheapest that syncs): {}", r.summary()),
            None    => println!("  TUNED: no config in grid syncs — production too hot"),
        }
        let serve=4096u64;
        println!("  PRODUCER SERVE LOAD: gossip {} blk/s (×{} flood) vs request-response {} blk/s — budget ~5000",
            producer_serve_load(serve,n,true), n+1, producer_serve_load(serve,n,false));
    }
}
