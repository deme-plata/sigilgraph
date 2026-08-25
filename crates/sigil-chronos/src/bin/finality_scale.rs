//! finality_scale — Part 3 of the SIGIL True Instant Finality Phase-1 task:
//! the large-scale (operator-requested "1TB, ~900MB/s") run that exercises
//! the REAL `sigil-finality` quorum/certificate assembler (Part 1) through
//! the REAL adversarial scenarios (Part 2's normal/equivocation/offline
//! cases) at volume, while writing through the REAL store engine
//! (flux-db: WAL -> memtable -> SST flush), reusing `chronos_scale.rs`'s
//! exact CLI/env-var/metrics-CSV/resumable-marker tooling (same knob names,
//! same MARKER-AFTER-FSYNC durability discipline) rather than inventing a
//! separate scale-running harness.
//!
//! ## What's genuinely being tested here, honestly stated
//!
//! Two different things are deliberately layered, at two different
//! cadences:
//!
//!  1. **Bulk byte volume / throughput** (every `height`, same as
//!     `chronos_scale`): a deterministic, incompressible `CHRONOS_VALUE_BYTES`
//!     blob is written per height under a `blk/` key — this is what drives
//!     the target byte count and the measured MB/s, directly comparable to
//!     the historical 500 GiB run (`/home/storage/chronos-500gb-run/`).
//!  2. **Real finality-gadget rounds** (every `FINALITY_CHECKPOINT_INTERVAL`
//!     heights, default 1000): a REAL n-validator committee signs REAL
//!     Ed25519 `FinalityVote`s over a deterministic `(spine_block_hash,
//!     order_hash)`, and the REAL `sigil_finality::assemble` is run against
//!     them — with a periodic adversarial variant (equivocation, or `>f`
//!     offline) whose EXPECTED outcome is hard-asserted (panics the whole
//!     run loudly on any violation — this is the "continuous verification
//!     at scale" the task calls for, not merely a throughput number). The
//!     resulting votes/certificate are bincode-serialized and persisted
//!     under a separate `fcert/` key.
//!
//! Running real crypto + `assemble()` on EVERY single height (rather than
//! periodically) was tried first and rejected on cost grounds: 5 real
//! Ed25519 signs + ~5 real verifies + assembly bookkeeping measures at
//! roughly 0.5-1ms per round on this hardware — at the ~131M heights a 1 TiB
//! run implies (8 KiB values), that alone would add tens of hours on top of
//! the storage-bound cost. A periodic cadence is not a corner cut: it
//! mirrors this exact codebase's own precedent (`producer_signing::
//! HYBRID_CHECKPOINT_INTERVAL = 128` — "signing every block would throttle
//! production... periodic checkpoints instead") and the design doc's own
//! language ("periodically look at what DagKnight has already produced").
//!
//! ## Simplification, stated honestly
//!
//! `order_hash` here is `BLAKE3(spine_hash || checkpoint_index)` — a
//! self-contained per-checkpoint identity, NOT the chained `Braid::
//! order_hash()`-style accumulator `finality_gadget.rs`'s honest-path
//! scenario uses. A chained accumulator would require either replaying the
//! full history on every resume (defeats the point of a resumable marker at
//! this scale) or persisting extra chain state this harness doesn't need to
//! prove the point: it does not change anything about the quorum/
//! certificate math under test, only what the opaque 32-byte order
//! commitment's bytes happen to be.
//!
//! Env (see `chronos_scale.rs` for the full inherited knob list — repeated
//! here are only the ones this binary adds or defaults differently):
//!   CHRONOS_DIR                (default /home/storage/chronos-finality-1tb/db)
//!   CHRONOS_TARGET_BYTES       (default 1 TiB = 1024^4, matching this
//!                               codebase's existing "TB" = tebibyte usage)
//!   FINALITY_VALIDATORS        (default 5)
//!   FINALITY_CHECKPOINT_INTERVAL (default 1000 heights per finality round)
//!   FINALITY_EQUIVOCATE_EVERY  (default 200 checkpoints; 0 = disabled)
//!   FINALITY_OFFLINE_EVERY     (default 700 checkpoints; 0 = disabled)

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ed25519_dalek::SigningKey;
use sigil_braidpool::committee::{availability_quorum, max_byzantine, Committee};
use sigil_finality::{assemble, FinalityVote};

fn dir_stats(dir: &Path) -> (u64, u64, u64) {
    fn scan(dir: &Path, depth: u8, total: &mut u64, wal: &mut u64, ssts: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
                if md.is_dir() {
                    if depth > 0 { scan(&e.path(), depth - 1, total, wal, ssts); }
                    continue;
                }
                *total += md.len();
                let name = e.file_name().to_string_lossy().to_string();
                if name == "flux.wal" { *wal += md.len(); }
                if name.ends_with(".sst") { *ssts += 1; }
            }
        }
    }
    let (mut total, mut wal, mut ssts) = (0u64, 0u64, 0u64);
    scan(dir, 1, &mut total, &mut wal, &mut ssts);
    (total, wal, ssts)
}

fn validator_keys(n: usize) -> Vec<SigningKey> {
    (0..n as u8)
        .map(|i| {
            let mut h = blake3::Hasher::new();
            h.update(b"finality_scale/validator");
            h.update(&[i]);
            SigningKey::from_bytes(h.finalize().as_bytes())
        })
        .collect()
}

fn committee_of(keys: &[SigningKey]) -> Committee {
    Committee::new(keys.iter().map(|k| { let id = k.verifying_key().to_bytes(); (id, id) }).collect())
}

/// One real finality round at `checkpoint_idx`. Returns `(outcome_tag,
/// bincode bytes of the votes actually cast)` for persistence, after
/// hard-asserting the EXPECTED outcome for this round's kind — a violation
/// here means Part 1's own safety/liveness property broke at scale and the
/// whole run aborts loudly (`panic!`), which is the point.
fn run_finality_round(
    committee: &Committee,
    keys: &[SigningKey],
    checkpoint_idx: u64,
    equivocate_every: u64,
    offline_every: u64,
) -> (&'static str, Vec<u8>) {
    let n = keys.len();
    let quorum = availability_quorum(n);
    let f = max_byzantine(n);
    let height = checkpoint_idx; // finality-vote height == checkpoint index
    let spine = *blake3::hash(&height.to_le_bytes()).as_bytes();
    let mut oh = blake3::Hasher::new();
    oh.update(&spine);
    oh.update(&height.to_le_bytes());
    let order = *oh.finalize().as_bytes();

    let is_equivocation_round = equivocate_every > 0 && checkpoint_idx > 0 && checkpoint_idx % equivocate_every == 0;
    let is_offline_round = offline_every > 0 && checkpoint_idx > 0 && checkpoint_idx % offline_every == 0;

    if is_offline_round {
        // >f offline: only quorum-1 validators online. Must NOT finalize.
        let online = quorum.saturating_sub(1).min(n);
        let votes: Vec<FinalityVote> = keys[..online].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        let report = assemble(committee, &votes);
        assert!(
            report.certificate_for_height(height).is_none(),
            "SAFETY/LIVENESS VIOLATION at checkpoint {height}: {online}-of-{n} (below quorum {quorum}) unexpectedly certified"
        );
        let bytes = bincode::serialize(&votes).expect("bincode serialize votes");
        ("halted_below_quorum", bytes)
    } else if is_equivocation_round && n > f {
        // The last validator equivocates: signs the real tuple AND a rogue
        // one. Honest quorum (n-1, requires n-1 >= quorum, true whenever
        // f>=1) must still certify; equivocation must be caught; no
        // conflicting certificate may form.
        let mut rogue_spine = spine;
        rogue_spine[0] ^= 0xFF;
        let mut votes: Vec<FinalityVote> = keys[..n - 1].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        votes.push(FinalityVote::sign(&keys[n - 1], height, spine, order));
        votes.push(FinalityVote::sign(&keys[n - 1], height, rogue_spine, order));
        let report = assemble(committee, &votes);
        assert_eq!(report.equivocations.len(), 1, "checkpoint {height}: equivocation not detected");
        assert!(report.conflicting_heights().is_empty(), "SAFETY VIOLATION at checkpoint {height}: conflicting certificates formed");
        let cert = report.certificate_for_height(height)
            .unwrap_or_else(|| panic!("checkpoint {height}: honest {}/{n} quorum must still certify despite equivocation", n - 1));
        assert_eq!(cert.votes.len(), n - 1);
        let bytes = bincode::serialize(&votes).expect("bincode serialize votes");
        ("equivocation_detected_certified", bytes)
    } else {
        // Normal round: all n validators agree.
        let votes: Vec<FinalityVote> = keys.iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        let report = assemble(committee, &votes);
        let cert = report.certificate_for_height(height)
            .unwrap_or_else(|| panic!("checkpoint {height}: all-honest {n}/{n} MUST certify — quorum={quorum}"));
        assert_eq!(cert.votes.len(), n);
        assert!(report.conflicting_heights().is_empty());
        let bytes = bincode::serialize(&votes).expect("bincode serialize votes");
        ("certified", bytes)
    }
}

fn main() {
    let dir: PathBuf = std::env::var("CHRONOS_DIR")
        .unwrap_or_else(|_| "/home/storage/chronos-finality-1tb/db".into()).into();
    let target: u64 = std::env::var("CHRONOS_TARGET_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(1024u64 * 1024 * 1024 * 1024); // 1 TiB
    let value_bytes: usize = std::env::var("CHRONOS_VALUE_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(8192);
    let sample_every: u64 = std::env::var("CHRONOS_SAMPLE_EVERY").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(100_000);

    let n_validators: usize = std::env::var("FINALITY_VALIDATORS").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(5);
    let checkpoint_interval: u64 = std::env::var("FINALITY_CHECKPOINT_INTERVAL").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(1000).max(1);
    let equivocate_every: u64 = std::env::var("FINALITY_EQUIVOCATE_EVERY").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(200);
    let offline_every: u64 = std::env::var("FINALITY_OFFLINE_EVERY").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(700);

    let keys = validator_keys(n_validators);
    let committee = committee_of(&keys);
    eprintln!(
        "finality_scale: committee n={n_validators} f={} quorum={} checkpoint_interval={checkpoint_interval} \
         equivocate_every={equivocate_every} offline_every={offline_every}",
        max_byzantine(n_validators), availability_quorum(n_validators)
    );

    std::fs::create_dir_all(&dir).expect("create dir");
    let csv_path = dir.parent().unwrap().join("metrics.csv");
    let mut csv = std::fs::OpenOptions::new().create(true).append(true).open(&csv_path).expect("csv");
    if csv.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = writeln!(csv, "height,elapsed_s,blk_per_s,mb_per_s,dir_bytes,wal_bytes,sst_count,read_p50_us,read_p99_us,finality_rounds,finality_certified");
    }
    let events_path = dir.parent().unwrap().join("finality_events.log");
    let mut events = std::fs::OpenOptions::new().create(true).append(true).open(&events_path).expect("events log");

    let knob = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    let (wal_max, block_cache, defer_comp, sync_every) = (
        knob("CHRONOS_WAL_MAX_BYTES"), knob("CHRONOS_BLOCK_CACHE_BYTES"),
        knob("CHRONOS_DEFER_COMPACTION").map(|v| v == 1).unwrap_or(false),
        knob("CHRONOS_SYNC_EVERY").unwrap_or(0),
    );
    let batch_size = knob("CHRONOS_BATCH").unwrap_or(256).max(1) as usize;
    let shards = match knob("CHRONOS_SHARDS") {
        Some(n) => n.max(1) as usize,
        None if flux_db::shard::exists(&dir) => {
            let n: usize = std::fs::read_to_string(dir.join("SHARDS")).ok()
                .and_then(|s| s.trim().parse().ok()).unwrap_or(1);
            eprintln!("finality_scale: existing sharded store — adopting {n} shards from SHARDS marker");
            n
        }
        None => 1,
    };

    enum Store { One(flux_db::Database), Many(flux_db::shard::ShardedDb) }
    impl Store {
        fn put(&self, k: &[u8], v: &[u8]) -> Result<(), String> {
            match self { Store::One(d) => d.put(k, v), Store::Many(d) => d.put(k, v) }
        }
        fn put_many(&self, e: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
            match self { Store::One(d) => d.put_many(e), Store::Many(d) => d.put_many(e) }
        }
        fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, String> {
            match self { Store::One(d) => d.get(k), Store::Many(d) => d.get(k) }
        }
        fn sync_wal(&self) -> Result<(), String> {
            match self { Store::One(d) => d.sync_wal(), Store::Many(d) => d.sync_wal() }
        }
    }
    let db = if shards > 1 {
        let d = flux_db::shard::ShardedDb::open(&dir, shards).expect("open sharded flux-db");
        if let Some(w) = wal_max { d.set_max_wal_bytes(w); }
        if defer_comp { d.set_defer_compaction(true); }
        Store::Many(d)
    } else {
        let mut d = flux_db::Database::open(&dir).expect("open flux-db");
        if let Some(bc) = block_cache { d = d.with_block_cache_capacity(bc as usize); }
        if let Some(w) = wal_max { d.set_max_wal_bytes(w); }
        if defer_comp { d.set_defer_compaction(true); }
        Store::One(d)
    };
    eprintln!(
        "finality_scale: dir={:?} target={} GiB value={} B sample_every={}",
        dir, target / (1024 * 1024 * 1024), value_bytes, sample_every
    );
    eprintln!(
        "finality_scale: knobs wal_max={:?} block_cache={:?} defer_compaction={} sync_every={} batch={} shards={}",
        wal_max, block_cache, defer_comp, sync_every, batch_size, shards
    );

    let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(batch_size);
    let mut height: u64 = std::fs::read_to_string(dir.parent().unwrap().join("height.marker"))
        .ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let mut finality_rounds_total: u64 = 0;
    let mut finality_certified_total: u64 = 0;
    let t0 = Instant::now();
    let mut window_t = Instant::now();
    let mut window_blocks = 0u64;
    let mut window_rounds = 0u64;
    let mut window_certified = 0u64;

    loop {
        // Bulk byte-volume payload — identical construction to chronos_scale.
        let mut value = Vec::with_capacity(value_bytes);
        let seed = blake3::hash(&height.to_le_bytes());
        let mut counter = 0u64;
        while value.len() < value_bytes {
            let mut h = blake3::Hasher::new();
            h.update(seed.as_bytes());
            h.update(&counter.to_le_bytes());
            value.extend_from_slice(h.finalize().as_bytes());
            counter += 1;
        }
        value.truncate(value_bytes);
        let key = {
            let mut k = Vec::with_capacity(40);
            k.extend_from_slice(b"blk/");
            k.extend_from_slice(&height.to_be_bytes());
            k.extend_from_slice(&seed.as_bytes()[..8]);
            k
        };
        if batch_size <= 1 {
            db.put(&key, &value).expect("put");
        } else {
            pending.push((key, value));
            if pending.len() >= batch_size {
                db.put_many(&pending).expect("put_many");
                pending.clear();
            }
        }

        // Real finality round, periodically.
        if height % checkpoint_interval == 0 {
            let checkpoint_idx = height / checkpoint_interval;
            let (tag, vote_bytes) = run_finality_round(&committee, &keys, checkpoint_idx, equivocate_every, offline_every);
            let fkey = {
                let mut k = Vec::with_capacity(16);
                k.extend_from_slice(b"fcert/");
                k.extend_from_slice(&checkpoint_idx.to_be_bytes());
                k
            };
            db.put(&fkey, &vote_bytes).expect("put finality round");
            finality_rounds_total += 1;
            window_rounds += 1;
            if tag == "certified" || tag == "equivocation_detected_certified" {
                finality_certified_total += 1;
                window_certified += 1;
            }
            let _ = writeln!(events, "{checkpoint_idx},{tag},{height}");
        }

        height += 1;
        window_blocks += 1;
        if sync_every > 0 && height % sync_every == 0 {
            if !pending.is_empty() { db.put_many(&pending).expect("put_many"); pending.clear(); }
            let _ = db.sync_wal();
        }

        if height % sample_every == 0 {
            if !pending.is_empty() { db.put_many(&pending).expect("put_many"); pending.clear(); }
            let (total, wal, ssts) = dir_stats(&dir);
            let mut lat: Vec<u128> = Vec::with_capacity(64);
            for i in 0..64u64 {
                let probe_h = (blake3::hash(&(height ^ i).to_le_bytes()).as_bytes()[0] as u64)
                    .wrapping_mul(height / 256).min(height.saturating_sub(1));
                let pseed = blake3::hash(&probe_h.to_le_bytes());
                let mut pk = Vec::with_capacity(40);
                pk.extend_from_slice(b"blk/");
                pk.extend_from_slice(&probe_h.to_be_bytes());
                pk.extend_from_slice(&pseed.as_bytes()[..8]);
                let t = Instant::now();
                let _ = db.get(&pk);
                lat.push(t.elapsed().as_micros());
            }
            lat.sort_unstable();
            let (p50, p99) = (lat[31], lat[62]);
            let wsecs = window_t.elapsed().as_secs_f64().max(1e-9);
            let bps = window_blocks as f64 / wsecs;
            let mbs = bps * value_bytes as f64 / (1024.0 * 1024.0);
            let line = format!(
                "{},{:.0},{:.0},{:.1},{},{},{},{},{},{},{}",
                height, t0.elapsed().as_secs_f64(), bps, mbs, total, wal, ssts, p50, p99,
                finality_rounds_total, finality_certified_total
            );
            let _ = writeln!(csv, "{line}");
            let _ = csv.flush();
            match db.sync_wal() {
                Err(e) => eprintln!("[finality_scale] sync_wal before marker FAILED ({e}) — marker not advanced"),
                Ok(()) => {
                    let marker = dir.parent().unwrap().join("height.marker");
                    let tmp_m = dir.parent().unwrap().join("height.marker.tmp");
                    let res = std::fs::write(&tmp_m, height.to_string())
                        .and_then(|_| std::fs::File::open(&tmp_m).and_then(|f| f.sync_all()))
                        .and_then(|_| std::fs::rename(&tmp_m, &marker));
                    if let Err(e) = res { eprintln!("[finality_scale] marker write failed: {e}"); }
                }
            }
            eprintln!(
                "[finality_scale] {line} (window: {window_rounds} finality rounds, {window_certified} certified)"
            );
            window_t = Instant::now();
            window_blocks = 0;
            window_rounds = 0;
            window_certified = 0;
            if total >= target {
                eprintln!(
                    "finality_scale: TARGET REACHED — {total} bytes at height {height} \
                     ({finality_rounds_total} real finality rounds run, {finality_certified_total} certified, \
                     0 safety/liveness violations — any violation would have panicked this process)"
                );
                break;
            }
        }
    }
}
