//! One-off diagnostic: print a block's hash, parent_hash, height, and state
//! roots for a given local chain.log directory + height, so two nodes' data
//! can be directly, independently compared (not just "no errors logged").
//! Usage: verify_block_at <chain_log_dir> <height>

use sigil_node::chain_log::ChainLog;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: verify_block_at <chain_log_dir> <height>");
        std::process::exit(2);
    }
    let dir = Path::new(&args[1]);
    let height: u64 = args[2].parse().expect("height must be a u64");

    let log = ChainLog::open(dir).expect("failed to open chain.log dir");
    // 2026-08-26: was `log.get(height)`. `get()` indexes the dense `offsets` array by
    // ORDINAL record position, not by real header height — and this braid writes
    // orphaned candidate blocks into the same log, so ordinal drifts ahead of height
    // (measured on Epsilon: asking for 2,172,423 returned header height 2,171,346, a
    // 1,077 drift). For a diagnostic whose whole purpose is comparing two nodes' data
    // AT A HEIGHT, that silently compares the wrong blocks. `get_by_height` re-reads
    // each record's real height and matches exactly.
    match log.get_by_height(height) {
        Some(block) => {
            println!("height={}", block.header.height);
            println!("hash={}", hex::encode(block.hash()));
            println!("parent_hash={}", hex::encode(block.header.parent_hash));
            println!("wallet_state_root={}", hex::encode(block.header.wallet_state_root));
            println!("dex_state_root={}", hex::encode(block.header.dex_state_root));
            println!("event_log_root={}", hex::encode(block.header.event_log_root));
            println!("contract_state_root={}", hex::encode(block.header.contract_state_root));
            println!("mutation_count={}", block.transition.mutations.len());
            println!("sig_scheme={:?}", block.header.sig_scheme);
            println!("producer={}", hex::encode(block.header.producer));
            println!("producer_sig_len={}", block.header.producer_sig.0.len());
            if block.header.sig_scheme == sigil_header::SigScheme::Ed25519Hot {
                println!("ed25519_verify={:?}", block.header.verify_producer_sig());
            }
        }
        None => {
            eprintln!("no block at height {} in {}", height, dir.display());
            std::process::exit(1);
        }
    }
}
