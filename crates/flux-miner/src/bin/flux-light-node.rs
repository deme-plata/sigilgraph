//! flux-light-node — verify a whole chain via ONE folded proof (no block bodies).

use std::time::Instant;
use flux_fold::Ajtai;
use flux_miner::light::{light_verify, prove_chain};

fn main() {
    let econ = std::env::args().any(|a| a == "--econ");
    let (m, n) = (64usize, 256usize);
    let ajtai = Ajtai::from_seed(m, n, &[3u8; 32]);
    println!("\n  flux-light-node — fold the chain, verify it in one shot (v0.3, flux-fold)\n");
    println!("  Ajtai {m}x{n} (transparent, post-quantum) · fold proof is constant {} B\n", (m + n) * 8 + 8);

    println!("  {:>9}  {:>10}  {:>12}  {:>13}  {:>12}", "N blocks", "fold B", "light DL", "full DL (est)", "light vfy ms");
    println!("  {}", "-".repeat(64));
    for &nblocks in &[1024usize, 10_000, 100_000] {
        let digests: Vec<[u8; 32]> =
            (0..nblocks as u64).map(|i| *blake3::hash(&i.to_le_bytes()).as_bytes()).collect();
        let proof = prove_chain(&ajtai, &digests);
        let t = Instant::now();
        let ok = light_verify(&ajtai, &proof);
        let vfy_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(ok);
        let light_dl = proof.light_download_bytes(m);
        let full_dl = nblocks * 2048; // est: a full node pulls ~2 KB block bodies each
        println!("  {nblocks:>9}  {:>10}  {:>10} KB  {:>11} KB  {vfy_ms:>12.2}",
            proof.fold.size_bytes(), light_dl / 1024, full_dl / 1024);
    }

    println!("\n  ── the lightweight-node win ──");
    println!("    A full node re-verifies N blocks; the light node runs ONE fold-verify and");
    println!("    accepts the whole chain — no block bodies, no per-block re-checks. The fold");
    println!("    proof is a constant 2.5 KB no matter how long the chain is.");
    println!("    (v0.4: IVC recursion collapses the O(N) commitments to an O(1) tip too.)\n");

    if econ {
        run_econ();
    } else {
        println!("  tip: run with --econ to also earn by ACCUMULATION (arb + Runefelt DCA, no mining).\n");
    }
}

/// The "light ECONOMIC node": it verified the chain above (no storage, no
/// mining) — now it EARNS by accumulation. Node-test finding: mining on
/// commodity/rented GPUs loses ~10×, but tip-verify is ~free, so the light node
/// shouldn't mine — it should run the propose-only arb/DCA/sentiment loop and
/// accumulate BTC. Propose-only; never auto-spends.
fn run_econ() {
    println!("  ══ LIGHT ECONOMIC NODE (--econ) — verify ✓ above · now EARN by accumulation ══");
    println!("  policy: NEVER mine (rented GPU loses ~10×) · NEVER sell · arb + DCA (Carl-Runefelt) · PROPOSE-ONLY\n");
    match flux_market::ticker_24h("BTCUSDT") {
        Ok(t) => {
            let chg = t.change_pct_24h;
            let mult = if chg <= -30.0 { 3.0 } else if chg <= -20.0 { 2.0 } else if chg <= -10.0 { 1.5 } else { 1.0 };
            let dca = if mult > 1.0 { format!("DIP {chg:.1}% → DCA ×{mult} (buy fear)") } else { "DCA on schedule".to_string() };
            let sent = flux_market::news::fetch_news_sentiment("bitcoin")
                .unwrap_or(flux_market::news::Sentiment { score: 0.0, label: "n/a".into(), headlines: 0, bull_hits: 0, bear_hits: 0, sample: vec![] });
            let bets = flux_market::polymarket::scan_arbs(1.0, 40).unwrap_or_default();
            println!("  ₿ ${:.0} ({:+.2}% 24h) · 🌙 {} · 📰 {} ({:+.2}, {}hl) · 📈 polymarket arbs: {}",
                t.last, chg, dca, sent.label, sent.score, sent.headlines, bets.len());
            println!("  → accumulate BTC via sigil-bridge; harvest arb as fuel; hold forever. (no funds moved)\n");
        }
        Err(e) => println!("  econ feed offline ({e}) — needs egress to api.binance.com / news.google.com\n"),
    }
}
