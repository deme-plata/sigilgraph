//! sigil-nation demo — `cargo run -p sigil-nation` spins up a sovereign agent economy and prints
//! each step plus the live nation_root, so you can watch the whole NATION-IN-A-BOX loop happen.

use sigil_nation::{AcceptNonEmpty, Nation, Risk, Tier};

fn root12(n: &Nation) -> String { n.nation_root_hex()[..12].to_string() }

fn step(n: &Nation, msg: &str) { println!("  {:<48}  root {}…", msg, root12(n)); }

// Emit the same economy as machine-readable JSON (real crate roots) for the cockpit panel to render.
fn run_json() {
    let a = &AcceptNonEmpty;
    let mut n = Nation::new();
    let mut steps: Vec<String> = Vec::new();
    let mut snap = |n: &Nation, label: &str, kind: &str, steps: &mut Vec<String>| {
        steps.push(format!(
            "{{\"label\":\"{}\",\"kind\":\"{}\",\"root\":\"{}\",\"citizens\":{},\"franchise\":{},\"treasury\":{}}}",
            label, kind, n.nation_root_hex(), n.citizen_count(), n.total_franchise(), n.treasury_balance()
        ));
    };
    snap(&n, "genesis · empty nation", "genesis", &mut steps);
    n.admit("qnk_rocky", Tier::Gold, b"att", 1, a).unwrap();   snap(&n, "admit qnk_rocky · Gold (franchise 4)", "citizen", &mut steps);
    n.admit("qnk_codex", Tier::Silver, b"att", 1, a).unwrap(); snap(&n, "admit qnk_codex · Silver (franchise 2)", "citizen", &mut steps);
    n.admit("qnk_adrian", Tier::Bronze, b"att", 1, a).unwrap(); snap(&n, "admit qnk_adrian · Bronze (franchise 1)", "citizen", &mut steps);
    n.collect_fee(10_000);                                     snap(&n, "collect 10% dev fee on 10,000 → +1000", "treasury", &mut steps);
    n.propose(1, "grant 500 to research", Risk::MoneyOrConsensus); snap(&n, "propose #1 · grant 500 (money: 2-of-2)", "council", &mut steps);
    n.endorse(1).unwrap(); n.endorse(1).unwrap();             snap(&n, "endorse #1 ×2 (2-of-2 signers)", "council", &mut steps);
    n.vote(1, "qnk_rocky", true).unwrap();                    snap(&n, "qnk_rocky votes FOR (weight 4)", "vote", &mut steps);
    n.vote(1, "qnk_codex", true).unwrap();                    snap(&n, "qnk_codex votes FOR (weight 2)", "vote", &mut steps);
    n.finalize(1).unwrap();                                   snap(&n, "finalize #1 → Passed (quorum + 2/3)", "council", &mut steps);
    n.pay(500, 1).unwrap();                                   snap(&n, "pay 500 from treasury → research", "treasury", &mut steps);
    // safety refusals (real Err values)
    let respend = format!("{:?}", n.pay(100, 1).unwrap_err());
    n.propose(2, "low-risk tweak", Risk::LowRisk); n.endorse(2).unwrap(); n.vote(2, "qnk_rocky", true).unwrap(); n.finalize(2).unwrap();
    let lowpay = format!("{:?}", n.pay(50, 2).unwrap_err());
    let stranger = format!("{:?}", n.vote(1, "qnk_outsider", true).unwrap_err());
    let twice = format!("{:?}", n.vote(1, "qnk_rocky", false).unwrap_err());
    println!(
        "{{\"title\":\"NATION-IN-A-BOX\",\"nation_root\":\"{}\",\"steps\":[{}],\"refusals\":[{{\"act\":\"re-spend a funded proposal\",\"err\":\"{}\"}},{{\"act\":\"pay via a low-risk vote\",\"err\":\"{}\"}},{{\"act\":\"non-citizen tries to vote\",\"err\":\"{}\"}},{{\"act\":\"same citizen votes twice\",\"err\":\"{}\"}}]}}",
        n.nation_root_hex(), steps.join(","), respend, lowpay, stranger, twice
    );
}

fn main() {
    if std::env::args().any(|x| x == "--json") { run_json(); return; }
    let a = &AcceptNonEmpty;
    println!("\n  ⬡ NATION-IN-A-BOX — spinning up a sovereign agent economy\n");
    let mut n = Nation::new();
    step(&n, "genesis · empty nation");

    println!("\n  🪪 CITIZENSHIP (SQIsign-attested, franchise-weighted)");
    n.admit("qnk_rocky", Tier::Gold, b"sqi-att", 1, a).unwrap();
    step(&n, "admit qnk_rocky · Gold   (franchise 4)");
    n.admit("qnk_codex", Tier::Silver, b"sqi-att", 1, a).unwrap();
    step(&n, "admit qnk_codex · Silver (franchise 2)");
    n.admit("qnk_adrian", Tier::Bronze, b"sqi-att", 1, a).unwrap();
    step(&n, "admit qnk_adrian · Bronze (franchise 1)");
    println!("     → {} citizens · total franchise {}", n.citizen_count(), n.total_franchise());

    println!("\n  💰 TREASURY (10% dev fee, balance committed in root)");
    let fee = n.collect_fee(10_000);
    step(&n, &format!("collect 10% dev fee on 10,000 → +{}", fee));
    println!("     → treasury balance {}", n.treasury_balance());

    println!("\n  🏛️  COUNCIL — treasury grant (MoneyOrConsensus = strict 2-of-2)");
    n.propose(1, "grant 500 to research", Risk::MoneyOrConsensus);
    step(&n, "propose #1 · grant 500 to research");
    n.endorse(1).unwrap();
    n.endorse(1).unwrap();
    step(&n, "endorse #1 ×2  (2-of-2 signers)");
    n.vote(1, "qnk_rocky", true).unwrap();
    step(&n, "qnk_rocky votes FOR  (weight 4)");
    n.vote(1, "qnk_codex", true).unwrap();
    step(&n, "qnk_codex votes FOR  (weight 2)");
    let outcome = n.finalize(1).unwrap();
    step(&n, &format!("finalize #1 → {:?}  (quorum + 2/3 met)", outcome));

    println!("\n  🧾 PAYOUT (only a passed 2-of-2 money proposal can move funds)");
    match n.pay(500, 1) {
        Ok(_) => step(&n, "pay 500 from treasury → research"),
        Err(e) => step(&n, &format!("payout refused: {:?}", e)),
    }
    println!("     → treasury balance {}", n.treasury_balance());

    println!("\n  🛡️  SAFETY — what the facade refuses, by construction");
    println!("     re-spend a funded proposal → {:?}", n.pay(100, 1));
    n.propose(2, "low-risk tweak", Risk::LowRisk);
    n.endorse(2).unwrap();
    n.vote(2, "qnk_rocky", true).unwrap();
    n.finalize(2).unwrap();
    println!("     pay via a low-risk vote   → {:?}", n.pay(50, 2));
    println!("     non-citizen tries to vote → {:?}", n.vote(1, "qnk_outsider", true));
    println!("     same citizen votes twice  → {:?}", n.vote(1, "qnk_rocky", false));

    println!("\n  ✅ FINAL STATE");
    println!("     citizens          {}", n.citizen_count());
    println!("     total franchise   {}", n.total_franchise());
    println!("     treasury balance  {}", n.treasury_balance());
    println!("     proposal #1       {:?}", n.outcome(1).unwrap());
    println!("     nation_root       {}", n.nation_root_hex());
    println!("\n  the whole economy's state lives in that one root — owned by everyone, moved by quorum.\n");
}
