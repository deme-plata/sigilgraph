//! THE MEASUREMENT HARNESS — what this court actually does, in numbers.
//!
//! Every figure in the accompanying paper comes out of this module at generation time. Nothing
//! here is a constant lifted from a previous run: each function builds a real court, runs a real
//! export, and reports what happened. Where a claim cannot be measured (a privacy property that is
//! a statement about all possible adversaries, say) the function says so rather than producing a
//! number that looks like proof.
//!
//! Run it with `sigil-court --science <out.json>`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sigil_events::SigilEvent;
use sigil_state::{WalletId, NATIVE};

use crate::bench::{Course, Rank};
use crate::constitution::Article;
use crate::disclosure::{self, BlockEvents, DisclosurePacket, DisclosureRequest, Jurisdiction, KeyKind, Purpose, Scope};
use crate::ruling::Verdict;
use crate::{tax_scope, verify_packet, SupremeCourt};

fn w(b: u8) -> WalletId { [b; 32] }

/// A synthetic but structurally faithful chain: `blocks` blocks, `per_block` events each, drawn
/// from the event kinds a tax authority actually asks about. Deterministic in `seed`.
pub fn synthetic_chain(blocks: u64, per_block: usize, subject: WalletId, seed: u64) -> Vec<BlockEvents> {
    let mut out = Vec::new();
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    let mut next = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
    for h in 0..blocks {
        let mut ev = Vec::with_capacity(per_block);
        for i in 0..per_block {
            let r = next();
            let other = w((r % 200) as u8 + 40);
            // Two thirds of events involve the subject; the rest are the crowd it hides in.
            let involves = r % 3 != 0;
            let amount = (r % 9_000_000_000) as u128 + 1;
            ev.push(match r % 5 {
                0 if involves => SigilEvent::MintReward { miner: subject, height: h, amount },
                0 => SigilEvent::MintReward { miner: other, height: h, amount },
                1 if involves => SigilEvent::Send { from: subject, to: other, amount, token: NATIVE, fee: amount / 1000 },
                1 => SigilEvent::Send { from: other, to: w((r % 37) as u8 + 100), amount, token: NATIVE, fee: 1 },
                2 if involves => SigilEvent::Receive { from: other, to: subject, amount, token: NATIVE },
                2 => SigilEvent::Receive { from: other, to: w((r % 41) as u8 + 100), amount, token: NATIVE },
                3 if involves => SigilEvent::WelfareClaimed { citizen: subject, amount },
                3 => SigilEvent::WelfareClaimed { citizen: other, amount },
                _ if involves => SigilEvent::BankExecuted { id: format!("b{h}-{i}"), from: [0x57; 32], to: subject, token: NATIVE, amount },
                _ => SigilEvent::SwapExecuted { pool: [3; 32], in_token: NATIVE, in_amt: amount, out_token: [4; 32], out_amt: amount * 2, slippage_bps: (r % 100) as u16, fee_paid: amount / 500 },
            });
        }
        out.push(BlockEvents::seal(h, ev));
    }
    out
}

/// A court with a seated bench, ready to order disclosure.
pub fn seated_court() -> SupremeCourt {
    let mut c = SupremeCourt::from_seed(blake3::hash(b"sigil-court/science/seed").as_bytes());
    c.appoint(w(1), Rank::ChiefJustice, 1).unwrap();
    c.appoint(w(2), Rank::Justice, 1).unwrap();
    c.appoint(w(3), Rank::Justice, 1).unwrap();
    c.appoint(w(4), Rank::Magistrate, 1).unwrap();
    c.appoint(w(5), Rank::Magistrate, 1).unwrap();
    c
}

fn request(purpose: Purpose, scope: Scope, key: Option<KeyKind>, ts: u64) -> DisclosureRequest {
    DisclosureRequest { jurisdiction: Jurisdiction::new("DK", "SKAT"), purpose, scope, key_kind: key, reason: "measurement".into(), requested_ts: ts, ttl_secs: 86_400 }
}

/// Issue an order through the real vote path and export it.
pub fn ordered_export(c: &mut SupremeCourt, req: DisclosureRequest, blocks: &[BlockEvents], height: u64, ts: u64) -> DisclosurePacket {
    let id = disclosure::order_id(&req, height, ts);
    let panel = c.bench().select_panel(&id, 3).unwrap();
    let votes: Vec<(WalletId, bool)> = panel.iter().map(|p| (*p, true)).collect();
    let order = c.order_disclosure(req, None, &votes, height, ts).unwrap();
    c.export_disclosure(order, blocks, &[], height + 1, ts + 1).unwrap()
}

// ───────────────────────────── M1 · root avalanche ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Avalanche {
    pub trials: u32,
    pub bits: u32,
    pub mean_flipped: f64,
    pub min_flipped: u32,
    pub max_flipped: u32,
    pub collisions: u32,
}

/// Perturb one act of the court by the smallest amount the type system allows (one basis point on
/// one exam) and measure how much of the 256-bit `court_root` moves. A root that hides small
/// divergences is the failure mode SIGIL exists to rule out; the honest test is the distance
/// distribution, not one example.
pub fn avalanche(trials: u32) -> Avalanche {
    let base_of = |score: u16| {
        let mut c = seated_court();
        c.appoint(w(9), Rank::Clerk, 2).unwrap();
        c.sit_exam(w(9), Course::ConstitutionalCode, score, 3).unwrap();
        c.court_root()
    };
    let mut roots = BTreeSet::new();
    let (mut sum, mut min, mut max) = (0u64, 256u32, 0u32);
    let base = base_of(7_000);
    roots.insert(base);
    for i in 1..=trials {
        let r = base_of(7_000 + i as u16);
        roots.insert(r);
        let d: u32 = base.iter().zip(r.iter()).map(|(a, b)| (a ^ b).count_ones()).sum();
        sum += d as u64;
        min = min.min(d);
        max = max.max(d);
    }
    Avalanche {
        trials,
        bits: 256,
        mean_flipped: sum as f64 / trials as f64,
        min_flipped: min,
        max_flipped: max,
        collisions: (trials + 1) - roots.len() as u32,
    }
}

// ───────────────────────────── M2 · minimization ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinimizationRow {
    pub purpose: String,
    pub in_scope_events: u32,
    pub records: u32,
    /// Bytes of the full canonical events that fall in scope.
    pub full_bytes: u64,
    /// Bytes of cleartext actually handed over (field names + values).
    pub cleartext_bytes: u64,
    pub disclosed_ratio: f64,
    pub redacted_fields: u32,
    /// Distinct wallets, other than the subject, the recipient can read in clear.
    pub counterparties_learned: u32,
    /// Distinct wallets that were in the underlying events at all.
    pub counterparties_present: u32,
    pub packet_bytes: u64,
    pub carries_full_events: bool,
}

fn cleartext_bytes(p: &DisclosurePacket) -> u64 {
    p.records.iter().flat_map(|r| r.fields.iter()).map(|(k, v)| (k.len() + v.len()) as u64).sum()
}

/// Field names that carry a WALLET. Scanning for "any 64-hex value" instead would count the
/// `token` id — 64 hex characters of zeros for native SIGIL — as a leaked counterparty, and report
/// a privacy failure where there is none. Measure the thing you mean.
const WALLET_FIELDS: [&str; 11] = ["from", "to", "miner", "citizen", "proposer", "approver", "agent", "wallet", "recipient", "conferred_by", "authority"];

fn wallets_in_clear(p: &DisclosurePacket, subject: &WalletId) -> u32 {
    let sub = hex::encode(subject);
    let mut s = BTreeSet::new();
    for r in &p.records {
        for name in WALLET_FIELDS {
            if let Some(v) = r.fields.get(name) {
                if v.len() == 64 && *v != sub && hex::decode(v).is_ok() {
                    s.insert(v.clone());
                }
            }
        }
    }
    s.len() as u32
}

/// How much less than everything each purpose hands over. Art. IV is a claim about ratios, so this
/// measures the ratio for every purpose on one identical chain.
pub fn minimization(blocks: &[BlockEvents], subject: WalletId) -> Vec<MinimizationRow> {
    let from = blocks.first().map(|b| b.height).unwrap_or(0);
    let to = blocks.last().map(|b| b.height).unwrap_or(0);
    let scope = tax_scope(subject, from, to);
    let mut out = Vec::new();
    for (i, purpose) in [Purpose::Tax, Purpose::Audit, Purpose::Aml, Purpose::CourtOrder].into_iter().enumerate() {
        let mut c = seated_court();
        // A CourtOrder needs a decided case, so give it one through the real path.
        let case = c.file_case(w(30), Some(subject), Article::Minimization, "measurement", 1);
        let panel = c.hear(case, 3, 2).unwrap();
        let votes: Vec<(WalletId, bool)> = panel.iter().map(|p| (*p, true)).collect();
        c.rule(case, Verdict::Upheld, "h", &votes, vec![], 3).unwrap();
        let req = request(purpose, scope.clone(), None, 1_000 + i as u64);
        let id = disclosure::order_id(&req, 4, 1_000 + i as u64);
        let opanel = c.bench().select_panel(&id, 3).unwrap();
        let ovotes: Vec<(WalletId, bool)> = opanel.iter().map(|p| (*p, true)).collect();
        let order = c.order_disclosure(req, Some(case), &ovotes, 4, 1_000 + i as u64).unwrap();
        let p = c.export_disclosure(order, blocks, &[], 5, 1_001 + i as u64).unwrap();

        // The in-scope ground truth the court is minimizing FROM.
        let mut full_bytes = 0u64;
        let mut in_scope = 0u32;
        let mut present = BTreeSet::new();
        for b in blocks {
            for e in &b.events {
                if scope.kinds.contains(&e.tag()) && disclosure::involvement(e, &subject).is_some() {
                    in_scope += 1;
                    full_bytes += e.encode().len() as u64;
                    for f in event_wallets(e) { if f != subject { present.insert(f); } }
                }
            }
        }
        let ct = cleartext_bytes(&p);
        out.push(MinimizationRow {
            purpose: format!("{purpose:?}"),
            in_scope_events: in_scope,
            records: p.records.len() as u32,
            full_bytes,
            cleartext_bytes: ct,
            disclosed_ratio: if full_bytes == 0 { 0.0 } else { ct as f64 / full_bytes as f64 },
            redacted_fields: p.summary.redacted_fields,
            counterparties_learned: wallets_in_clear(&p, &subject),
            counterparties_present: present.len() as u32,
            packet_bytes: p.to_json().len() as u64,
            carries_full_events: p.records.first().map(|r| r.full_event.is_some()).unwrap_or(false),
        });
    }
    out
}

fn event_wallets(e: &SigilEvent) -> Vec<WalletId> {
    match e {
        SigilEvent::Send { from, to, .. } | SigilEvent::Receive { from, to, .. } => vec![*from, *to],
        SigilEvent::BankExecuted { from, to, .. } => vec![*from, *to],
        SigilEvent::MintReward { miner, .. } => vec![*miner],
        SigilEvent::WelfareClaimed { citizen, .. } => vec![*citizen],
        _ => Vec::new(),
    }
}

// ───────────────────────────── M3 · cross-order unlinkability ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unlinkability {
    pub orders: u32,
    pub subjects: u32,
    pub distinct_pseudonyms: u32,
    pub collisions_within_order: u32,
    pub collisions_across_orders: u32,
    pub any_pseudonym_equals_wallet: bool,
}

/// A pseudonym must be stable inside one order (so the recipient can follow a counterparty through
/// the return) and unlinkable across orders (so two authorities cannot join their files). Measured
/// by generating the whole cross product and counting.
pub fn unlinkability(orders: u32, subjects: u32) -> Unlinkability {
    let mut all: BTreeMap<String, BTreeSet<[u8; 32]>> = BTreeMap::new();
    let mut within = 0u32;
    let mut equals = false;
    for o in 0..orders {
        let oid = *blake3::hash(&o.to_le_bytes()).as_bytes();
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        for s in 0..subjects {
            let wallet_hex = hex::encode([s as u8; 32]);
            let p = disclosure::pseudonym(&oid, &wallet_hex);
            if p == wallet_hex { equals = true; }
            // stability inside the order
            assert_eq!(p, disclosure::pseudonym(&oid, &wallet_hex));
            *seen.entry(p.clone()).or_insert(0) += 1;
            all.entry(p).or_default().insert(oid);
        }
        within += seen.values().filter(|c| **c > 1).count() as u32;
    }
    let across = all.values().filter(|v| v.len() > 1).count() as u32;
    Unlinkability {
        orders,
        subjects,
        distinct_pseudonyms: all.len() as u32,
        collisions_within_order: within,
        collisions_across_orders: across,
        any_pseudonym_equals_wallet: equals,
    }
}

// ───────────────────────────── M4 · tamper sweep ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TamperSweep {
    pub mutations: u32,
    pub caught: u32,
    pub missed: Vec<String>,
    pub caught_ratio: f64,
}

/// Mutate the packet in every way a dishonest intermediary plausibly would, and count how many the
/// recipient's `verify_packet` refuses. A single miss here is a hole in the whole scheme, so the
/// misses are listed by name rather than summarized.
pub fn tamper_sweep(c: &SupremeCourt, packet: &DisclosurePacket, roots: &BTreeMap<u64, [u8; 32]>, now: u64) -> TamperSweep {
    let mut cases: Vec<(String, DisclosurePacket)> = Vec::new();
    let push = |v: &mut Vec<(String, DisclosurePacket)>, n: &str, p: DisclosurePacket| v.push((n.into(), p));

    let mut p = packet.clone(); p.records[0].fields.insert("amount".into(), "1".into());
    push(&mut cases, "change a disclosed amount", p);
    let mut p = packet.clone(); if let Some(f) = p.records[0].fields.get_mut("token") { *f = hex::encode([9u8; 32]); }
    push(&mut cases, "change a disclosed token", p);
    let mut p = packet.clone(); p.records[0].leaf = [0xEE; 32];
    push(&mut cases, "swap a record's leaf", p);
    let mut p = packet.clone(); p.records.remove(0);
    push(&mut cases, "drop a record", p);
    let mut p = packet.clone(); let dup = p.records[0].clone(); p.records.push(dup);
    push(&mut cases, "duplicate a record", p);
    let mut p = packet.clone(); p.records.swap(0, 1);
    push(&mut cases, "reorder two records", p);
    let mut p = packet.clone(); p.records[0].height += 1;
    push(&mut cases, "restate a record's block height", p);
    let mut p = packet.clone(); p.records[0].proof.siblings.pop();
    push(&mut cases, "shorten an inclusion proof", p);
    let mut p = packet.clone(); if let Some(s) = p.records[0].proof.siblings.first_mut() { *s = [1u8; 32]; }
    push(&mut cases, "corrupt an inclusion-proof sibling", p);
    let mut p = packet.clone(); p.records[0].proof.index += 1;
    push(&mut cases, "move a record's index", p);
    let mut p = packet.clone(); p.records[0].redacted.clear();
    push(&mut cases, "claim nothing was redacted", p);
    let mut p = packet.clone(); p.summary.records += 1;
    push(&mut cases, "inflate the summary count", p);
    if let Some((k, _)) = packet.summary.per_token.iter().next() {
        let mut p = packet.clone();
        let k = k.clone();
        if let Some(t) = p.summary.per_token.get_mut(&k) { t.sent = t.sent.saturating_add(1); }
        push(&mut cases, "inflate a summary total", p);
    }
    let mut p = packet.clone(); p.seal.body.packet_root = [7u8; 32];
    push(&mut cases, "restate the sealed packet root", p);
    let mut p = packet.clone(); p.seal.body.expires_ts = u64::MAX;
    push(&mut cases, "extend the seal's expiry", p);
    let mut p = packet.clone(); p.seal.body.jurisdiction = "XX/OTHER".into();
    push(&mut cases, "re-address the seal to another authority", p);
    let mut p = packet.clone(); p.seal.body.purpose = Purpose::CourtOrder;
    push(&mut cases, "upgrade the sealed purpose", p);
    let mut p = packet.clone(); p.seal.sig_hex = hex::encode(vec![0u8; 292]);
    push(&mut cases, "replace the signature with zeroes", p);
    let mut p = packet.clone(); p.seal.body.constitution_hash = [3u8; 32];
    push(&mut cases, "seal under a different constitution", p);
    let mut p = packet.clone(); p.order.request.scope.from_height = 0; p.order.request.scope.to_height = u64::MAX;
    push(&mut cases, "widen the order's scope after the fact", p);
    let mut p = packet.clone(); p.order.minimization.reveal_counterparties = true;
    push(&mut cases, "claim the order allowed counterparties", p);
    let mut p = packet.clone(); p.order.minimization.carry_full_event = true;
    push(&mut cases, "claim the order allowed full events", p);
    let mut p = packet.clone(); p.order.votes_for += 1;
    push(&mut cases, "inflate the panel's vote for the order", p);
    let mut p = packet.clone(); p.order.panel.push(w(99));
    push(&mut cases, "add a judge to the order's panel", p);
    let mut p = packet.clone(); p.order.expires_ts = u64::MAX;
    push(&mut cases, "extend the order's own expiry", p);
    let mut p = packet.clone(); p.order.request.jurisdiction = crate::Jurisdiction::new("XX", "OTHER");
    push(&mut cases, "re-address the order to another authority", p);
    let mut p = packet.clone(); p.schema = "sigil-court/disclosure/v2".into();
    push(&mut cases, "downgrade the schema", p);
    let mut p = packet.clone(); p.viewing_grants.push(crate::ViewingGrant { subject: hex::encode(w(9)), viewing_key_hex: hex::encode([1u8; 32]) });
    push(&mut cases, "append a viewing-key grant", p);
    // Re-seal with a different court key: the packet is internally consistent but not ours.
    let forger = crate::CourtKeys::from_seed(&[0xFF; 32]);
    let mut p = packet.clone();
    let sig = flux_sqisign::sign(&p.seal.body.signing_bytes(), &forger.sk, &forger.pk).unwrap();
    p.seal.court_pubkey_hex = hex::encode(&forger.pk);
    p.seal.sig_hex = hex::encode(sig);
    push(&mut cases, "re-seal the whole packet with a forged court key", p);

    let mut caught = 0u32;
    let mut missed = Vec::new();
    for (name, p) in &cases {
        match verify_packet(p, roots, c.public_key(), now) {
            Err(_) => caught += 1,
            Ok(_) => missed.push(name.clone()),
        }
    }
    TamperSweep { mutations: cases.len() as u32, caught, caught_ratio: caught as f64 / cases.len() as f64, missed }
}

// ───────────────────────────── M5 · the solo-path search ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoalitionRow {
    pub rank: String,
    pub electorate: u32,
    pub min_coalition: u32,
    pub solo_paths_found: u32,
    pub vote_subsets_tried: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoloSearch {
    pub bench_size: u32,
    pub rows: Vec<CoalitionRow>,
    pub total_subsets_tried: u64,
    pub total_solo_paths: u32,
}

/// "Code is law" is only worth saying if the code refuses the thing it forbids. Art. VIII says a
/// rank is never granted solo, so this enumerates EVERY subset of the eligible electorate for every
/// rung of the ladder and reports the smallest one that carries — and how many subsets of size ≤ 1
/// do. An exhaustive search over the vote space, not a spot check.
///
/// The search runs against [`Bench`] directly rather than a whole [`SupremeCourt`], for two
/// reasons: the promotion law lives there, and a court would re-derive an SQIsign key on every one
/// of the ~128 rebuilds for nothing.
///
/// The candidate is seated one rung below the target and handed every credential and deed that
/// rung demands, so the ONLY variable is who voted. A multi-rung climb reuses the SAME voter set
/// throughout — a lone actor may not borrow a fresh coalition for each rung.
pub fn solo_search() -> SoloSearch {
    fn seat(target: Rank) -> (crate::Bench, Vec<Rank>) {
        let mut b = crate::Bench::new();
        b.appoint(w(1), Rank::ChiefJustice, 1).unwrap();
        b.appoint(w(2), Rank::Justice, 1).unwrap();
        b.appoint(w(3), Rank::Justice, 1).unwrap();
        b.appoint(w(4), Rank::Magistrate, 1).unwrap();
        b.appoint(w(5), Rank::Magistrate, 1).unwrap();
        let (start, path) = match target {
            Rank::Advocate => (Rank::Clerk, vec![Rank::Advocate]),
            Rank::Magistrate => (Rank::Clerk, vec![Rank::Advocate, Rank::Magistrate]),
            Rank::Justice => (Rank::Magistrate, vec![Rank::Justice]),
            Rank::ChiefJustice => (Rank::Justice, vec![Rank::ChiefJustice]),
            Rank::Clerk => (Rank::Clerk, vec![]),
        };
        b.appoint(w(9), start, 2).unwrap();
        for course in Course::ALL {
            b.sit_exam(&w(9), course, 10_000).unwrap();
        }
        // Exactly the deeds the whole path demands, and not one more.
        let deeds = path.iter().filter_map(|r| crate::Requirements::for_rank(*r)).map(|r| r.min_panels_sat).max().unwrap_or(0);
        for _ in 0..deeds {
            b.record_panel(&[w(9)]);
        }
        (b, path)
    }

    let mut rows = Vec::new();
    let mut total = 0u64;
    let mut solo = 0u32;
    for target in [Rank::Advocate, Rank::Magistrate, Rank::Justice, Rank::ChiefJustice] {
        let (probe, path) = seat(target);
        // Voters must be eligible at EVERY rung, so the search space is the narrowest electorate
        // on the path — the widest set a single coalition could draw from.
        let min_rank = path.iter().filter_map(|r| crate::Requirements::for_rank(*r)).map(|r| r.electorate_min_rank).max().unwrap_or(Rank::Justice);
        let electorate: Vec<WalletId> = probe.at_least(min_rank).into_iter().filter(|x| *x != w(9)).collect();
        let n = electorate.len();
        let mut min_carry = u32::MAX;
        let mut tried = 0u64;
        let mut solo_here = 0u32;
        for mask in 0u32..(1u32 << n) {
            let voters: Vec<WalletId> = (0..n).filter(|i| mask >> i & 1 == 1).map(|i| electorate[i]).collect();
            tried += 1;
            let (mut b, path) = seat(target);
            let mut ok = true;
            for step in path {
                if b.promote(&w(9), step, &voters).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                min_carry = min_carry.min(voters.len() as u32);
                if voters.len() <= 1 {
                    solo_here += 1;
                }
            }
        }
        assert_ne!(min_carry, u32::MAX, "{} is unreachable even with the full electorate — the search is mis-seeded, not the law", target.name());
        total += tried;
        solo += solo_here;
        rows.push(CoalitionRow {
            rank: target.name().into(),
            electorate: n as u32,
            min_coalition: min_carry,
            solo_paths_found: solo_here,
            vote_subsets_tried: tried,
        });
    }
    SoloSearch { bench_size: 6, rows, total_subsets_tried: total, total_solo_paths: solo }
}

// ───────────────────────────── M6/M7 · cost and scaling ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleRow {
    pub blocks: u64,
    pub events_per_block: u32,
    pub chain_events: u64,
    pub chain_bytes: u64,
    pub records: u32,
    pub packet_bytes: u64,
    pub proof_siblings: u32,
    pub mean_siblings_per_record: f64,
    pub export_ms: f64,
    pub verify_ms: f64,
    pub verify_ms_per_record: f64,
    /// Packet bytes ÷ bytes of the blocks it draws from.
    pub packet_over_chain: f64,
}

/// The recipient's cost, and the size of what they hold, as the chain grows. The interesting
/// quantity is the slope: an inclusion proof is `⌈log2 n⌉` siblings, so the packet should grow
/// like `records × log(block size)`, not like the chain.
pub fn scaling(sizes: &[(u64, usize)], subject: WalletId) -> Vec<ScaleRow> {
    let mut out = Vec::new();
    for (blocks, per_block) in sizes {
        let ch = synthetic_chain(*blocks, *per_block, subject, 7);
        let chain_bytes: u64 = ch.iter().flat_map(|b| b.events.iter()).map(|e| e.encode().len() as u64).sum();
        let mut c = seated_court();
        let req = request(Purpose::Tax, tax_scope(subject, 0, blocks - 1), None, 1_000);
        let t0 = Instant::now();
        let p = ordered_export(&mut c, req, &ch, 10, 1_000);
        let export_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();
        let t1 = Instant::now();
        verify_packet(&p, &roots, c.public_key(), 1_500).unwrap();
        let verify_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let sibs: u32 = p.records.iter().map(|r| r.proof.siblings.len() as u32).sum();
        let bytes = p.to_json().len() as u64;
        out.push(ScaleRow {
            blocks: *blocks,
            events_per_block: *per_block as u32,
            chain_events: blocks * *per_block as u64,
            chain_bytes,
            records: p.records.len() as u32,
            packet_bytes: bytes,
            proof_siblings: sibs,
            mean_siblings_per_record: if p.records.is_empty() { 0.0 } else { sibs as f64 / p.records.len() as f64 },
            export_ms,
            verify_ms,
            verify_ms_per_record: if p.records.is_empty() { 0.0 } else { verify_ms / p.records.len() as f64 },
            packet_over_chain: if chain_bytes == 0 { 0.0 } else { bytes as f64 / chain_bytes as f64 },
        });
    }
    out
}

/// Where the recipient's verification time actually goes. The seal is one signature regardless of
/// packet size; the Merkle work is per record. Separating them is the difference between "verify
/// is expensive" and "the constant is expensive".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSplit {
    pub sqisign_verify_ms: f64,
    pub sqisign_sign_ms: f64,
    pub sig_bytes: usize,
    pub pubkey_bytes: usize,
    pub merkle_verify_ms_per_record: f64,
    pub seal_share_of_verify: f64,
    pub samples: u32,
}

pub fn cost_split(subject: WalletId, samples: u32) -> CostSplit {
    let keys = crate::CourtKeys::from_seed(&[9; 32]);
    let msg = b"sigil-court cost split";
    let t = Instant::now();
    for _ in 0..samples { flux_sqisign::sign(msg, &keys.sk, &keys.pk).unwrap(); }
    let sign_ms = t.elapsed().as_secs_f64() * 1000.0 / samples as f64;
    let sig = flux_sqisign::sign(msg, &keys.sk, &keys.pk).unwrap();
    let t = Instant::now();
    for _ in 0..samples { assert!(flux_sqisign::verify(msg, &sig, &keys.pk).unwrap()); }
    let verify_ms = t.elapsed().as_secs_f64() * 1000.0 / samples as f64;

    let ch = synthetic_chain(40, 16, subject, 11);
    let mut c = seated_court();
    let p = ordered_export(&mut c, request(Purpose::Tax, tax_scope(subject, 0, 39), None, 1_000), &ch, 10, 1_000);
    let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();
    let t = Instant::now();
    let reps = 20u32;
    for _ in 0..reps {
        for r in &p.records {
            assert!(crate::merkle::verify(r.leaf, &r.proof, roots[&r.height]));
        }
    }
    let merkle_total = t.elapsed().as_secs_f64() * 1000.0 / reps as f64;
    let per_record = merkle_total / p.records.len().max(1) as f64;
    CostSplit {
        sqisign_verify_ms: verify_ms,
        sqisign_sign_ms: sign_ms,
        sig_bytes: sig.len(),
        pubkey_bytes: keys.pk.len(),
        merkle_verify_ms_per_record: per_record,
        seal_share_of_verify: verify_ms / (verify_ms + merkle_total),
        samples,
    }
}

/// Docket cost per act — the court's own storage footprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocketCost {
    pub entries: u32,
    pub encoded_bytes: u64,
    pub bytes_per_entry: f64,
    pub chain_verify_ms: f64,
    pub histogram: BTreeMap<String, u32>,
}

pub fn docket_cost(reps: u32) -> DocketCost {
    let mut c = seated_court();
    for i in 0..reps {
        let case = c.file_case(w(30), Some(w(31)), Article::PrivacyByDefault, format!("case {i}"), i as u64);
        let panel = c.hear(case, 3, i as u64).unwrap();
        let votes: Vec<(WalletId, bool)> = panel.iter().map(|p| (*p, true)).collect();
        c.rule(case, Verdict::Upheld, format!("holding {i}"), &votes, vec![], i as u64).unwrap();
    }
    let bytes: u64 = c.docket().entries().iter().map(|e| e.event.encode().len() as u64 + 48).sum();
    let t = Instant::now();
    c.docket().verify_chain().unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let n = c.docket().len() as u32;
    DocketCost {
        entries: n,
        encoded_bytes: bytes,
        bytes_per_entry: bytes as f64 / n as f64,
        chain_verify_ms: ms,
        histogram: c.docket().histogram().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

// ───────────────────────────── the report ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub generated_unix: u64,
    pub constitution_version: u32,
    pub constitution_hash: String,
    pub articles: usize,
    pub court_pubkey: String,
    pub avalanche: Avalanche,
    pub minimization: Vec<MinimizationRow>,
    pub unlinkability: Unlinkability,
    pub tamper: TamperSweep,
    pub solo: SoloSearch,
    pub scaling: Vec<ScaleRow>,
    pub cost: CostSplit,
    pub docket: DocketCost,
    /// Claims this harness does NOT measure, named so the paper cannot imply it did.
    pub not_measured: Vec<String>,
}

pub fn run() -> Report {
    let subject = w(0x77);
    let ch = synthetic_chain(60, 12, subject, 3);
    let mut c = seated_court();
    let p = ordered_export(&mut c, request(Purpose::Tax, tax_scope(subject, 0, 59), None, 1_000), &ch, 10, 1_000);
    let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();
    Report {
        generated_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        constitution_version: crate::CONSTITUTION_VERSION,
        constitution_hash: crate::constitution_hash_hex(),
        articles: Article::ALL.len(),
        court_pubkey: hex::encode(c.public_key()),
        avalanche: avalanche(64),
        minimization: minimization(&ch, subject),
        unlinkability: unlinkability(256, 64),
        tamper: tamper_sweep(&c, &p, &roots, 1_500),
        solo: solo_search(),
        scaling: scaling(&[(10, 8), (40, 12), (100, 16), (250, 20), (500, 24)], subject),
        cost: cost_split(subject, 24),
        docket: docket_cost(60),
        not_measured: vec![
            "Whether a recipient re-identifies a pseudonymized counterparty using data from outside the packet. The pseudonym is unlinkable across orders by construction and measured to be so here, but an authority holding its own records can still join on amount and timing; nothing in this design prevents that.".into(),
            "Whether the court's own key is held honestly. Every guarantee downstream of the seal reduces to that key, and this harness signs with a key it generated itself.".into(),
            "Behaviour under an adversarial bench. The solo search fixes the constitution and varies the votes; it says nothing about a bench where a majority is already captured.".into(),
            "Any property of the live chain. Every chain here is synthetic and generated in-process; the block roots are computed by the same code that proves against them.".into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_measurements_hold_their_own_claims() {
        // Cheap versions of the same measurements, so a regression fails the suite rather than
        // quietly changing a number in the paper.
        let a = avalanche(16);
        assert_eq!(a.collisions, 0, "distinct courts must have distinct roots");
        assert!(a.min_flipped > 64, "a one-basis-point change must move most of the root, got {}", a.min_flipped);

        let u = unlinkability(32, 16);
        assert_eq!(u.collisions_within_order, 0);
        assert_eq!(u.collisions_across_orders, 0);
        assert!(!u.any_pseudonym_equals_wallet);
        assert_eq!(u.distinct_pseudonyms, 32 * 16);

        let s = solo_search();
        assert_eq!(s.total_solo_paths, 0, "Art. VIII: no rank is reachable by a lone voter");
        assert!(s.rows.iter().all(|r| r.min_coalition >= 2));

        let subject = w(0x77);
        let ch = synthetic_chain(20, 8, subject, 3);
        let mut c = seated_court();
        let p = ordered_export(&mut c, request(Purpose::Tax, tax_scope(subject, 0, 19), None, 1_000), &ch, 10, 1_000);
        let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();
        let t = tamper_sweep(&c, &p, &roots, 1_500);
        assert!(t.missed.is_empty(), "unrefused mutations: {:?}", t.missed);
        assert_eq!(t.caught, t.mutations);

        let m = minimization(&ch, subject);
        let tax = m.iter().find(|r| r.purpose == "Tax").unwrap();
        let court = m.iter().find(|r| r.purpose == "CourtOrder").unwrap();
        assert_eq!(tax.counterparties_learned, 0, "a Tax order must leak no counterparty in clear");
        assert!(court.counterparties_learned > 0, "a CourtOrder deliberately does");
        assert!(tax.disclosed_ratio < court.disclosed_ratio, "minimization must actually minimize");
        assert!(tax.records > 0);
    }
}
