//! repair_chain_log — de-duplicate a `chain.log` that has been REWOUND.
//!
//! THE DAMAGE THIS REPAIRS (measured live on Epsilon, 2026-08-26):
//! `chain.log` is an append-only log every consumer assumes is linear and
//! height-ordered. A boot that resumed appending from a stale offset instead
//! of the true tip made it rewind: at record #2,173,502 the height dropped
//! from 2,172,423 back to 2,143,692 and re-wrote ~28.7k blocks a second time
//! — with DIFFERENT `parent_hash` values, i.e. a competing fork branch
//! stitched in after the original rather than replacing it. Totals: 2,229,689
//! records / 2,199,880 distinct heights / 29,809 duplicates / 2 rewind points.
//!
//! Downstream effects, both observed live:
//!   * `get_range_by_height(from, ..)` seeks via `chain.idx` then skips
//!     `height < from`. Which duplicate the seek lands on decides the answer,
//!     so the SAME request returned 8,193 records once and 2 the next time.
//!     A syncing client could never assemble a contiguous parent-linked chain
//!     past the rewind: `sigil-top` sat at `synced=2,172,424` for over an hour
//!     while fetching 19,047,587 blocks (8.7x the whole chain) to advance zero.
//!   * `ChainLog::open()` sets `height() == offsets.len()`, one entry per
//!     RECORD, so duplicates make the node over-report its own height by
//!     29,810.
//!
//! HOW IT DECIDES WHICH COPY IS CANONICAL — it does not guess. It walks
//! BACKWARD from the tip following `parent_hash`, and at each height keeps the
//! record whose `Block::hash()` equals the child's `parent_hash`. That is the
//! chain the current tip actually descends from, by the node's own hash
//! function (`sigil_header`'s BLAKE3 canonicalization) rather than a
//! reimplementation. Orphaned branch copies simply never match and are
//! dropped.
//!
//! SAFETY: a DRY RUN by default — it opens the log read-only and writes
//! nothing. `--apply` writes a NEW file (`chain.log.repaired`) and still never
//! touches the live `chain.log`; swapping it in is a deliberate, separate,
//! operator step taken with the node STOPPED. Records are copied as their
//! original RAW bytes (length prefix + payload), never re-serialized, so
//! surviving blocks are byte-identical and every hash/signature still verifies.
//!
//! Usage:
//!   repair_chain_log <chain_log_dir> [--apply] [--fast] [--out <path>]
//!     --apply   also write the repaired log (default: report only)
//!     --fast    only hash where a height has >1 candidate (default: hash
//!               every height, i.e. verify the entire chain's linkage)

use sigil_node::block::Block;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Where one record lives in the file. `len` is the payload length, i.e. what
/// the 4-byte little-endian prefix declares.
#[derive(Clone, Copy)]
struct Rec {
    off: u64,
    len: u32,
}

/// Minimal shape for pass 1 — serde ignores the rest of the block, so this
/// avoids paying for a full `Block` decode just to learn the height.
#[derive(serde::Deserialize)]
struct HeightProbe {
    header: HeightOnly,
}
#[derive(serde::Deserialize)]
struct HeightOnly {
    height: u64,
}

fn read_payload(r: &mut BufReader<File>, rec: Rec) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(rec.off))?;
    let mut buf = vec![0u8; rec.len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn hex16(b: &[u8; 32]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: repair_chain_log <chain_log_dir> [--apply] [--fast] [--out <path>]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let apply = args.iter().any(|a| a == "--apply");
    let fast = args.iter().any(|a| a == "--fast");
    let out_path: PathBuf = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("chain.log.repaired"));

    let log_path = dir.join("chain.log");
    let Ok(file) = File::open(&log_path) else {
        eprintln!("cannot open {}", log_path.display());
        std::process::exit(1);
    };
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    println!("chain.log      : {} ({} bytes)", log_path.display(), total_bytes);
    println!("mode           : {}", if apply { "APPLY (writes a new file)" } else { "DRY RUN (writes nothing)" });
    println!(
        "linkage        : {}\n",
        if fast { "duplicated heights only (--fast)" } else { "EVERY height (full chain verification)" }
    );

    // ── PASS 1 — index every record by height ───────────────────────────────
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut by_height: HashMap<u64, Vec<Rec>> = HashMap::new();
    let mut pos = 0u64;
    let mut n_records = 0u64;
    let mut n_unparsed = 0u64;
    let mut max_height = 0u64;
    let mut prev_height: Option<u64> = None;
    let mut rewinds: Vec<(u64, u64, u64)> = Vec::new(); // (from, to, record_index)

    loop {
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() {
            break; // clean EOF
        }
        let len = u32::from_le_bytes(lb);
        let payload_off = pos + 4;
        let mut buf = vec![0u8; len as usize];
        if r.read_exact(&mut buf).is_err() {
            eprintln!("torn trailing record at offset {pos} — stopping scan there");
            break;
        }
        pos = payload_off + len as u64;
        n_records += 1;

        match serde_json::from_slice::<HeightProbe>(&buf) {
            Ok(p) => {
                let h = p.header.height;
                if h > max_height {
                    max_height = h;
                }
                if let Some(prev) = prev_height {
                    if h <= prev {
                        rewinds.push((prev, h, n_records));
                    }
                }
                prev_height = Some(h);
                by_height.entry(h).or_default().push(Rec { off: payload_off, len });
            }
            Err(_) => n_unparsed += 1,
        }
        if n_records % 250_000 == 0 {
            println!("  … scanned {n_records} records");
        }
    }

    let distinct = by_height.len() as u64;
    let dupes = n_records.saturating_sub(distinct).saturating_sub(n_unparsed);
    println!("\n── PASS 1 · scan ──");
    println!("records            : {n_records}");
    println!("distinct heights   : {distinct}");
    println!("max height         : {max_height}");
    println!("duplicate records  : {dupes}");
    println!("unparsable records : {n_unparsed}");
    println!("rewind points      : {}", rewinds.len());
    for (from, to, at) in rewinds.iter().take(10) {
        println!("   record #{at}: height {from} -> {to}  (back {})", from.saturating_sub(*to));
    }

    // Every height present at least once? A genuine HOLE cannot be repaired by
    // de-duplication — the missing blocks would have to be re-fetched.
    let mut missing: Vec<u64> = Vec::new();
    for h in 0..=max_height {
        if !by_height.contains_key(&h) {
            missing.push(h);
            if missing.len() >= 32 {
                break;
            }
        }
    }
    if !missing.is_empty() {
        println!("\n🚨 MISSING HEIGHTS (first {}): {:?}", missing.len(), missing);
        println!("   De-duplication CANNOT fix a hole. Aborting — these blocks must be re-fetched.");
        std::process::exit(1);
    }
    println!("coverage           : complete, 0..={max_height}, no holes");

    // ── PASS 2 — walk backward from the tip following parent_hash ───────────
    println!("\n── PASS 2 · canonical chain by parent-hash linkage ──");
    let mut r = BufReader::with_capacity(1 << 20, File::open(&log_path).expect("reopen"));

    // At the tip, prefer the LAST-written record: it is the one the producer
    // is currently building on.
    let tip_cands = by_height.get(&max_height).expect("tip present");
    let tip_rec = *tip_cands.last().expect("tip candidate");
    let tip_block: Block = {
        let buf = read_payload(&mut r, tip_rec).expect("read tip");
        serde_json::from_slice(&buf).expect("decode tip")
    };

    let mut chosen: Vec<Rec> = vec![Rec { off: 0, len: 0 }; (max_height + 1) as usize];
    chosen[max_height as usize] = tip_rec;
    let mut want_parent = tip_block.header.parent_hash;
    let mut resolved_dupes = 0u64;
    let mut hashed = 0u64;
    let mut break_at: Option<u64> = None;

    for h in (0..max_height).rev() {
        let cands = by_height.get(&h).expect("coverage checked above");
        let multi = cands.len() > 1;

        let mut picked: Option<(Rec, [u8; 32])> = None;
        if !multi && fast {
            // --fast: trust the sole candidate without hashing it.
            let rec = cands[0];
            let buf = read_payload(&mut r, rec).expect("read record");
            let b: Block = serde_json::from_slice(&buf).expect("decode record");
            picked = Some((rec, b.header.parent_hash));
        } else {
            for &rec in cands {
                let buf = read_payload(&mut r, rec).expect("read record");
                let b: Block = match serde_json::from_slice(&buf) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                hashed += 1;
                if b.hash() == want_parent {
                    picked = Some((rec, b.header.parent_hash));
                    break;
                }
            }
        }

        match picked {
            Some((rec, parent)) => {
                if multi {
                    resolved_dupes += 1;
                }
                chosen[h as usize] = rec;
                want_parent = parent;
            }
            None => {
                println!(
                    "🚨 LINKAGE BREAK at height {h}: none of the {} candidate(s) hashes to the \
                     child's parent_hash {}",
                    cands.len(),
                    hex16(&want_parent)
                );
                break_at = Some(h);
                break;
            }
        }

        if h % 250_000 == 0 {
            println!("  … walked back to height {h}");
        }
    }

    if let Some(h) = break_at {
        println!("\nRESULT: chain is NOT repairable by de-duplication alone — it breaks at {h}.");
        println!("The blocks below that point do not link to the current tip; they must be re-fetched.");
        std::process::exit(1);
    }

    println!("linkage            : ✅ VERIFIED contiguous 0..={max_height}");
    println!("blocks hashed      : {hashed}");
    println!("duplicate heights resolved by linkage: {resolved_dupes}");
    println!("records to drop    : {}", n_records.saturating_sub(max_height + 1));

    if !apply {
        println!("\nDRY RUN — nothing written. Re-run with --apply to produce {}", out_path.display());
        return;
    }

    // ── PASS 3 — write the repaired log (raw bytes, ascending height) ───────
    println!("\n── PASS 3 · writing {} ──", out_path.display());
    let mut w = BufWriter::with_capacity(1 << 20, File::create(&out_path).expect("create out"));
    let mut written = 0u64;
    let mut out_bytes = 0u64;
    for h in 0..=max_height {
        let rec = chosen[h as usize];
        let buf = read_payload(&mut r, rec).expect("read record");
        w.write_all(&rec.len.to_le_bytes()).expect("write len");
        w.write_all(&buf).expect("write payload");
        written += 1;
        out_bytes += 4 + buf.len() as u64;
        if h % 250_000 == 0 && h > 0 {
            println!("  … wrote {h}");
        }
    }
    w.flush().expect("flush");
    w.into_inner().expect("unwrap writer").sync_all().expect("fsync");

    println!("\nwrote              : {written} records, {out_bytes} bytes");
    println!("dropped            : {} duplicate records", n_records - written);
    println!("\n✅ Repaired log written. The live chain.log was NOT modified.");
    println!("To swap it in (node STOPPED, backup kept):");
    println!("   systemctl stop sigil-node");
    println!("   mv {}/chain.log {}/chain.log.corrupt-backup", dir.display(), dir.display());
    println!("   mv {} {}/chain.log", out_path.display(), dir.display());
    println!("   rm -f {}/chain.idx     # sparse index is rebuilt on next open", dir.display());
    println!("   systemctl start sigil-node");
}
