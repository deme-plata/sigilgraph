//! sigil-court demo — `sigil-court [--json] [--packet <file>] [--csv <file>]`
//!
//! Runs the whole Supreme Court story on a synthetic three-block chain and prints every act with
//! the live court_root: seat the bench → educate + promote a clerk → file SKAT v. wallet → admit
//! proven evidence (and refuse unproven) → rule → refuse a spend-key request (Art. III) → order a
//! viewing-key tax disclosure → export + verify the sealed packet → appeal en banc → commit the
//! court_root into a SigilState's contract_state_root.

use std::collections::BTreeMap;

use sigil_court::chain::{commit_court_root, read_court_root};
use sigil_court::*;
use sigil_events::{prove_inclusion, SigilEvent};
use sigil_state::{SigilState, WalletId, NATIVE};
use sigil_university::UniversityRegistry;

fn w(b: u8) -> WalletId { [b; 32] }
fn short(h: &[u8]) -> String { hex::encode(&h[..6]) }

struct Story {
    json: bool,
    steps: Vec<serde_json::Value>,
}

impl Story {
    fn step(&mut self, c: &SupremeCourt, kind: &str, msg: &str) {
        if self.json {
            self.steps.push(serde_json::json!({ "label": msg, "kind": kind, "court_root": c.court_root_hex(), "docket": c.docket().len(), "docket_head": hex::encode(c.docket().head()) }));
        } else {
            println!("  {:<62} root {}…  docket {:>2}", msg, &c.court_root_hex()[..12], c.docket().len());
        }
    }
    fn refusal(&mut self, c: &SupremeCourt, act: &str, err: &str) {
        if self.json {
            self.steps.push(serde_json::json!({ "label": act, "kind": "refusal", "err": err, "court_root": c.court_root_hex(), "docket": c.docket().len() }));
        } else {
            println!("  ✗ {:<60} → {}", act, err);
        }
    }
    fn say(&self, s: &str) { if !self.json { println!("{s}"); } }
}

fn chain() -> Vec<BlockEvents> {
    let viktor = w(0x77);
    let shop = w(0x55);
    let treasury = [0x57; 32]; // sigil_bank::welfare::WELFARE_WALLET
    vec![
        BlockEvents::seal(2_165_800, vec![
            SigilEvent::MintReward { miner: viktor, height: 2_165_800, amount: 3_900_000_000 },
            SigilEvent::Send { from: viktor, to: shop, amount: 12_500_000_000, token: NATIVE, fee: 100_000 },
            SigilEvent::Send { from: w(0x11), to: w(0x22), amount: 7, token: NATIVE, fee: 1 },
        ]),
        BlockEvents::seal(2_165_801, vec![
            SigilEvent::WelfareClaimed { citizen: viktor, amount: 10_000_000_000 },
            SigilEvent::BankExecuted { id: "bank-42".into(), from: treasury, to: viktor, token: NATIVE, amount: 5_000_000_000 },
            SigilEvent::Receive { from: w(0x99), to: viktor, amount: 2_000_000_000, token: NATIVE },
        ]),
        BlockEvents::seal(2_165_802, vec![
            SigilEvent::SwapExecuted { pool: [3; 32], in_token: NATIVE, in_amt: 10, out_token: [4; 32], out_amt: 20, slippage_bps: 5, fee_paid: 1 },
            SigilEvent::MandateCreated { id: "m-1".into(), agent: w(0x42), max_amount: 1_000, purpose: "agent groceries".into(), created_ts: 1_000, expires_ts: 9_000 },
        ]),
    ]
}


fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json = args.iter().any(|a| a == "--json");
    let packet_out = args.iter().position(|a| a == "--packet").and_then(|i| args.get(i + 1).cloned());
    let csv_out = args.iter().position(|a| a == "--csv").and_then(|i| args.get(i + 1).cloned());
    if let Some(i) = args.iter().position(|a| a == "--science") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "sigil-court-science.json".into());
        eprintln!("measuring… (exhaustive vote search + scaling sweep, ~1 min)");
        let r = sigil_court::science::run();
        let js = serde_json::to_string_pretty(&r).expect("serialize report");
        std::fs::write(&out, &js).expect("write report");
        eprintln!("science → {out} ({} bytes)", js.len());
        println!("{js}");
        return;
    }
    let mut s = Story { json, steps: Vec::new() };

    let (rocky, codex, adrian, grok, pedre, viktor, skat) = (w(1), w(2), w(3), w(4), w(9), w(0x77), w(0x5A));
    let mut c = SupremeCourt::from_seed(blake3::hash(b"sigil-court/v3/clerk-of-court/demo-seed").as_bytes());
    let ch = chain();
    let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();

    s.say("\n  ⚖️  SIGIL NATION — SUPREME COURT v3 · code is law\n");
    s.say(&format!("  constitution v{} · {} Articles · hash {}…", CONSTITUTION_VERSION, Article::ALL.len(), &constitution_hash_hex()[..16]));
    s.say(&format!("  court seal: SQIsign L5 · pubkey {}…\n", short(c.public_key())));
    s.step(&c, "genesis", "genesis · empty court");

    s.say("\n  🏛  THE BENCH (appointments)");
    c.appoint(rocky, Rank::ChiefJustice, 1).unwrap();  s.step(&c, "bench", "appoint rocky · Chief Justice");
    c.appoint(codex, Rank::Justice, 1).unwrap();        s.step(&c, "bench", "appoint codex · Justice");
    c.appoint(adrian, Rank::Justice, 1).unwrap();       s.step(&c, "bench", "appoint adrian · Justice");
    c.appoint(grok, Rank::Magistrate, 1).unwrap();      s.step(&c, "bench", "appoint grok · Magistrate");
    c.appoint(pedre, Rank::Clerk, 1).unwrap();          s.step(&c, "bench", "appoint pedre · Clerk");

    s.say("\n  🎓 EDUCATION + PROMOTION (Art. VIII — exams, deeds, quorum; never solo)");
    let p = c.sit_exam(pedre, Course::ConstitutionalCode, 6_400, 2).unwrap();
    s.step(&c, "exam", &format!("pedre sits Constitutional Code · 64.00% → {}", if p { "PASS" } else { "FAIL (pass mark 70%)" }));
    let p = c.sit_exam(pedre, Course::ConstitutionalCode, 8_200, 3).unwrap();
    s.step(&c, "exam", &format!("pedre retakes Constitutional Code · 82.00% → {}", if p { "PASS" } else { "FAIL" }));
    c.sit_exam(pedre, Course::Evidence, 7_500, 4).unwrap();
    s.step(&c, "exam", "pedre sits Evidence · 75.00% → PASS → BAR ADMITTED");
    let mut reg = UniversityRegistry::new();
    reg.mark_graduated(&pedre).unwrap();
    c.admit_graduate(pedre, &reg, 5).unwrap();
    s.step(&c, "credential", "registrar: pedre graduated SIGIL University → credential");
    match c.promote(pedre, Rank::Advocate, &[rocky], 6) {
        Err(e) => s.refusal(&c, "promote pedre → Advocate with ONE vote", &e.to_string()),
        Ok(_) => unreachable!(),
    }
    c.promote(pedre, Rank::Advocate, &[rocky, grok], 6).unwrap();
    s.step(&c, "promotion", "promote pedre → Advocate (2 of 4 Magistrate+ electorate)");
    match c.promote(pedre, Rank::Magistrate, &[rocky, codex], 7) {
        Err(e) => s.refusal(&c, "promote pedre → Magistrate without Disclosure Law", &e.to_string()),
        Ok(_) => unreachable!(),
    }

    s.say("\n  📂 THE CASE · SKAT v. wallet 77 (Art. IV Minimization)");
    let case = c.file_case(skat, Some(viktor), Article::Minimization, "annual statement 2026: what may SKAT see?", 10);
    s.step(&c, "case", &format!("file case {}… plaintiff DK/SKAT, defendant wallet 77", short(&case)));
    let pr = prove_inclusion(&ch[0].events, 1).unwrap();
    c.admit_evidence(case, ch[0].events[1].clone(), ch[0].height, 1, pr.clone(), ch[0].event_log_root, 11).unwrap();
    s.step(&c, "evidence", "admit evidence: Send@2165800#1 with inclusion proof ✓ (Art. V)");
    match c.admit_evidence(case, ch[0].events[0].clone(), ch[0].height, 1, pr, ch[0].event_log_root, 11) {
        Err(e) => s.refusal(&c, "admit MintReward under the Send's proof", &e.to_string()),
        Ok(_) => unreachable!(),
    }
    let panel = c.hear(case, 3, 12).unwrap();
    s.step(&c, "hearing", &format!("hearing · panel [{}] (deterministic, Magistrate+)", panel.iter().map(|p| short(p)).collect::<Vec<_>>().join(" ")));
    let votes: Vec<(WalletId, bool)> = panel.iter().map(|p| (*p, true)).collect();
    let ruling = c.rule(case, Verdict::Upheld, "SKAT may see amounts, fees and direction; counterparties are pseudonymized; spend keys never.", &votes, vec![], 13).unwrap();
    s.step(&c, "ruling", &format!("ruling {}… UPHELD 3-0 → precedent set (Art. VII)", short(&ruling)));

    s.say("\n  🌍 DISCLOSURE TO A FOREIGN AUTHORITY (Art. II / III / IV / V / XI)");
    let mut req = DisclosureRequest {
        jurisdiction: Jurisdiction::new("DK", "SKAT"),
        purpose: Purpose::Tax,
        scope: tax_scope(viktor, 2_165_800, 2_165_802),
        key_kind: Some(KeyKind::Spend),
        reason: "årsopgørelse 2026".into(),
        requested_ts: 1_757_500_000,
        ttl_secs: 30 * 24 * 3600,
    };
    match c.order_disclosure(req.clone(), Some(case), &[], 14, 1_757_500_000) {
        Err(e) => s.refusal(&c, "SKAT requests the SPEND key", &e.to_string()),
        Ok(_) => unreachable!(),
    }
    req.key_kind = Some(KeyKind::Viewing);
    let oid = disclosure::order_id(&req, 14, 1_757_500_000);
    let opanel = c.bench().select_panel(&oid, 3).unwrap();
    let ovotes: Vec<(WalletId, bool)> = opanel.iter().enumerate().map(|(i, p)| (*p, i != 0)).collect();
    let order = c.order_disclosure(req, Some(case), &ovotes, 14, 1_757_500_000).unwrap();
    s.step(&c, "order", &format!("order {}… Tax · DK/SKAT · VIEWING key · panel 2-1 · expires +30d", short(&order)));
    let viewing_key = *blake3::hash(b"demo viewing key (sk_enc) for wallet 77").as_bytes();
    let packet = c.export_disclosure(order, &ch, &[(viktor, viewing_key)], 15, 1_757_500_100).unwrap();
    s.step(&c, "export", &format!("export packet · {} records · {} redacted fields · 1 viewing grant", packet.records.len(), packet.summary.redacted_fields));
    let v = verify_packet(&packet, &roots, c.public_key(), 1_757_500_200).unwrap();
    s.step(&c, "verify", &format!("SKAT verifies: {} proven under block roots · {} attested by seal", v.records_proven, v.fields_attested));
    let mut tampered = packet.clone();
    tampered.records[0].fields.insert("amount".into(), "1".into());
    match verify_packet(&tampered, &roots, c.public_key(), 1_757_500_200) {
        Err(e) => s.refusal(&c, "SKAT verifies a packet with one amount changed", &e.to_string()),
        Ok(_) => unreachable!(),
    }
    match verify_packet(&packet, &roots, c.public_key(), 1_757_500_000 + 31 * 24 * 3600) {
        Err(e) => s.refusal(&c, "SKAT verifies the same packet 31 days later", &e.to_string()),
        Ok(_) => unreachable!(),
    }

    s.say("\n  🏦 SIGIL BANK — the books, institution-wide (Purpose::Audit)");
    let breq = DisclosureRequest { jurisdiction: Jurisdiction::new("SIGIL", "BANK"), purpose: Purpose::Audit, scope: bank_scope(2_165_800, 2_165_802), key_kind: None, reason: "quarterly books".into(), requested_ts: 1_757_500_000, ttl_secs: 86_400 };
    let bid = disclosure::order_id(&breq, 16, 1_757_500_000);
    let bpanel = c.bench().select_panel(&bid, 3).unwrap();
    let bvotes: Vec<(WalletId, bool)> = bpanel.iter().map(|p| (*p, true)).collect();
    let border = c.order_disclosure(breq, None, &bvotes, 16, 1_757_500_000).unwrap();
    let bank = c.export_disclosure(border, &ch, &[], 17, 1_757_500_100).unwrap();
    s.step(&c, "bank", &format!("bank books packet · {} records · kinds {:?}", bank.records.len(), bank.summary.per_kind.keys().collect::<Vec<_>>()));

    s.say("\n  ⚖️  APPEAL EN BANC (Art. VI / VII)");
    c.appeal(case, viktor, 18).unwrap();
    s.step(&c, "appeal", "wallet 77 appeals the ruling (once)");
    let verdict = c.decide_appeal(case, &[(rocky, false), (codex, true), (adrian, false)], "the panel got it right", 19).unwrap();
    s.step(&c, "en-banc", &format!("en banc 3 Justices · 1-2 for reversal → {:?} · precedent stands", verdict));

    s.say("\n  ⛓  COMMIT court_root INTO THE CHAIN (contract_state_root, like the Nation)");
    let mut state = SigilState::new();
    let roots_after = commit_court_root(&mut state, &c, 20).unwrap();
    s.step(&c, "chain", &format!("contract_state_root {}… carries court_root {}…", short(&roots_after.contract_state_root), short(&read_court_root(&state))));
    c.docket().verify_chain().unwrap();

    if let Some(path) = packet_out { std::fs::write(&path, packet.to_json()).expect("write packet"); s.say(&format!("\n  packet → {path}")); }
    if let Some(path) = csv_out { std::fs::write(&path, packet.tax_csv()).expect("write csv"); s.say(&format!("  csv    → {path}")); }

    if json {
        println!("{}", serde_json::json!({
            "title": "SIGIL NATION — SUPREME COURT v3",
            "constitution_version": CONSTITUTION_VERSION,
            "constitution_hash": constitution_hash_hex(),
            "court_pubkey": hex::encode(c.public_key()),
            "court_root": c.court_root_hex(),
            "docket_head": hex::encode(c.docket().head()),
            "docket_root": hex::encode(c.docket().root()),
            "docket_histogram": c.docket().histogram(),
            "live_precedents": c.register().live_precedents().len(),
            "steps": s.steps,
            "tax_packet": { "records": packet.records.len(), "summary": packet.summary, "seal": packet.seal.body, "csv": packet.tax_csv() },
            "bank_packet": { "records": bank.records.len(), "summary": bank.summary },
        }));
    } else {
        println!("\n  📊 TAX PACKET SUMMARY (what SKAT receives)");
        for (tok, t) in &packet.summary.per_token {
            println!("     token {}…  mined {}  received {}  sent {}  fees {}  welfare {}  bank_in {}", &tok[..8], t.mined, t.received, t.sent, t.fees, t.welfare, t.bank_in);
        }
        println!("\n{}", packet.tax_csv().lines().map(|l| format!("     {l}")).collect::<Vec<_>>().join("\n"));
        println!("\n  docket: {} entries · head {}… · {:?}", c.docket().len(), &hex::encode(c.docket().head())[..12], c.docket().histogram());
        println!("  court_root {}\n", c.court_root_hex());
    }
}
