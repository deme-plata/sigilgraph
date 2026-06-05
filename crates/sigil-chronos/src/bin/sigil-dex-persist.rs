//! sigil-dex-persist — sustained-write stress test. Runs the DEX market across
//! N threads and persists a binary trade ledger (128B/record) to a dir until
//! <target_gb> consumed. Disk-bound at scale (saturates the array), generating
//! real DEX swap data the whole time.
//!   sigil-dex-persist [target_gb] [dir] [threads]
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use sigil_dex::{swap, Pool, SwapDirection};

const REC: usize = 128;
const FILE_BYTES: u64 = 2_000_000_000; // 2GB per file

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let target_gb: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let dir = a.get(2).cloned().unwrap_or_else(|| "/home/storage/chronos-dex-5tb".into());
    let threads: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    std::fs::create_dir_all(&dir).unwrap();
    let target = target_gb * 1_000_000_000;
    let per = target / threads as u64;
    let bytes = Arc::new(AtomicU64::new(0));
    let trades = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    eprintln!("persist: target={}GB dir={} threads={} rec={}B", target_gb, dir, threads, REC);

    let mut hs = vec![];
    for t in 0..threads {
        let (dir, bytes, trades) = (dir.clone(), bytes.clone(), trades.clone());
        hs.push(thread::spawn(move || {
            let mut s = (t as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut rng = || { s = s.wrapping_add(0x9E37_79B9_7F4A_7C15); let mut z=s; z=(z^(z>>30)).wrapping_mul(0xBF58_476D_1CE4_E5B9); z=(z^(z>>27)).wrapping_mul(0x94D0_49BB_1331_11EB); z^(z>>31) };
            let mut pool = Pool { reserve_a: 1_000_000_000_000_000, reserve_b: 1_000_000_000_000_000, total_shares: 1, fee_bps: 30, accrued_fees_a: 0, accrued_fees_b: 0 };
            let mut local: u64 = 0;
            let mut fi = 0u64;
            while local < per {
                let path = format!("{}/dex-t{}-{:06}.bin", dir, t, fi); fi += 1;
                let mut w = BufWriter::with_capacity(1 << 22, File::create(&path).expect("create"));
                let mut fbytes = 0u64;
                let mut buf = [0u8; REC];
                while fbytes < FILE_BYTES && local < per {
                    let r = rng();
                    let dir_ = if r & 1 == 0 { SwapDirection::AtoB } else { SwapDirection::BtoA };
                    let amt = 1000 + (r % 100_000) as u128;
                    let out = swap(&pool, dir_, amt, 0);
                    let (ao, fee) = if let Ok(o) = out { let v=(o.amount_out, o.fee_amount); pool = o.pool_after; v } else { (0, 0) };
                    let mut p = 0;
                    buf[p..p+8].copy_from_slice(&local.to_le_bytes()); p+=8;
                    buf[p..p+4].copy_from_slice(&(t as u32).to_le_bytes()); p+=4;
                    buf[p] = (r & 0xff) as u8; p+=1; buf[p] = (r & 1) as u8; p+=1; p+=2;
                    buf[p..p+16].copy_from_slice(&pool.reserve_a.to_le_bytes()); p+=16;
                    buf[p..p+16].copy_from_slice(&pool.reserve_b.to_le_bytes()); p+=16;
                    buf[p..p+16].copy_from_slice(&amt.to_le_bytes()); p+=16;
                    buf[p..p+16].copy_from_slice(&ao.to_le_bytes()); p+=16;
                    buf[p..p+16].copy_from_slice(&fee.to_le_bytes()); p+=16;
                    // remainder: a blake3-mini "block hash" of the record so far
                    let h = blake3::hash(&buf[..p]); let hb = h.as_bytes();
                    let rem = REC - p; buf[p..].copy_from_slice(&hb[..rem]);
                    w.write_all(&buf).expect("write");
                    fbytes += REC as u64; local += REC as u64;
                    if local % (256 << 20) < REC as u64 { trades.fetch_add((256<<20)/REC as u64, Ordering::Relaxed); bytes.store(0, Ordering::Relaxed); }
                }
                w.flush().ok();
            }
        }));
    }
    // progress reporter
    let rep_bytes = bytes.clone();
    {
        let dir = dir.clone();
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_secs(30));
            let used = du(&dir);
            let gb = used as f64 / 1e9;
            let rate = gb / t0.elapsed().as_secs_f64().max(1.0);
            eprintln!("[{:.0}s] {:.1} GB / {} GB · {:.2} GB/s · ~{:.0} min left", t0.elapsed().as_secs_f64(), gb, target_gb, rate, (target_gb as f64 - gb)/rate.max(0.01)/60.0);
            if used >= target { eprintln!("TARGET REACHED {:.1} GB", gb); std::process::exit(0); }
            let _ = &rep_bytes;
        });
    }
    for h in hs { h.join().ok(); }
    eprintln!("DONE {:.1} GB written in {:.0}s", du(&dir) as f64/1e9, t0.elapsed().as_secs_f64());
}

fn du(dir: &str) -> u64 {
    std::fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0)
}
