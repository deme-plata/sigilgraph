//! Measure candidate chain-log encodings against REAL blocks from a live `chain.log`.
//!
//! The chain log currently persists every block as `[u32 LE len][serde_json bytes]`, which
//! measured **4,214 bytes per block** on a chain carrying essentially zero transactions —
//! ~44× Bitcoin's bytes/day. That number is the thing standing between SIGIL and anyone
//! being willing to keep an archival copy, so before changing the settlement path we
//! measure rather than assume.
//!
//! Deliberately compares on the SAME real records, decoded once and re-encoded each way,
//! so the comparison cannot be skewed by different block populations.
//!
//! ```text
//! cargo run --release --example chainlog_codec_bench -- <chain.log> [max_blocks]
//! ```
//!
//! **Self-describing vs not is a first-class column here, not a footnote.** A ledger you
//! cannot decode in ten years is not durable, whatever it weighs. bincode is not
//! self-describing: any future field added to `Block` makes every previously written
//! record undecodable. MessagePack keeps field names, so an old record still round-trips
//! through a newer struct (with `#[serde(default)]`), exactly like JSON does today.

use std::io::{BufReader, Read};

use sigil_node::block::Block;

fn zstd_len(bytes: &[u8], level: i32) -> usize {
    zstd::encode_all(bytes, level).map(|v| v.len()).unwrap_or(0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: chainlog_codec_bench <chain.log> [max_blocks]");
        std::process::exit(2);
    });
    let max: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5_000);

    let f = std::fs::File::open(&path).expect("open chain.log");
    let mut r = BufReader::new(f);

    let (mut n, mut json_raw, mut json_zstd, mut mp_raw, mut mp_zstd, mut bc_raw, mut bc_zstd) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);

    let mut len_buf = [0u8; 4];
    while n < max {
        if r.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > 64 * 1024 * 1024 {
            break;
        }
        let mut rec = vec![0u8; len];
        if r.read_exact(&mut rec).is_err() {
            break;
        }
        // Only JSON records exist today; skip anything else rather than guess.
        let Ok(block) = serde_json::from_slice::<Block>(&rec) else {
            continue;
        };

        json_raw += rec.len();
        json_zstd += zstd_len(&rec, 3);

        let mp = rmp_serde::to_vec_named(&block).expect("msgpack encode");
        mp_raw += mp.len();
        mp_zstd += zstd_len(&mp, 3);

        let bc = bincode::serialize(&block).expect("bincode encode");
        bc_raw += bc.len();
        bc_zstd += zstd_len(&bc, 3);

        n += 1;
    }

    if n == 0 {
        eprintln!("no decodable blocks found in {path}");
        std::process::exit(1);
    }

    let per = |t: usize| t as f64 / n as f64;
    let base = per(json_raw);
    let row = |name: &str, total: usize, self_desc: &str| {
        let p = per(total);
        println!(
            "  {name:<26} {p:>9.0} B/blk   {:>5.2}x smaller   {:>7.2} TB/yr @26blk/s   {self_desc}",
            base / p,
            p * 26.0 * 86_400.0 * 365.0 / 1e12,
        );
    };

    println!("\nMeasured on {n} REAL blocks from {path}\n");
    row("JSON (current)", json_raw, "self-describing");
    row("JSON + zstd", json_zstd, "self-describing");
    row("MessagePack", mp_raw, "self-describing");
    row("MessagePack + zstd", mp_zstd, "self-describing");
    row("bincode", bc_raw, "NOT self-describing");
    row("bincode + zstd", bc_zstd, "NOT self-describing");
    println!("\n  Bitcoin for scale:                                        0.079 TB/yr\n");
}
