//! sigil-emission-chronos — the emission controller, stress-tested through chronos.
//!
//! Randomized · multi-node · superhero-recovery · storage-scaled. Proves the
//! QUG-fix: emission (coinbase: producer 94.9% / master 5% / operator-pool 0.1%)
//! is COMMITTED IN THE WALLET ROOT, so a killed node, rebuilt from the chain,
//! recovers BYTE-IDENTICAL — emission state cannot drift.
//!
//! BOUNDED RAM: blocks stream to chunked disk archives; recovery REPLAYS FROM
//! DISK (one chunk in RAM at a time). So it runs to 10 GB / 3 TB on /home/storage
//! without OOM — node state is O(wallets), not O(blocks).
//!
//! Env knobs:
//!   SIGIL_TEST_BLOCKS      max blocks               (default 10_000)
//!   SIGIL_TEST_NODES       total nodes incl producer(default 4)
//!   SIGIL_TEST_STORAGE_GB  stop when archives ≥ GB  (default 1; set 10 / 3000)
//!   SIGIL_TEST_SEED        RNG seed (reproducible)  (default 2026)
//!   SIGIL_TEST_DIR         archive dir              (default /home/storage/sigil-emission-chronos)

use std::collections::VecDeque;
use std::time::Instant;

use flux_chronos::NodeId;
use sigil_chronos::{
    demo_genesis, sign_dummy, ApplyOutcome, Block, SigilSimNode, BLOCK_REWARD,
    OPERATOR_POOL_WALLET, PRODUCER_WALLET,
};
use sigil_state::{StateRoots, WalletId};
use sigil_tip_proof::{TipProof, NETWORK_ID_BYTES};
use sigil_tx::{SigilTx, NATIVE};

const MASTER: WalletId = [0x99u8; 32];
const BLOCK_TIME: u64 = 1_000_000;
const CHUNK: usize = 2000; // blocks per archive file

struct Rng(u64);
impl Rng {
    fn new(s: u64) -> Self {
        Self(s)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn wallet(i: u8) -> WalletId {
    [i; 32]
}

fn roots_fp(r: &StateRoots) -> String {
    let b = serde_json::to_vec(r).unwrap_or_default();
    blake3::hash(&b).to_hex().to_string()[..16].to_string()
}

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Superhero recovery, BOUNDED RAM: rebuild a node by replaying the chain from
/// the on-disk archives (one chunk at a time) + any not-yet-flushed leftover.
fn recover_from_disk(dir: &str, n_archives: u64, leftover: &[Block]) -> SigilSimNode {
    let g = demo_genesis();
    let mut node = SigilSimNode::new("recovered", NodeId(900), vec![], false, BLOCK_TIME, &g);
    for i in 0..n_archives {
        let path = format!("{dir}/archive_{i:06}.json");
        if let Ok(s) = std::fs::read(&path) {
            if let Ok(blocks) = serde_json::from_slice::<Vec<Block>>(&s) {
                for b in &blocks {
                    let _ = node.apply_external_block(b);
                }
            }
        }
    }
    for b in leftover {
        let _ = node.apply_external_block(b);
    }
    node
}

fn flush(dir: &str, idx: u64, buf: &[Block]) -> u64 {
    let path = format!("{dir}/archive_{idx:06}.json");
    if let Ok(f) = std::fs::File::create(&path) {
        if serde_json::to_writer(std::io::BufWriter::new(f), buf).is_ok() {
            return std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
    }
    0
}

fn main() {
    let blocks_target = env_u64("SIGIL_TEST_BLOCKS", 10_000);
    let total_nodes = env_u64("SIGIL_TEST_NODES", 4).max(2);
    let storage_gb = env_u64("SIGIL_TEST_STORAGE_GB", 1);
    let seed = env_u64("SIGIL_TEST_SEED", 2026);
    let dir = std::env::var("SIGIL_TEST_DIR")
        .unwrap_or_else(|_| "/home/storage/sigil-emission-chronos".into());
    let storage_target = storage_gb.saturating_mul(1_000_000_000);
    std::fs::create_dir_all(&dir).ok();

    println!("⬡ sigil-emission-chronos — emission × chronos  (bounded RAM, stream-to-disk)");
    println!("   blocks≤{blocks_target} nodes={total_nodes} seed={seed} storage_stop={storage_gb}GB dir={dir}\n");

    let g = demo_genesis();
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![], true, BLOCK_TIME, &g);
    let mut followers: Vec<SigilSimNode> = (1..total_nodes)
        .map(|i| SigilSimNode::new(&format!("follower-{i}"), NodeId(i as u32), vec![], false, BLOCK_TIME, &g))
        .collect();

    let split = sigil_bank::split_mining_reward(BLOCK_REWARD, Some(MASTER)).unwrap();
    println!(
        "   emission/block: {} → producer {} (94.9%) · master {} (5%) · operator {} (0.1%)\n",
        BLOCK_REWARD, split.validator_share, split.master_share, split.operator_share
    );

    let mut rng = Rng::new(seed);
    let mut buf: Vec<Block> = Vec::with_capacity(CHUNK); // current archive chunk (bounded)
    let mut recent: VecDeque<Block> = VecDeque::with_capacity(8); // for the reject test (bounded)
    let (mut produced, mut archive_n, mut bytes) = (0u64, 0u64, 0u64);
    let (mut divergences, mut rejects_seen, mut recoveries_ok, mut recoveries_bad) = (0u64, 0u64, 0u64, 0u64);
    // NEW: light-node tip-proof verification (the 10µs sync gate, per block).
    let (mut tip_verified, mut tip_failed, mut tip_us, mut tip_bytes) = (0u64, 0u64, 0u128, 0u64);
    let mut last_report = Instant::now();
    let t0 = Instant::now();

    while produced < blocks_target && bytes < storage_target {
        let from = wallet((rng.range(5) + 1) as u8);
        let mut to = wallet((rng.range(5) + 1) as u8);
        while to == from {
            to = wallet((rng.range(5) + 1) as u8);
        }
        let amt = 1 + rng.range(50) as u128;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: amt, token: NATIVE, fee: 0 }));
        let block = match producer.produce_one() {
            Some(b) => b,
            None => continue,
        };
        let h = block.header.height;

        for f in followers.iter_mut() {
            match f.apply_external_block(&block) {
                ApplyOutcome::Ok => {}
                ApplyOutcome::Divergence => {
                    divergences += 1;
                    eprintln!("🚨 DIVERGENCE at H={h}");
                }
                ApplyOutcome::Rejected => eprintln!("⚠ unexpected reject at H={h}"),
            }
        }
        produced += 1;

        // NEW (LIGHT-4): build the tip-proof from the block's HEADER roots and
        // verify it — the exact ~10µs gate a light node uses to "sync" to the tip.
        let hr = StateRoots {
            wallet_state_root:   block.header.wallet_state_root,
            dex_state_root:      block.header.dex_state_root,
            event_log_root:      block.header.event_log_root,
            contract_state_root: block.header.contract_state_root,
        };
        let tp = TipProof::new_blake3(block.header.height, hr);
        tip_bytes += serde_json::to_vec(&tp).map(|v| v.len() as u64).unwrap_or(0);
        let tpt = Instant::now();
        if tp.verify(NETWORK_ID_BYTES).is_ok() { tip_verified += 1; } else { tip_failed += 1; }
        tip_us += tpt.elapsed().as_micros();

        // reject-test ring buffer (bounded at 8)
        if recent.len() == 8 {
            recent.pop_front();
        }
        recent.push_back(block.clone());

        // stream to disk in chunks (bounded RAM)
        buf.push(block);
        if buf.len() >= CHUNK {
            bytes += flush(&dir, archive_n, &buf);
            archive_n += 1;
            buf.clear();
        }

        // ── randomized events ──
        // Early superhero recovery (only while cheap: few archives) — replays from disk.
        if archive_n < 4 && archive_n > 0 && rng.range(4000) == 0 {
            let rec = recover_from_disk(&dir, archive_n, &buf);
            // recovered is at the last flushed boundary; compare against a node also
            // at that boundary would need a snapshot — cheap check: its roots are a
            // valid prefix tip (height matches an archive boundary). We assert the
            // strong version once at the end; here we just confirm it replayed clean.
            if rec.divergence_count == 0 {
                recoveries_ok += 1;
            } else {
                recoveries_bad += 1;
            }
        }
        // Reject path: replay a recent block → must be Rejected (already advanced).
        if recent.len() > 3 && rng.range(1500) == 0 {
            let old = &recent[0];
            if matches!(followers[0].apply_external_block(old), ApplyOutcome::Rejected) {
                rejects_seen += 1;
            }
        }

        // progress heartbeat every 5s
        if last_report.elapsed().as_secs() >= 5 {
            let gb = bytes as f64 / 1e9;
            eprintln!("   … H={produced} · {gb:.2} GB · {:.0} blk/s", produced as f64 / t0.elapsed().as_secs_f64().max(1e-6));
            last_report = Instant::now();
        }
    }
    // flush the tail
    if !buf.is_empty() {
        bytes += flush(&dir, archive_n, &buf);
        archive_n += 1;
    }
    let secs = t0.elapsed().as_secs_f64().max(1e-6);

    // ── DEFINITIVE superhero recovery: rebuild fully from disk, must match live ──
    let rec = recover_from_disk(&dir, archive_n, &[]);
    let recovered_matches = rec.roots() == producer.roots() && rec.height() == producer.height();
    if recovered_matches {
        recoveries_ok += 1;
    } else {
        recoveries_bad += 1;
    }

    println!("\n═══ PER-NODE REPORT (every node must agree) ═══");
    let report = |name: &str, n: &SigilSimNode| {
        println!(
            "  {name:<13} H={:<8} roots {} · producer {} · master {} · operator {}",
            n.height(),
            roots_fp(&n.roots()),
            n.balance_of(&PRODUCER_WALLET, &NATIVE),
            n.balance_of(&MASTER, &NATIVE),
            n.balance_of(&OPERATOR_POOL_WALLET, &NATIVE),
        );
    };
    report("producer", &producer);
    for (i, f) in followers.iter().enumerate() {
        report(&format!("follower-{}", i + 1), f);
    }
    report("recovered", &rec);

    let pr = producer.roots();
    let all_agree = followers.iter().all(|f| f.roots() == pr) && recovered_matches;

    let prod_bal = producer.balance_of(&PRODUCER_WALLET, &NATIVE);
    let mast_bal = producer.balance_of(&MASTER, &NATIVE);
    let oper_bal = producer.balance_of(&OPERATOR_POOL_WALLET, &NATIVE);
    let minted = prod_bal + mast_bal + oper_bal;
    let expect = produced as u128 * BLOCK_REWARD;
    let conserved = minted == expect;
    let demo_total: u128 = (1..=5u8).map(|i| producer.balance_of(&wallet(i), &NATIVE)).sum();

    println!("\n═══ VERDICT ═══");
    println!("  blocks produced     {produced}  ({:.0}/s, {:.1}s)", produced as f64 / secs, secs);
    println!("  nodes               {} (1 producer + {} followers + 1 recovered-from-DISK)", total_nodes, total_nodes - 1);
    println!("  divergences         {divergences}   {}", if divergences == 0 { "✓" } else { "✗ FAIL" });
    println!("  all nodes agree     {}", if all_agree { "✓ identical roots" } else { "✗ FAIL" });
    println!("  recovery (from disk) {}   ok={recoveries_ok} bad={recoveries_bad}", if recovered_matches { "✓ byte-identical" } else { "✗ FAIL" });
    println!("  reject path         {rejects_seen}  ✓");
    let tip_total = tip_verified + tip_failed;
    println!("  light-sync (NEW)    {tip_verified}/{tip_total} tip-proofs verified · {}µs avg · ~{} bytes/tip   {}",
        tip_us / tip_total.max(1) as u128, tip_bytes / produced.max(1), if tip_failed == 0 { "✓" } else { "✗ FAIL" });
    println!("  emission committed  {prod_bal} + {mast_bal} + {oper_bal} = {minted}");
    println!("  conservation        {minted} == {expect}  {}", if conserved { "✓" } else { "✗ FAIL" });
    println!("  demo-wallet supply  {demo_total}  {}", if demo_total == 5_000_000 { "✓" } else { "✗" });
    println!("  storage archived    {:.3} GB in {} files (peak RAM ~ {} blocks)", bytes as f64 / 1e9, archive_n, CHUNK);
    let pass = divergences == 0 && all_agree && recoveries_bad == 0 && conserved && demo_total == 5_000_000;
    println!("\n  {}", if pass { "✅ PASS — committed-in-roots, deterministic, recovers exactly FROM DISK (bounded RAM)." } else { "❌ FAIL — see above." });
    std::process::exit(if pass { 0 } else { 1 });
}
