//! TURBO-1 sweep — sync every block, verified, measured.
//!   sigil-turbosync [blocks] [batch]
use sigil_chronos::turbosync::run_turbo_sync;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() >= 2 {
        let blocks: u64 = a[1].parse().unwrap_or(100_000);
        let batch: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(256);
        println!("🛰 {}", run_turbo_sync(blocks, batch).summary());
        return;
    }
    println!("🛰 SIGIL turbo-sync sweep — late-joiner syncs every block, verified\n");
    for (blocks, batch) in [(10_000u64, 256u64), (100_000, 512), (500_000, 1024)] {
        println!("• {}\n", run_turbo_sync(blocks, batch).summary());
    }
}
