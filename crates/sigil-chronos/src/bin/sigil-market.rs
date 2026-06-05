//! Agentic-money DEX market sim — thousands of AI traders, scored by X-Algo +
//! SAP to find the best swap strategy.
//!   sigil-market [agents] [ticks]
use sigil_chronos::market::run_market;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let agents: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let ticks: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(400);
    println!("🏦 SIGIL agentic-money DEX market — {agents} agents × {ticks} ticks\n");
    let r = run_market(agents, ticks, 42);
    println!("{}", r.summary());
}
