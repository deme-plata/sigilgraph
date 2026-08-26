//! search_index.rs — wires the real `flux-search` engine into the wallet's
//! search (`/v1/search`, mirrored at `/api/v1/search`).
//!
//! Deliberately DOES NOT hook into block-apply. The live producer applies
//! blocks through more than one path (the DAGKnight/braid settlement path and
//! a linear fallback — see `main.rs`'s `dag_drain_apply` vs. `chain.apply`),
//! and that code is the single most consensus-sensitive part of the node.
//! Instead this indexes by TAILING `ChainLog` — the durable on-disk log every
//! applied block is already written to, regardless of which path applied it.
//! `ChainLog::replay_from` opens its own independent read handle (no lock, no
//! coordination with the live writer), so this can run as a fully separate
//! background task: zero changes to, and zero risk to, the actual
//! block-application/consensus code. The trade-off is a few seconds of lag
//! between "block committed" and "block searchable" (one poll interval),
//! which is the right side of that trade-off for a search feature.
//!
//! Address searchability: `WalletId`/`TokenId`/`PoolId`/etc. are all plain
//! `[u8; 32]` type aliases (checked: `sigil-state/src/lib.rs`), so serde's
//! default `Vec<u8>`-shaped JSON serialization would embed an address as
//! `[215,202,...]`, not the hex string a user would actually type or paste.
//! `hexify_byte32_arrays` walks the serialized event JSON generically and
//! rewrites every exact-32-small-integers array to its hex string — this
//! covers every address-shaped field in every `SigilEvent` variant, present
//! and future, without hand-matching the (large, still-growing) enum.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flux_search::{Document, SearchEngine};

use crate::block::Block;

/// Rewrite every JSON array of exactly 32 small integers (0-255) — the shape
/// serde gives a `[u8; 32]` — into its hex-string form, recursively. Turns
/// every WalletId/TokenId/PoolId/ContractId/ValidatorId/hash field in a
/// serialized `SigilEvent` into something a pasted hex address will actually
/// match via `literal_search`.
fn hexify_byte32_arrays(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Array(arr) => {
            let is_byte32 = arr.len() == 32
                && arr.iter().all(|x| x.as_u64().map(|n| n <= 255).unwrap_or(false));
            if is_byte32 {
                let bytes: Vec<u8> = arr.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect();
                *v = serde_json::Value::String(hex::encode(bytes));
            } else {
                for item in arr.iter_mut() {
                    hexify_byte32_arrays(item);
                }
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                hexify_byte32_arrays(val);
            }
        }
        _ => {}
    }
}

/// Insert spaces at camelCase boundaries so `MintReward` is searchable both
/// as "mintreward" and "mint reward" — same trick chronos_search.rs uses.
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

/// Build the searchable `Document` for one block: height/timestamp/producer
/// plus every event, hex-addressed and decamel-expanded so address lookups,
/// event-kind lookups ("mint reward", "swap"), and free-text all work off
/// the same indexed content.
fn to_document(b: &Block) -> Document {
    let h = &b.header;
    let mut events_val = serde_json::to_value(&b.events).unwrap_or(serde_json::Value::Null);
    hexify_byte32_arrays(&mut events_val);
    let events_json = serde_json::to_string(&events_val).unwrap_or_default();
    let events_decamel = decamel(&events_json);
    let producer_hex = hex::encode(h.producer);

    let content = format!(
        "SIGIL block height {height} timestamp {ts} tx_count {tc} producer {prod} \
         events {ev} {ev_decamel}",
        height = h.height,
        ts = h.timestamp_ms,
        tc = h.tx_count,
        prod = producer_hex,
        ev = events_json,
        ev_decamel = events_decamel,
    );
    let wc = content.split_whitespace().count();
    let content_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    Document {
        id: format!("block-{}", h.height),
        url: format!("sigil://block/{}", h.height),
        title: format!("SIGIL block {} · {} tx · {} events", h.height, h.tx_count, b.events.len()),
        content,
        meta_description: Some(format!("Block {} at ts {}", h.height, h.timestamp_ms)),
        language: Some("en".into()),
        category: Some(if b.events.is_empty() { "block".into() } else { "block-events".into() }),
        page_rank: 0.0,
        readability_score: 1.0,
        word_count: wc,
        last_crawled: Some(h.timestamp_ms / 1000),
        content_hash,
    }
}

/// Where the search index (and its progress marker) live — its own
/// subdirectory under the data root, same convention `snapshot.rs` already
/// uses for `aether` (never reusing `SIGIL_DB_PATH` directly).
fn search_dir(snap_dir: &Path) -> PathBuf {
    snap_dir.join("search-index")
}

fn index_file(snap_dir: &Path) -> PathBuf {
    search_dir(snap_dir).join("flux-search.idx")
}

fn progress_file(snap_dir: &Path) -> PathBuf {
    search_dir(snap_dir).join("indexed-to-height")
}

/// `None` means "nothing indexed yet" — distinct from `Some(0)` ("indexed
/// through height 0", i.e. genesis). Collapsing that distinction into a bare
/// `u64` (defaulting missing-file to `0`) was a real bug: `from = last + 1`
/// would then compute `from = 1` on a fresh node and PERMANENTLY skip height
/// 0 — found live on happysrv (2026-08-20): a real genesis block carrying a
/// real `MintReward` event never became searchable, no error, just silently
/// never indexed, because there is no "index 0, and also remember 0 is done"
/// representation in a plain integer that defaults to 0 either way.
fn read_progress(snap_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(progress_file(snap_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// The next height to index: 0 on a fresh node (never indexed anything,
/// including genesis), `last + 1` once something has been.
fn next_height_to_index(snap_dir: &Path) -> u64 {
    match read_progress(snap_dir) {
        Some(last) => last + 1,
        None => 0,
    }
}

fn write_progress(snap_dir: &Path, height: u64) {
    // Defensive create_dir_all: std::fs::write does NOT create the parent
    // directory, and silently no-ops (via the `let _` below) if it's
    // missing — worth not depending on every caller having already created
    // `search_dir` first (load_or_new does, but write_progress is also
    // called from the indexer's poll loop, long after startup).
    let _ = std::fs::create_dir_all(search_dir(snap_dir));
    let _ = std::fs::write(progress_file(snap_dir), height.to_string());
}

/// Load (or create) the on-disk search index for this node's data dir.
/// Call once at startup, before spawning the indexer, so `AppState.search`
/// can be constructed with whatever was already indexed as of last shutdown.
pub fn load_or_new(snap_dir: &Path) -> SearchEngine {
    let dir = search_dir(snap_dir);
    let _ = std::fs::create_dir_all(&dir);
    SearchEngine::load_or_new(&index_file(snap_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_events::SigilEvent;
    use sigil_state::{WalletId, NATIVE};


    /// REGRESSION — the 2026-08-26 livelock. The catch-up used an UNBOUNDED
    /// `replay_from` and wrote its progress file only after that one pass
    /// returned. With a ~209k-block backlog the pass never returned before the
    /// cgroup memory ceiling killed the process, so progress stayed pinned at
    /// its three-day-old value and every restart replayed the identical doomed
    /// range — block production starved for hours.
    ///
    /// This drives the same `replay_range` + `write_progress` sequence
    /// `spawn_indexer` now uses, and kills the run mid-catch-up (a fresh engine
    /// + a fresh read of the progress file is exactly what the next boot sees).
    /// The assertion that matters is `from2 == 5`: it RESUMES. Under the old
    /// code the equivalent value was always the same starting height, forever.
    #[test]
    fn interrupted_catchup_resumes_instead_of_restarting() {
        let dir = std::env::temp_dir()
            .join(format!("sigil-search-resume-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = load_or_new(&dir);

        // Heights 1..=9. (`__test_chain` only produces 1..=n; the height-0
        // genesis case has its own dedicated test above.)
        {
            let mut log = crate::chain_log::ChainLog::open(&dir).unwrap();
            for h in 1..=9u64 {
                log.append(&block_with_send(h, ALICE, BOB, 10 + h as u128)).unwrap();
            }
        }

        const BATCH: u64 = 4;

        // ── Batch 1 ─────────────────────────────────────────────────────────
        let from1 = next_height_to_index(&dir);
        assert_eq!(from1, 0, "a fresh index must start at 0, not 1");
        let mut engine1 = SearchEngine::new();
        let mut last1 = from1.saturating_sub(1);
        let n1 = crate::chain_log::ChainLog::replay_range(
            &dir,
            from1,
            from1 + BATCH - 1,
            |b| {
                last1 = b.header.height;
                engine1.index_document(to_document(&b));
            },
        )
        .unwrap();
        assert_eq!(
            n1, 3,
            "window [0,3] must yield exactly heights 1..=3. Nine blocks are on disk, so an \
             UNBOUNDED replay — the old code — would return 9 here. That difference IS the fix."
        );
        assert_eq!(last1, 3);
        write_progress(&dir, last1);
        assert_eq!(
            read_progress(&dir),
            Some(3),
            "progress must be checkpointed after EVERY batch — this is the fix"
        );

        // ── Process dies here (the OOM kill) ────────────────────────────────
        let mut engine2 = SearchEngine::new();
        let from2 = next_height_to_index(&dir);
        assert_eq!(
            from2, 4,
            "must RESUME at 4; restarting at 0 is the livelock this test exists for"
        );

        // ── Batch 2 ─────────────────────────────────────────────────────────
        let mut last2 = from2.saturating_sub(1);
        let n2 = crate::chain_log::ChainLog::replay_range(
            &dir,
            from2,
            from2 + BATCH - 1,
            |b| {
                last2 = b.header.height;
                engine2.index_document(to_document(&b));
            },
        )
        .unwrap();
        assert_eq!(n2, BATCH, "second window also bounded");
        assert_eq!(last2, 7);
        write_progress(&dir, last2);

        // ── Batch 3: short batch is how the loop learns it is caught up ─────
        let from3 = next_height_to_index(&dir);
        assert_eq!(from3, 8);
        let mut last3 = from3.saturating_sub(1);
        let n3 = crate::chain_log::ChainLog::replay_range(
            &dir,
            from3,
            from3 + BATCH - 1,
            |b| {
                last3 = b.header.height;
                engine2.index_document(to_document(&b));
            },
        )
        .unwrap();
        assert_eq!(n3, 2, "only heights 8 and 9 are left");
        assert!(n3 < BATCH, "a short batch signals caught-up, ending the fast loop");
        write_progress(&dir, last3);
        assert_eq!(read_progress(&dir), Some(9));

        // Nothing left: an empty batch must not move progress backwards.
        let from4 = next_height_to_index(&dir);
        let n4 = crate::chain_log::ChainLog::replay_range(&dir, from4, from4 + BATCH - 1, |_| {})
            .unwrap();
        assert_eq!(n4, 0);
        assert_eq!(read_progress(&dir), Some(9), "an empty pass leaves progress intact");

        let _ = std::fs::remove_dir_all(&dir);
    }

    const ALICE: WalletId = [0xD7u8; 32]; // arbitrary, just needs to be non-zero/distinct
    const BOB: WalletId = [0x42u8; 32];

    fn block_with_send(height: u64, from: WalletId, to: WalletId, amount: u128) -> Block {
        let mut blocks = crate::block::__test_chain(height);
        let mut b = blocks.pop().unwrap();
        b.events = vec![SigilEvent::Send { from, to, amount, token: NATIVE, fee: 0 }];
        b
    }

    /// The core correctness claim of `hexify_byte32_arrays`: a raw `[u8;32]`
    /// address, which serde would otherwise emit as `[215,215,...]`, must
    /// come out of `to_document` as the actual hex string a user would type
    /// or paste — and be found by `literal_search` on that exact string.
    #[test]
    fn address_is_hex_searchable_not_number_array() {
        let b = block_with_send(1, ALICE, BOB, 12345);
        let doc = to_document(&b);
        let alice_hex = hex::encode(ALICE);
        assert!(
            doc.content.contains(&alice_hex),
            "document content should contain the hex address, got: {}",
            doc.content
        );
        assert!(
            !doc.content.contains("215,215,215"),
            "document content should NOT contain the raw byte-array form"
        );

        let mut engine = SearchEngine::new();
        engine.index_document(doc);
        let resp = engine.literal_search(&alice_hex, 1, 10, false);
        assert_eq!(resp.total_results, 1, "literal_search on the hex address should find the block");
        assert_eq!(resp.results[0].url, "sigil://block/1");
    }

    /// End-to-end: append real blocks to a real ChainLog, run the same
    /// replay_from + index_document path spawn_indexer uses, confirm the
    /// resulting index is searchable, then verify a SECOND catch-up pass
    /// (simulating the next poll tick) only picks up the NEW block and
    /// doesn't re-index or duplicate anything already indexed.
    #[test]
    fn chain_log_backfill_then_incremental_catchup() {
        let dir = std::env::temp_dir().join(format!("sigil-search-idx-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = load_or_new(&dir); // creates search_dir(&dir), matching real startup

        {
            let mut log = crate::chain_log::ChainLog::open(&dir).unwrap();
            log.append(&block_with_send(1, ALICE, BOB, 100)).unwrap();
            log.append(&block_with_send(2, BOB, ALICE, 50)).unwrap();
        }

        // First pass: backfill from height 1 (progress file doesn't exist yet).
        let mut engine = SearchEngine::new();
        let mut last_seen = 0u64;
        let applied = crate::chain_log::ChainLog::replay_from(&dir, 1, |b| {
            last_seen = b.header.height;
            engine.index_document(to_document(&b));
        })
        .unwrap();
        assert_eq!(applied, 2, "both blocks should be replayed on first pass");
        assert_eq!(last_seen, 2);
        write_progress(&dir, last_seen);
        assert_eq!(read_progress(&dir), Some(2));

        let alice_hex = hex::encode(ALICE);
        let resp = engine.literal_search(&alice_hex, 1, 10, false);
        assert_eq!(resp.total_results, 2, "ALICE appears as sender in block 1 and recipient in block 2");

        // Append a third block — simulates the live producer committing while
        // the indexer is between poll ticks.
        {
            let mut log = crate::chain_log::ChainLog::open(&dir).unwrap();
            log.append(&block_with_send(3, ALICE, ALICE, 7)).unwrap();
        }

        // Second pass ("next poll tick"): must start from progress+1 = 3, and
        // must NOT re-see blocks 1 or 2.
        let from = next_height_to_index(&dir);
        assert_eq!(from, 3);
        let mut seen_heights = Vec::new();
        let applied2 = crate::chain_log::ChainLog::replay_from(&dir, from, |b| {
            seen_heights.push(b.header.height);
            engine.index_document(to_document(&b));
        })
        .unwrap();
        assert_eq!(applied2, 1, "incremental catch-up should see exactly the one new block");
        assert_eq!(seen_heights, vec![3]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression test for the exact bug found live on happysrv (2026-08-20):
    /// a real chain's genesis block sits at height 0, and a naive `last+1`
    /// with a `u64` progress marker defaulting to 0 would compute `from = 1`
    /// on a totally fresh node — permanently skipping height 0, silently,
    /// forever. Pins `next_height_to_index` returning 0 (not 1) when nothing
    /// has been indexed yet, and confirms a real height-0 block gets indexed.
    #[test]
    fn genesis_at_height_zero_is_not_skipped_on_a_fresh_node() {
        let dir = std::env::temp_dir().join(format!("sigil-search-idx-genesis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = load_or_new(&dir);

        assert_eq!(read_progress(&dir), None, "a fresh node has no progress marker at all");
        assert_eq!(next_height_to_index(&dir), 0, "a fresh node must start indexing AT height 0, not 1");

        // Build a real height-0 block (genesis) — __test_chain only produces
        // 1..=n, so start from its height-1 block and relabel it, matching
        // the real chain's actual shape: genesis is a normal Block, just at
        // height 0, carrying a real MintReward event (the exact shape seen
        // live on happysrv's chain.log).
        let mut genesis = crate::block::__test_chain(1).pop().unwrap();
        genesis.header.height = 0;
        genesis.transition.at_height = 0;
        genesis.events = vec![SigilEvent::MintReward { miner: ALICE, height: 0, amount: 1_000_000 }];

        {
            let mut log = crate::chain_log::ChainLog::open(&dir).unwrap();
            log.append(&genesis).unwrap();
        }

        let from = next_height_to_index(&dir);
        assert_eq!(from, 0);
        let mut engine = SearchEngine::new();
        let mut last_seen = 0u64;
        let mut saw_height_0 = false;
        let applied = crate::chain_log::ChainLog::replay_from(&dir, from, |b| {
            if b.header.height == 0 { saw_height_0 = true; }
            last_seen = b.header.height;
            engine.index_document(to_document(&b));
        })
        .unwrap();
        assert_eq!(applied, 1, "the genesis block must actually be replayed");
        assert!(saw_height_0, "the genesis block (height 0) must not be skipped");
        write_progress(&dir, last_seen);
        assert_eq!(read_progress(&dir), Some(0), "progress after indexing ONLY genesis is 0, not skipped-to-1");

        let resp = engine.literal_search("MintReward", 1, 10, false);
        assert_eq!(resp.total_results, 1, "genesis's MintReward event must be searchable");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Blocks indexed per checkpointed catch-up batch
/// (`SIGIL_SEARCH_BATCH`, default 25_000). Every batch ends with the index
/// saved and progress written, so an interrupted catch-up resumes from the
/// last batch instead of starting over.
fn catchup_batch() -> u64 {
    std::env::var("SIGIL_SEARCH_BATCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(25_000)
}

/// Soft RSS ceiling for the whole process, in MiB
/// (`SIGIL_SEARCH_RSS_MAX_MB`, default 6144). Above it the catch-up pauses.
fn rss_ceiling_mb() -> u64 {
    std::env::var("SIGIL_SEARCH_RSS_MAX_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(6144)
}

/// Resident set size of THIS process in MiB, from `/proc/self/statm` (field 2
/// is resident pages). `None` when unreadable — callers must treat that as
/// "cannot measure, do not throttle" rather than as zero, so a missing /proc
/// never silently disables the catch-up.
fn self_rss_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = 4096u64; // x86_64/aarch64 default; only used as a soft bound
    Some(pages.saturating_mul(page_size) / (1024 * 1024))
}

/// Pause between back-to-back catch-up batches. Small enough that a long
/// backlog still drains promptly, non-zero so the producer's tasks always get
/// the runtime back between batches.
const CATCHUP_YIELD: Duration = Duration::from_millis(200);

/// Spawn the background indexer: catches up from wherever it left off
/// (`indexed-to-height`, 0 on a fresh index — a full historical backfill,
/// since `ChainLog` retains the whole chain), then polls `ChainLog` every
/// `poll_every` for newly-committed blocks. Saves the index to disk after
/// every catch-up batch. Never touches block-application code — purely a
/// reader of the same durable log the live producer already writes to.
pub fn spawn_indexer(
    snap_dir: PathBuf,
    engine: Arc<Mutex<SearchEngine>>,
    poll_every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let dir_for_blocking = snap_dir.clone();
            let engine_for_blocking = Arc::clone(&engine);
            let (indexed, now_at, batch) = tokio::task::spawn_blocking(move || -> (u64, u64, u64) {
                let batch = catchup_batch();
                let from = next_height_to_index(&dir_for_blocking);
                let mut last_seen = from.saturating_sub(1);

                // Memory backstop. The engine holds the whole index in RAM, so a
                // long catch-up grows RSS monotonically; this process shares its
                // cgroup ceiling with block production. Pausing (progress already
                // checkpointed) degrades search gracefully instead of taking the
                // producer down with it.
                if let Some(rss) = self_rss_mb() {
                    let cap = rss_ceiling_mb();
                    if rss >= cap {
                        eprintln!(
                            "🔎 search index: PAUSED at height {last_seen} — RSS {rss} MiB >= SIGIL_SEARCH_RSS_MAX_MB {cap}"
                        );
                        return (0, last_seen, batch);
                    }
                }

                // BOUNDED window. Never an open-ended replay to EOF: see
                // `ChainLog::replay_range`'s doc for the livelock that caused.
                let to = from.saturating_add(batch.saturating_sub(1));
                let applied = crate::chain_log::ChainLog::replay_range(
                    &dir_for_blocking,
                    from,
                    to,
                    |b| {
                        last_seen = b.header.height;
                        let doc = to_document(&b);
                        if let Ok(mut eng) = engine_for_blocking.lock() {
                            eng.index_document(doc);
                        }
                    },
                )
                .unwrap_or_else(|e| {
                    // Was `.unwrap_or(0)` — a silent 0. Failing invisibly is how
                    // this stayed unnoticed for three days.
                    eprintln!("🔎 search index: replay_range({from}..={to}) failed: {e}");
                    0
                });

                if applied > 0 {
                    // Index is saved BEFORE progress is written, so the progress
                    // file can never claim more than what is durably in the index.
                    // A crash between the two costs a re-index of one batch, never
                    // a silent hole.
                    if let Ok(mut eng) = engine_for_blocking.lock() {
                        eng.recalculate_pagerank();
                        let _ = eng.save_to_path(&index_file(&dir_for_blocking));
                    }
                    write_progress(&dir_for_blocking, last_seen);
                }
                (applied, last_seen, batch)
            })
            .await
            .unwrap_or((0, 0, 1));

            if indexed > 0 {
                eprintln!(
                    "🔎 search index: indexed {indexed} new block(s), now at height {now_at} (checkpointed)"
                );
            }
            // A full batch means we are still behind: keep going, but yield first
            // so the catch-up can never monopolise the runtime the producer needs.
            // A short batch means we are caught up — back to the normal poll.
            if indexed >= batch {
                tokio::time::sleep(CATCHUP_YIELD).await;
            } else {
                tokio::time::sleep(poll_every).await;
            }
        }
    })
}
