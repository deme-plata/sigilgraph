//! chainlog_batch_bench — what a block on disk actually costs, and what batching +
//! a trained zstd dictionary would buy. Reads the TAIL of a real chain.log (v1 records),
//! so the numbers are today's blocks, not a synthetic.
//!
//!   chainlog_batch_bench <chain.log> [n_blocks=4096]
use std::io::{BufReader, Read, Seek, SeekFrom};

use sigil_node::block::Block;
use sigil_node::chain_log::decode_record;

fn z(bytes: &[u8], level: i32) -> usize { zstd::encode_all(bytes, level).map(|v| v.len()).unwrap_or(0) }

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("chain.log path");
    let want: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4096);
    let mut f = std::fs::File::open(&path).expect("open");
    let size = f.metadata().unwrap().len();
    // Start ~want*1400 B before the end, then resync on the u32 length framing by scanning
    // for a plausible [len][0xB5 0x01] record head.
    let start = size.saturating_sub((want as u64) * 1400 + 4096);
    f.seek(SeekFrom::Start(start)).unwrap();
    let mut r = BufReader::new(f);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    let mut i = 0usize;
    let mut recs: Vec<(usize, Block)> = Vec::new();
    // resync
    while i + 6 < buf.len() {
        let len = u32::from_le_bytes([buf[i], buf[i+1], buf[i+2], buf[i+3]]) as usize;
        if len > 100 && len < 1 << 20 && buf[i+4] == 0xB5 && buf[i+5] == 1 && i + 4 + len <= buf.len() {
            if let Some(b) = decode_record(&buf[i+4..i+4+len]) {
                recs.push((len, b));
                i += 4 + len;
                continue;
            }
        }
        if !recs.is_empty() { break; } // lost sync after a good run: stop
        i += 1;
    }
    let n = recs.len();
    assert!(n > 0, "no records decoded");
    let on_disk: usize = recs.iter().map(|(l, _)| l + 4).sum();
    let blocks: Vec<&Block> = recs.iter().map(|(_, b)| b).collect();
    let txs: usize = blocks.iter().map(|b| b.transition.mutations.len()).sum();
    println!("blocks={n} heights {}..{} mutations={txs}  ON DISK today: {} B/blk (per-record msgpack+zstd3)", blocks[0].header.height, blocks[n-1].header.height, on_disk / n);

    // Anatomy of one empty block (raw msgpack, named)
    let b0 = blocks.iter().min_by_key(|b| b.transition.mutations.len()).unwrap_or(&blocks[0]);
    let mp0 = rmp_serde::to_vec_named(b0).unwrap();
    let hdr = rmp_serde::to_vec_named(&b0.header).unwrap();
    println!("anatomy of one empty block: msgpack {} B total, header {} B, body {} B; zstd3 alone {} B",
        mp0.len(), hdr.len(), mp0.len() - hdr.len(), z(&mp0, 3));
    if let Ok(v) = serde_json::to_value(b0) {
        if let Some(m) = v.as_object() {
            let mut sizes: Vec<(String, usize)> = Vec::new();
            fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, usize)>) {
                match v {
                    serde_json::Value::Object(m) => for (k, x) in m { walk(&format!("{prefix}{k}."), x, out) },
                    _ => {
                        // estimate the binary size: byte arrays as len, numbers 8, strings len
                        let est = match v {
                            serde_json::Value::Array(a) if a.iter().all(|e| e.is_u64()) => a.len(),
                            serde_json::Value::Array(a) => a.iter().map(|e| e.to_string().len()).sum(),
                            serde_json::Value::String(s) => s.len(),
                            serde_json::Value::Number(_) => 8,
                            _ => 1,
                        };
                        out.push((prefix.trim_end_matches('.').to_string(), est));
                    }
                }
            }
            walk("", &serde_json::Value::Object(m.clone()), &mut sizes);
            sizes.sort_by(|a, b| b.1.cmp(&a.1));
            for (k, s) in sizes.iter().take(14) { println!("   {s:>5} B  {k}"); }
        }
    }

    if std::env::var("PEEK").is_ok() {
        let v = serde_json::to_value(b0).unwrap();
        let nonce = v["header"]["nonce_sqisign"].as_array().map(|a| a.iter().filter(|x| x.as_u64() == Some(0)).count()).unwrap_or(0);
        println!("nonce_sqisign zero bytes: {nonce}/292");
        for m in v["transition"]["mutations"].as_array().unwrap() {
            let s = m.to_string();
            println!("   mutation {} B: {}", s.len(), &s[..s.len().min(160)]);
        }
        println!("events: {}", v["events"].to_string().len());
    }
    // Batched: one zstd frame over N msgpack records (what the wire does now)
    let mut cat = Vec::new();
    let mut mps: Vec<Vec<u8>> = Vec::new();
    for b in &blocks { let m = rmp_serde::to_vec_named(b).unwrap(); cat.extend_from_slice(&(m.len() as u32).to_le_bytes()); cat.extend_from_slice(&m); mps.push(m); }
    for lvl in [3, 9, 19] {
        println!("batch of {n} msgpack, zstd{lvl}: {} B/blk", z(&cat, lvl) / n);
    }
    for chunk in [64usize, 256, 1024] {
        let mut tot = 0; let mut c = 0;
        for w in mps.chunks(chunk) { let mut cc = Vec::new(); for m in w { cc.extend_from_slice(&(m.len() as u32).to_le_bytes()); cc.extend_from_slice(m); } tot += z(&cc, 3); c += w.len(); }
        println!("segments of {chunk} blocks, zstd3: {} B/blk", tot / c);
    }
    // Dictionary-trained per-record (keeps random access per record)
    let samples: Vec<&[u8]> = mps.iter().take(2048).map(|v| v.as_slice()).collect();
    match zstd::dict::from_samples(&samples, 110 * 1024) {
        Ok(dict) => {
            let mut tot = 0;
            for m in &mps {
                let mut enc = zstd::stream::Encoder::with_dictionary(Vec::new(), 3, &dict).unwrap();
                std::io::Write::write_all(&mut enc, m).unwrap();
                tot += enc.finish().unwrap().len();
            }
            println!("per-record with a {} KB trained dictionary, zstd3: {} B/blk (random access kept)", dict.len() / 1024, tot / n);
            let mut tot9 = 0;
            for m in &mps {
                let mut enc = zstd::stream::Encoder::with_dictionary(Vec::new(), 9, &dict).unwrap();
                std::io::Write::write_all(&mut enc, m).unwrap();
                tot9 += enc.finish().unwrap().len();
            }
            println!("per-record with dictionary, zstd9: {} B/blk", tot9 / n);
        }
        Err(e) => println!("dict training failed: {e}"),
    }
    // Dictionary size sweep (train on the OLDER half, measure on the NEWER half — no self-fit)
    let half = mps.len() / 2;
    let train: Vec<&[u8]> = mps[..half].iter().map(|v| v.as_slice()).collect();
    for kb in [32usize, 64, 110, 256] {
        if let Ok(dict) = zstd::dict::from_samples(&train, kb * 1024) {
            let mut tot = 0;
            for m in &mps[half..] {
                let mut enc = zstd::stream::Encoder::with_dictionary(Vec::new(), 3, &dict).unwrap();
                std::io::Write::write_all(&mut enc, m).unwrap();
                tot += enc.finish().unwrap().len();
            }
            println!("dict {kb} KB (trained on older half, measured on newer half), zstd3: {} B/blk", tot / (mps.len() - half));
            if kb == 64 { if let Ok(out) = std::env::var("DICT_OUT") { std::fs::write(&out, &dict).unwrap(); println!("wrote {out} ({} B)", dict.len()); } }
        }
    }
    // bincode batch reference
    let mut bc = Vec::new();
    for b in &blocks { bc.extend_from_slice(&bincode::serialize(b).unwrap()); }
    println!("batch bincode zstd3 (reference, NOT self-describing): {} B/blk", z(&bc, 3) / n);
}

#[allow(dead_code)]
fn unused() {}
