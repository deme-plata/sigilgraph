//! gpu.rs — orchestrate REAL GPU miners (the PROFITABLE path → BTC).
//!
//! Honest strategy (grounded 2026): GPUs cannot mine BTC (ASIC-only). They CAN
//! mine GPU-coins profitably — Kaspa / Alephium / Ergo — which **swap to BTC**
//! for the economy. So the miner runs T-Rex / lolMiner / GMiner on a GPU coin,
//! a BTC-payout (or auto-swap) pool turns it into BTC, and flux-pool does the
//! provable share split. This builds the launch command; running it needs a
//! CUDA GPU (Vast follow-on — Epsilon has none).

/// Top-3 GPU miner tools (2026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miner {
    TRex,     // fastest Nvidia/CUDA
    LolMiner, // best all-rounder, AMD+Nvidia
    GMiner,   // broad algos, high profit
}

/// GPU coins — profitable AND swappable to BTC.
///
/// NODE-TEST FINDING (2026-05-31): Kaspa is **ASIC-deprecated** — lolMiner 1.98a
/// dropped it (ASICs took kHeavyHash). Kept for reference but `recommended()` is
/// now **Etc** (Etchash): GPU-profitable, deepest BTC liquidity, measured 21.36
/// MH/s on a GTX 1660S in the fabric test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coin {
    Etc,
    Alephium,
    Ergo,
    /// ASIC-deprecated (ASICs dominate kHeavyHash); not a GPU play anymore.
    Kaspa,
}

impl Coin {
    /// The recommended GPU coin for the BTC economy (post node-test): ETC.
    pub fn recommended() -> Coin {
        Coin::Etc
    }
    /// True for coins ASICs have taken over (don't GPU-mine these).
    pub fn asic_deprecated(self) -> bool {
        matches!(self, Coin::Kaspa)
    }
    pub fn algo(self) -> &'static str {
        match self {
            Coin::Etc => "etchash",
            Coin::Alephium => "blake3",
            Coin::Ergo => "autolykos2",
            Coin::Kaspa => "kheavyhash",
        }
    }
    pub fn ticker(self) -> &'static str {
        match self {
            Coin::Etc => "ETC",
            Coin::Alephium => "ALPH",
            Coin::Ergo => "ERG",
            Coin::Kaspa => "KAS",
        }
    }
    /// A sensible default stratum pool (prefer one with BTC payout / auto-swap).
    pub fn default_pool(self) -> &'static str {
        match self {
            Coin::Etc => "stratum+tcp://etc.2miners.com:1010",
            Coin::Alephium => "stratum+tcp://alephium.herominers.com:1199",
            Coin::Ergo => "stratum+tcp://ergo.herominers.com:1180",
            Coin::Kaspa => "stratum+tcp://kaspa.herominers.com:1206",
        }
    }
}

impl Miner {
    pub fn binary(self) -> &'static str {
        match self {
            Miner::TRex => "t-rex",
            Miner::LolMiner => "lolMiner",
            Miner::GMiner => "miner", // GMiner's binary
        }
    }
}

/// A GPU mining plan: which tool, coin, pool, BTC payout wallet, and the ~10%
/// resource cap (`gpu_pct`).
#[derive(Debug, Clone)]
pub struct GpuPlan {
    pub miner: Miner,
    pub coin: Coin,
    pub pool: String,
    /// The BTC/LN address the pool (or swap) pays out to.
    pub btc_payout: String,
    /// Worker name (per-node share attribution into flux-pool).
    pub worker: String,
    /// Resource cap, e.g. 10 (%). Mapped to each miner's intensity flag.
    pub gpu_pct: u32,
}

impl GpuPlan {
    pub fn new(miner: Miner, coin: Coin, btc_payout: impl Into<String>, worker: impl Into<String>) -> Self {
        Self { miner, coin, pool: coin.default_pool().to_string(), btc_payout: btc_payout.into(), worker: worker.into(), gpu_pct: 10 }
    }

    /// Build the argv to spawn the chosen GPU miner against the chosen coin/pool.
    /// `gpu_pct` is mapped to the miner's intensity flag to honor the ~10% cap.
    pub fn build_command(&self) -> Vec<String> {
        let user = format!("{}.{}", self.btc_payout, self.worker);
        match self.miner {
            Miner::TRex => vec![
                "t-rex".into(), "-a".into(), self.coin.algo().into(),
                "-o".into(), self.pool.clone(), "-u".into(), user,
                "-p".into(), "x".into(),
                // ~10% via low intensity (T-Rex intensity 8..25; ~10 ≈ light load)
                "--intensity".into(), format!("{}", (self.gpu_pct / 4).max(8)),
            ],
            Miner::LolMiner => vec![
                "lolMiner".into(), "--algo".into(), self.coin.algo().to_uppercase(),
                "--pool".into(), self.pool.clone(), "--user".into(), user,
                // lolMiner --maxd​... use --gputhreads/ --tstop; approximate the cap with worksize
                "--worksize".into(), format!("{}", self.gpu_pct.max(8)),
            ],
            Miner::GMiner => vec![
                "miner".into(), "--algo".into(), self.coin.algo().into(),
                "--server".into(), self.pool.clone(), "--user".into(), user,
                "--intensity".into(), format!("{}", self.gpu_pct.max(8)),
            ],
        }
    }
}

/// Parse a hashrate (→ H/s) out of a miner's stdout line. Handles MH/s, GH/s,
/// kH/s, H/s. Returns None if the line carries no rate.
pub fn parse_hashrate(line: &str) -> Option<f64> {
    let l = line.to_ascii_lowercase();
    for (unit, mult) in [("gh/s", 1e9), ("mh/s", 1e6), ("kh/s", 1e3), ("h/s", 1.0)] {
        if let Some(pos) = l.find(unit) {
            // grab the number immediately before the unit
            let head = &l[..pos];
            let num: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ' ').collect::<String>().chars().rev().collect();
            if let Ok(v) = num.trim().parse::<f64>() {
                return Some(v * mult);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_trex_kaspa_command() {
        let plan = GpuPlan::new(Miner::TRex, Coin::Kaspa, "bc1qexample", "rocky-node1");
        let cmd = plan.build_command();
        assert_eq!(cmd[0], "t-rex");
        assert!(cmd.contains(&"kheavyhash".to_string()));
        assert!(cmd.iter().any(|a| a.contains("bc1qexample.rocky-node1")));
    }

    #[test]
    fn coin_metadata() {
        assert_eq!(Coin::Kaspa.ticker(), "KAS");
        assert_eq!(Coin::Ergo.algo(), "autolykos2");
        assert!(Coin::Alephium.default_pool().starts_with("stratum"));
    }

    #[test]
    fn node_test_finding_etc_recommended_kaspa_deprecated() {
        assert_eq!(Coin::recommended(), Coin::Etc); // post node-test
        assert_eq!(Coin::Etc.algo(), "etchash");
        assert!(Coin::Kaspa.asic_deprecated());
        assert!(!Coin::Etc.asic_deprecated());
    }

    #[test]
    fn parses_hashrate_units() {
        assert_eq!(parse_hashrate("GPU0 mining at 880.5 MH/s"), Some(880.5e6));
        assert_eq!(parse_hashrate("total 1.2 GH/s"), Some(1.2e9));
        assert_eq!(parse_hashrate("no rate here"), None);
    }
}
