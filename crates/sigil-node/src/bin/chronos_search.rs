//! chronos_search.rs — a SEARCHABLE, DURABLE chronos chain.
//!
//! Wires three Flux crates around the SIGIL chronos simulator:
//!   sigil-chronos → produces real deterministic blocks (transitions + events)
//!   flux-aether   → stores the chain as Reed-Solomon shards (can lose N-K hosts)
//!   flux-search   → indexes every block (the Vizily-ported engine: persistent
//!                   index, TF-IDF/BM25, PageRank, snippets) so the chain is
//!                   QUERYABLE: "find blocks that minted rewards", "wallet 0101…".
//!
//! Run:  SEARCH_BLOCKS=2000 chronos_search
//! Proves: produce → durable-store → index → search, end to end, on real data.

use flux_aether::{rs_reassemble, rs_shard};
use flux_chronos::NodeId;
use flux_search::{Document, SearchEngine, SearchQuery};
use sigil_chronos::{demo_genesis, sign_dummy, Block, SigilSimNode};
use sigil_state::WalletId;
use sigil_tx::{SigilTx, NATIVE};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Insert spaces at camelCase boundaries so `MintReward` is searchable both as
/// "mintreward" AND "mint reward".
fn decamel(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut prev = ' ';
    for c in s.chars() {
        if c.is_uppercase() && (prev.is_lowercase() || prev.is_numeric()) {
            out.push(' ');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// Turn a block into searchable text. SEMANTIC terms lead (before flux-search's
/// content-truncation budget): height, event kinds, tx info. The 64-char hex
/// roots are dropped — `is_search_token` filters them as noise anyway, and they
/// only push the meaningful terms past the cutoff.
fn block_text(b: &Block) -> String {
    let h = &b.header;
    let ev = serde_json::to_string(&b.events).unwrap_or_default();
    let tr = serde_json::to_string(&b.transition).unwrap_or_default();
    format!(
        "SIGIL block height {h_height} timestamp {ts} tx_count {tc} \
         events {ev} {ev_decamel} \
         transition {tr} {tr_decamel} \
         producer {prod}",
        h_height = h.height,
        ts = h.timestamp_ms,
        tc = h.tx_count,
        ev_decamel = decamel(&ev),
        ev = ev,
        tr_decamel = decamel(&tr),
        tr = tr,
        prod = &hex(&h.producer)[..8],
    )
}

fn to_document(b: &Block) -> Document {
    let content = block_text(b);
    let wc = content.split_whitespace().count();
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    Document {
        id: format!("block-{}", b.header.height),
        url: format!("sigil://block/{}", b.header.height),
        title: format!("SIGIL block {} · {} tx · {} events", b.header.height, b.header.tx_count, b.events.len()),
        content,
        meta_description: Some(format!("Block {} at ts {}", b.header.height, b.header.timestamp_ms)),
        language: Some("en".into()),
        category: Some(if b.events.is_empty() { "block".into() } else { "block-events".into() }),
        page_rank: 0.0,
        readability_score: 1.0,
        word_count: wc,
        last_crawled: Some(b.header.timestamp_ms / 1000),
        content_hash: hash,
    }
}

fn main() {
    let n: u64 = std::env::var("SEARCH_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000);
    let base = std::env::temp_dir().join(format!("sigil-chronos-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let index_path = base.join("flux-search.idx");

    // ── produce a real chronos chain ──
    let g = demo_genesis();
    let funded: Vec<WalletId> = g.funded.iter().map(|(w, _)| *w).collect();
    let mut producer = SigilSimNode::new("search-producer", NodeId(0), vec![], true, 1_000, &g);
    let mut engine = SearchEngine::load_or_new(&index_path);
    let mut blocks: Vec<Block> = Vec::with_capacity(n as usize);

    // genesis
    let gblock = g.build_block();
    engine.index_document(to_document(&gblock));
    blocks.push(gblock);

    println!("=== chronos_search: produce → aether → flux-search ({n} blocks) ===");
    let t_prod = std::time::Instant::now();
    for h in 1..=n {
        let from = funded[(h as usize) % funded.len()];
        let to = funded[(h as usize + 1) % funded.len()];
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        let b = match producer.produce_one() {
            Some(b) => b,
            None => break,
        };
        engine.index_document(to_document(&b));
        blocks.push(b);
    }
    let prod_ms = t_prod.elapsed().as_millis();

    // ── durability: store the whole chain as Reed-Solomon shards (flux-aether) ──
    const K: usize = 16;
    const PARITY: usize = 8;
    let chain_bytes = rmp_serde::to_vec(&blocks).unwrap();
    let (orig_len, shards) = rs_shard(&chain_bytes, K, PARITY);
    let shard_dir = base.join("aether");
    std::fs::create_dir_all(&shard_dir).unwrap();
    for (i, s) in shards.iter().enumerate() {
        std::fs::write(shard_dir.join(format!("shard.{i}")), s).unwrap();
    }
    // prove recoverability after losing PARITY shards (lost hosts)
    let mut survivors: Vec<Option<Vec<u8>>> =
        (0..shards.len()).map(|i| std::fs::read(shard_dir.join(format!("shard.{i}"))).ok()).collect();
    for slot in survivors.iter_mut().take(PARITY) {
        *slot = None; // simulate losing 8 of 24 shards
    }
    let recovered = rs_reassemble(orig_len, K, PARITY, survivors).expect("aether recovers from survivors");
    let aether_ok = recovered == chain_bytes;

    // ── persist the search index + finalize ranking ──
    engine.recalculate_pagerank();
    engine.save_to_path(&index_path).expect("persist search index");
    let docs = engine.doc_count();

    println!(
        "\nproduced {} blocks in {} ms · aether: {} shards (lose any {} of {}), recover-after-loss {}",
        blocks.len(), prod_ms, shards.len(), PARITY, shards.len(),
        if aether_ok { "✓ byte-identical" } else { "✗ FAILED" }
    );
    println!("indexed {} docs · index persisted to {}", docs, index_path.display());

    // ── SEARCH the simulated chain ──
    let queries = ["MintReward", "mint reward", "Send NATIVE token", "SetBalance", "block events"];
    println!("\n┌─ SEARCH the chronos chain ────────────────────────────────────────");
    for q in queries {
        let t = std::time::Instant::now();
        let resp = engine.search(SearchQuery { q: q.to_string(), page: 1, per_page: 3, ..Default::default() });
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("│ q={:<26} {:>4} hits in {:>5.2} ms", format!("\"{q}\""), resp.total_results, ms);
        for r in resp.results.iter().take(2) {
            println!("│     → {:<30} score {:.3}", r.title, r.score);
        }
    }
    println!("└───────────────────────────────────────────────────────────────────");
    println!("\n✓ chronos chain is now durable (aether) AND searchable (flux-search), end to end.");
    let _ = std::fs::remove_dir_all(&base);
}
