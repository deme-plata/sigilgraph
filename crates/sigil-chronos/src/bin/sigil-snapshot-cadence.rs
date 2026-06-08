use sigil_chronos::snapshot_cadence::run_snapshot_cadence;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let blocks: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(12_000);
    let rate: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(2500);
    println!("🛰 {}", run_snapshot_cadence(blocks, rate).summary());
}
