//! # sigil-btc-miner — Prototype-1 Bitcoin side-miner (10% resources, share-based).
//!
//! HONEST framing (told Viktor): CPU/GPU SHA256d earns ~0 real mainnet BTC vs a
//! ~700 EH/s ASIC network. This is the **experiment**: run SHA256d at ~10% of
//! cores, measure the real hashrate, submit *shares* (low-difficulty solutions)
//! to the provable `flux-pool`, and compute the honest expected yield. The value
//! is the proven pipeline (miner → shares → provable pool → tiny LN payout via
//! sigil-bridge), not the BTC.

pub mod gpu;

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Bitcoin's hash: SHA256(SHA256(data)).
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    let a = Sha256::digest(data);
    let b = Sha256::digest(a);
    let mut o = [0u8; 32];
    o.copy_from_slice(&b);
    o
}

/// Does `hash` (big-endian compare) have at least `bits` leading zero bits?
/// That's a "share" at the pool's share-difficulty.
pub fn meets_share(hash: &[u8; 32], bits: u32) -> bool {
    let full = (bits / 8) as usize;
    for &byte in hash.iter().take(full) {
        if byte != 0 {
            return false;
        }
    }
    let rem = bits % 8;
    if rem != 0 {
        if let Some(&b) = hash.get(full) {
            if b >> (8 - rem) != 0 {
                return false;
            }
        }
    }
    true
}

/// How many threads = ~10% of the machine (the experiment's resource cap).
pub fn ten_percent_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    (cores / 10).max(1)
}

/// Result of a mining run.
#[derive(Debug, Clone)]
pub struct MinerStats {
    pub threads: usize,
    pub total_cores: usize,
    pub hashes: u64,
    pub elapsed_s: f64,
    pub hashrate_hps: f64,
    pub shares_found: u64,
    pub share_bits: u32,
}

impl MinerStats {
    /// HONEST expected mainnet BTC/day at this hashrate. (network ≈ 700 EH/s,
    /// 144 blocks/day, 3.125 BTC reward.) Spoiler: ~0.
    pub fn expected_btc_per_day(&self) -> f64 {
        const NETWORK_HPS: f64 = 7.0e20; // ~700 EH/s
        const BLOCKS_DAY: f64 = 144.0;
        const REWARD: f64 = 3.125;
        (self.hashrate_hps / NETWORK_HPS) * BLOCKS_DAY * REWARD
    }
}

/// Run the side-miner for `dur` using ~10% of cores. Hashes `header_prefix||nonce`
/// (SHA256d), counts hashes + shares (≥ `share_bits` leading zeros). Pure-CPU,
/// no GPU (CUDA path is the Vast follow-on).
pub fn mine(header_prefix: &[u8], share_bits: u32, dur: Duration) -> MinerStats {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let threads = (cores / 10).max(1);
    let stop = AtomicBool::new(false);
    let hashes = AtomicU64::new(0);
    let shares = AtomicU64::new(0);
    let t0 = Instant::now();

    std::thread::scope(|s| {
        for t in 0..threads {
            let (stop, hashes, shares, prefix) = (&stop, &hashes, &shares, header_prefix);
            s.spawn(move || {
                let mut buf = prefix.to_vec();
                buf.extend_from_slice(&[0u8; 8]); // nonce slot
                let nlen = buf.len();
                let mut nonce: u64 = (t as u64) << 56;
                let mut local = 0u64;
                loop {
                    for _ in 0..8192 {
                        buf[nlen - 8..].copy_from_slice(&nonce.to_le_bytes());
                        let h = sha256d(&buf);
                        if meets_share(&h, share_bits) {
                            shares.fetch_add(1, Ordering::Relaxed);
                        }
                        nonce = nonce.wrapping_add(1);
                    }
                    local += 8192;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
                hashes.fetch_add(local, Ordering::Relaxed);
            });
        }
        while t0.elapsed() < dur {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
    });

    let elapsed_s = t0.elapsed().as_secs_f64().max(1e-6);
    let h = hashes.load(Ordering::Relaxed);
    MinerStats {
        threads,
        total_cores: cores,
        hashes: h,
        elapsed_s,
        hashrate_hps: h as f64 / elapsed_s,
        shares_found: shares.load(Ordering::Relaxed),
        share_bits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256d_known_vector() {
        // double-SHA256("") — well-known value.
        let h = sha256d(b"");
        assert_eq!(hex_lower(&h), "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456");
    }

    #[test]
    fn meets_share_counts_leading_zero_bits() {
        let mut h = [0xffu8; 32];
        assert!(!meets_share(&h, 8));
        h[0] = 0x00;
        assert!(meets_share(&h, 8));
        assert!(!meets_share(&h, 9)); // next bit is 1
        h[1] = 0x7f;
        assert!(meets_share(&h, 9)); // 0x00,0x7f → 9 leading zeros
    }

    #[test]
    fn ten_percent_is_at_least_one() {
        assert!(ten_percent_threads() >= 1);
    }

    fn hex_lower(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
