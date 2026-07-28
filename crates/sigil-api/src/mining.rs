//! mining.rs — the dual-lane mining surface **on the braid**.
//!
//! `sigil-rpcd` grew a second, linear chain because mining needed a settled tip
//! to bind work to, and the braid only settles after `final_depth`. This module
//! removes that reason: work binds to the braid's **frontier** (the settled chain
//! plus the selected-spine suffix the producer is about to extend), which is
//! exactly the parent the next braid block will carry. Same `flux-miner`
//! verification gate as rpcd, so every existing miner speaks to the braid
//! unchanged — only the URL moves.
//!
//! ## The property that makes this real: headers self-certify their work
//!
//! A challenge is fully determined by `(parent_hash, height)`, and the solved
//! header additionally binds the miner's wallet. All three live in
//! `SigilBlockHeaderV0` (`parent_hash`, `height`, `producer`), and the winning
//! `(nonce, blake4_hash)` ride in the 292-byte `nonce_sqisign` carrier with the
//! Wesolowski proof in `vdf_proof`. So **any follower can reconstruct the
//! challenge and re-verify both lanes from the header alone** —
//! [`verify_header_pow`]. No trust in the producer, no side-channel, no stored
//! block record required. That is what the empty braid never had.
//!
//! ## Canonical wallet encoding (a real tightening over rpcd)
//!
//! `build_header` hashes the wallet as the *string the miner sent*. rpcd accepts
//! any casing, which means a follower holding only the 32 header bytes cannot
//! always rebuild that string. The braid therefore requires the submitted wallet
//! to be exactly `hex::encode(bytes)` — 64 lowercase hex, no `0x`. Non-canonical
//! submissions are rejected with a reason that says how to fix it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, RwLock};

use flux_miner::client::{build_header, check_submission_at, Challenge, Submission};
use flux_miner::{blake4, verify_dual, DualLaneBlock};
use flux_vdf::{ModSquaring, VdfProof};
use sigil_state::WalletId;

/// Domain-separated per-height challenge seed, bound to the frontier parent:
/// `BLAKE3(domain ‖ parent ‖ height)`. Because `parent` is the hash of the block
/// this work will extend, the seed for the next height is unknowable until that
/// block exists — the same precompute defence rpcd's tip-fold provides.
pub fn mining_seed(parent: &[u8; 32], height: u64) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-g0/mining-challenge/v1");
    h.update(parent);
    h.update(&height.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Lane-A target for difficulty `bits`: `u64::MAX >> bits`.
pub fn target_from_bits(bits: u32) -> u64 {
    u64::MAX >> bits.min(63)
}

/// Lane-A difficulty (`SIGIL_MINING_BLAKE4_BITS`, default 16 — matches rpcd, so a
/// miner pointed at the braid does the same amount of work it does today).
pub fn blake4_bits() -> u32 {
    std::env::var("SIGIL_MINING_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(16)
}

/// Lane-B sequential work (`SIGIL_MINING_VDF_T`, default 600 squarings).
pub fn vdf_t() -> u64 {
    std::env::var("SIGIL_MINING_VDF_T").ok().and_then(|s| s.parse().ok()).unwrap_or(600)
}

/// Sub-difficulty share ease in bits (`SIGIL_SHARE_EASE_BITS`). Default 0 =
/// **solo semantics**: only full-difficulty solves are accepted, exactly the
/// pre-pool wire behaviour. Set >0 to run the braid as a pool.
pub fn share_ease_bits() -> u32 {
    std::env::var("SIGIL_SHARE_EASE_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// The 292-byte `nonce_sqisign` carrier layout shared with
/// `sigil_rpc::build_ledger_header`: `nonce` LE ‖ `blake4_hash` LE ‖ zeros.
/// The claimed Lane-A word must be the word the header+nonce actually hash to.
///
/// `verify_dual` bounds the claimed `blake4_hash` by the target and separately
/// re-hashes the nonce — but it never ties the two together, so a valid nonce
/// paired with ANY under-target word (`0` being the easy one) passes. rpcd can
/// live with that because it only credits a share. The braid cannot: this word
/// is packed into the header's nonce carrier and is what the chain permanently
/// records as the block's proof-of-work, so a forged word would write a lie
/// into a header that still verified. One extra BLAKE4 buys the self-certifying
/// property this module claims. Honest miners are unaffected — `mine_dual`
/// always reports the exact recomputed word.
fn lane_a_word_is_honest(header: &[u8], nonce: u64, claimed: u64) -> bool {
    blake4(header, nonce) == claimed
}

pub fn pack_nonce_carrier(nonce: u64, blake4_hash: u64) -> [u8; 292] {
    let mut c = [0u8; 292];
    c[..8].copy_from_slice(&nonce.to_le_bytes());
    c[8..16].copy_from_slice(&blake4_hash.to_le_bytes());
    c
}

/// Inverse of [`pack_nonce_carrier`] — `(nonce, blake4_hash)`.
pub fn unpack_nonce_carrier(carrier: &[u8]) -> Option<(u64, u64)> {
    if carrier.len() < 16 {
        return None;
    }
    let mut n = [0u8; 8];
    let mut b = [0u8; 8];
    n.copy_from_slice(&carrier[..8]);
    b.copy_from_slice(&carrier[8..16]);
    Some((u64::from_le_bytes(n), u64::from_le_bytes(b)))
}

/// **The follower's verification of a mined braid block.** Rebuilds the exact
/// challenge the miner solved from header material only, then verifies Lane A
/// (BLAKE4 ≤ target) and Lane B (the Wesolowski proof over the required number
/// of sequential squarings). Every node runs this on every block — the work is
/// checked, never trusted.
pub fn verify_header_pow(
    parent_hash: &[u8; 32],
    height: u64,
    producer: &WalletId,
    nonce_carrier: &[u8],
    vdf: &VdfProof,
    bits: u32,
    required_vdf_t: u64,
) -> bool {
    let Some((nonce, blake4_hash)) = unpack_nonce_carrier(nonce_carrier) else {
        return false;
    };
    if vdf.t != required_vdf_t {
        return false;
    }
    let c = Challenge {
        height,
        vdf_input: mining_seed(parent_hash, height),
        blake4_target: target_from_bits(bits),
        vdf_t: required_vdf_t,
        net_hps: 0.0,
        share_target: 0,
    };
    let block = DualLaneBlock {
        header: build_header(&c, &hex::encode(producer)),
        nonce,
        blake4_hash,
        vdf: vdf.clone(),
    };
    if !lane_a_word_is_honest(&block.header, nonce, blake4_hash) {
        return false;
    }
    verify_dual(&ModSquaring::bench_2048(), &block, c.blake4_target)
}

/// The frontier a miner is working against: the parent the next braid block will
/// carry, plus the difficulty parameters in force for it.
#[derive(Clone, Debug, PartialEq)]
pub struct MiningTip {
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub bits: u32,
    pub vdf_t: u64,
}

/// A verified full-difficulty solve, queued for the producer to mint into a
/// braid block. Carries everything the header needs — no re-derivation, no
/// second lookup.
#[derive(Clone, Debug)]
pub struct AcceptedSolve {
    pub wallet: WalletId,
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub nonce: u64,
    pub blake4_hash: u64,
    pub vdf: VdfProof,
    pub bits: u32,
    /// Share weights accumulated for this height, for proportional payout. The
    /// winner's own weight is already folded in.
    pub shares: HashMap<WalletId, u64>,
}

/// Why a submission was turned away — the categories the pool diagnostics count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectKind {
    StaleHeight,
    NonCanonicalWallet,
    Duplicate,
    VerifyMismatch,
    NoTip,
}

impl RejectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectKind::StaleHeight => "stale_height",
            RejectKind::NonCanonicalWallet => "non_canonical_wallet",
            RejectKind::Duplicate => "duplicate",
            RejectKind::VerifyMismatch => "verify_mismatch",
            RejectKind::NoTip => "no_tip",
        }
    }
}

/// Outcome of a submission, mapped to the wire `SubmitResult` by the handler.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmitOutcome {
    /// Full-difficulty solve — queued; the producer will mint it.
    Block { height: u64 },
    /// Sub-difficulty share — credited into this height's payout window.
    Share { height: u64, weight: u64 },
    Rejected { kind: RejectKind, detail: String },
}

/// The seam between the HTTP surface (many threads) and the producer loop (one
/// thread). The producer publishes the frontier it is about to extend; miners
/// pull challenges from it and push solves back; the producer pops a solve when
/// it mints. Nothing else crosses.
#[derive(Default)]
pub struct MiningBridge {
    tip: RwLock<Option<MiningTip>>,
    solved: Mutex<VecDeque<AcceptedSolve>>,
    /// Share weights for the height currently being worked.
    shares: Mutex<HashMap<WalletId, u64>>,
    /// `(wallet, nonce)` already credited at the current height — replay guard.
    seen: Mutex<HashSet<(WalletId, u64)>>,
    /// Self-reported Lane-A rate per miner: `(hashes/s, last_report_ms)`.
    hps: Mutex<HashMap<WalletId, (f64, u64)>>,
    rejects: Mutex<HashMap<&'static str, u64>>,
    accepted_blocks: Mutex<u64>,
    accepted_shares: Mutex<u64>,
}

/// Depth of the solve queue. A producer mints one block per solve, so a deep
/// queue would let a burst of solves mint a burst of blocks against a stale
/// parent. Small on purpose.
const SOLVE_QUEUE_CAP: usize = 8;

/// Miners are pruned from the rate table after this long without a challenge
/// fetch, so `net_hps` reflects live power only.
const HPS_IDLE_MS: u64 = 30_000;

impl MiningBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Producer: publish the frontier the next block will extend. Called every
    /// produce tick; when the height advances, the previous height's share
    /// window and replay set are cleared (they belong to work that is done).
    pub fn publish_tip(&self, height: u64, parent_hash: [u8; 32]) {
        let new = MiningTip { height, parent_hash, bits: blake4_bits(), vdf_t: vdf_t() };
        let advanced = {
            let cur = self.tip.read().ok();
            match cur.as_deref() {
                Some(Some(t)) => t.height != new.height || t.parent_hash != new.parent_hash,
                _ => true,
            }
        };
        if let Ok(mut w) = self.tip.write() {
            *w = Some(new);
        }
        if advanced {
            if let Ok(mut s) = self.seen.lock() {
                s.clear();
            }
        }
    }

    pub fn tip(&self) -> Option<MiningTip> {
        self.tip.read().ok().and_then(|t| t.clone())
    }

    /// Record a miner's self-reported Lane-A rate and return the live network
    /// total (sum over miners that fetched a challenge in the last 30s).
    pub fn report_hps(&self, wallet: Option<WalletId>, hps: Option<f64>, now_ms: u64) -> f64 {
        let Ok(mut m) = self.hps.lock() else { return 0.0 };
        if let (Some(w), Some(r)) = (wallet, hps) {
            if r.is_finite() && r >= 0.0 {
                m.insert(w, (r, now_ms));
            }
        }
        m.retain(|_, (_, t)| now_ms.saturating_sub(*t) <= HPS_IDLE_MS);
        m.values().map(|(r, _)| *r).sum()
    }

    /// Build the challenge for `wallet` against the current frontier. `None`
    /// when the producer has not published a frontier yet (node still booting,
    /// or not a producer).
    pub fn challenge_for(&self, wallet: Option<WalletId>, now_ms: u64) -> Option<Challenge> {
        let tip = self.tip()?;
        let net_hps = self.report_hps(wallet, None, now_ms);
        let ease = share_ease_bits();
        Some(Challenge {
            height: tip.height,
            vdf_input: mining_seed(&tip.parent_hash, tip.height),
            blake4_target: target_from_bits(tip.bits),
            vdf_t: tip.vdf_t,
            net_hps,
            share_target: if ease == 0 { 0 } else { target_from_bits(tip.bits.saturating_sub(ease)) },
        })
    }

    fn count_reject(&self, kind: RejectKind) {
        if let Ok(mut r) = self.rejects.lock() {
            *r.entry(kind.as_str()).or_insert(0) += 1;
        }
    }

    /// Verify a submission against the current frontier and, if it is work,
    /// bank it: a full-difficulty solve is queued for minting, a sub-difficulty
    /// solve is credited into this height's share window.
    pub fn submit(&self, sub: &Submission) -> SubmitOutcome {
        let Some(tip) = self.tip() else {
            self.count_reject(RejectKind::NoTip);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NoTip,
                detail: "this node is not producing — no frontier to mine on".into(),
            };
        };

        // Canonical wallet: the header commits to the wallet STRING, so it must
        // be reproducible from the 32 bytes a follower reads out of the header.
        let Some(wallet) = crate::hex32(&sub.wallet) else {
            self.count_reject(RejectKind::NonCanonicalWallet);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NonCanonicalWallet,
                detail: "wallet must be 64-hex".into(),
            };
        };
        if sub.wallet != hex::encode(wallet) {
            self.count_reject(RejectKind::NonCanonicalWallet);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NonCanonicalWallet,
                detail: "wallet must be canonical lowercase 64-hex with no 0x prefix — \
                         the block header commits to this exact string so every follower \
                         can rebuild and re-verify the work".into(),
            };
        }

        if sub.height != tip.height {
            self.count_reject(RejectKind::StaleHeight);
            return SubmitOutcome::Rejected {
                kind: RejectKind::StaleHeight,
                detail: format!(
                    "stale height: submitted {} but the mineable frontier is {}",
                    sub.height, tip.height
                ),
            };
        }

        // Replay guard before any verification work is banked.
        let key = (wallet, sub.block.nonce);
        {
            let Ok(mut seen) = self.seen.lock() else {
                return SubmitOutcome::Rejected {
                    kind: RejectKind::VerifyMismatch,
                    detail: "replay set unavailable".into(),
                };
            };
            if !seen.insert(key) {
                self.count_reject(RejectKind::Duplicate);
                return SubmitOutcome::Rejected {
                    kind: RejectKind::Duplicate,
                    detail: "this (wallet, nonce) already credited at this height".into(),
                };
            }
        }

        let ease = share_ease_bits();
        let c = Challenge {
            height: tip.height,
            vdf_input: mining_seed(&tip.parent_hash, tip.height),
            blake4_target: target_from_bits(tip.bits),
            vdf_t: tip.vdf_t,
            net_hps: 0.0,
            share_target: if ease == 0 { 0 } else { target_from_bits(tip.bits.saturating_sub(ease)) },
        };
        let g = ModSquaring::bench_2048();

        // A dishonest Lane-A word falls through to the tail, which releases the
        // replay slot — so an honest retry on the same nonce is not locked out.
        let honest_word =
            lane_a_word_is_honest(&sub.block.header, sub.block.nonce, sub.block.blake4_hash);

        if honest_word && check_submission_at(&g, &c, sub, c.blake4_target) {
            let weight: u64 = 1u64 << ease.min(32);
            let mut shares = self.shares.lock().map(|m| m.clone()).unwrap_or_default();
            *shares.entry(wallet).or_insert(0) += weight;
            let solve = AcceptedSolve {
                wallet,
                height: tip.height,
                parent_hash: tip.parent_hash,
                nonce: sub.block.nonce,
                blake4_hash: sub.block.blake4_hash,
                vdf: sub.block.vdf.clone(),
                bits: tip.bits,
                shares,
            };
            if let Ok(mut q) = self.solved.lock() {
                if q.len() >= SOLVE_QUEUE_CAP {
                    q.pop_front();
                }
                q.push_back(solve);
            }
            if let Ok(mut m) = self.shares.lock() {
                m.clear();
            }
            if let Ok(mut n) = self.accepted_blocks.lock() {
                *n += 1;
            }
            return SubmitOutcome::Block { height: tip.height };
        }

        if honest_word && c.share_target > 0 && check_submission_at(&g, &c, sub, c.share_target) {
            let weight = 1u64;
            if let Ok(mut m) = self.shares.lock() {
                *m.entry(wallet).or_insert(0) += weight;
            }
            if let Ok(mut n) = self.accepted_shares.lock() {
                *n += 1;
            }
            return SubmitOutcome::Share { height: tip.height, weight };
        }

        // Not work: release the replay slot so an honest retry isn't locked out.
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(&key);
        }
        self.count_reject(RejectKind::VerifyMismatch);
        SubmitOutcome::Rejected {
            kind: RejectKind::VerifyMismatch,
            detail: "dual-lane verify failed (wrong frontier, target, VDF, or a \
                     blake4_hash that is not what this header+nonce hash to)".into(),
        }
    }

    /// Producer: take the next verified solve to mint, if one is waiting.
    pub fn take_solve(&self) -> Option<AcceptedSolve> {
        self.solved.lock().ok().and_then(|mut q| q.pop_front())
    }

    pub fn queued_solves(&self) -> usize {
        self.solved.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Snapshot for the miners endpoint: `(net_hps, live_miners, blocks, shares, rejects)`.
    pub fn stats(&self, now_ms: u64) -> (f64, usize, u64, u64, Vec<(String, u64)>) {
        let (net_hps, live) = self
            .hps
            .lock()
            .map(|m| {
                let live: Vec<_> =
                    m.values().filter(|(_, t)| now_ms.saturating_sub(*t) <= HPS_IDLE_MS).collect();
                (live.iter().map(|(r, _)| *r).sum::<f64>(), live.len())
            })
            .unwrap_or((0.0, 0));
        let blocks = self.accepted_blocks.lock().map(|n| *n).unwrap_or(0);
        let shares = self.accepted_shares.lock().map(|n| *n).unwrap_or(0);
        let mut rejects: Vec<(String, u64)> = self
            .rejects
            .lock()
            .map(|r| r.iter().map(|(k, v)| ((*k).to_string(), *v)).collect())
            .unwrap_or_default();
        rejects.sort();
        (net_hps, live, blocks, shares, rejects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_miner::client::solve;

    fn tiny_bits() -> u32 {
        4
    }

    fn bridge_at(height: u64, parent: [u8; 32]) -> MiningBridge {
        // Keep the test's work trivial and deterministic.
        std::env::set_var("SIGIL_MINING_BLAKE4_BITS", tiny_bits().to_string());
        std::env::set_var("SIGIL_MINING_VDF_T", "8");
        std::env::set_var("SIGIL_SHARE_EASE_BITS", "0");
        let b = MiningBridge::new();
        b.publish_tip(height, parent);
        b
    }

    fn solve_for(c: &Challenge, wallet: &WalletId) -> Submission {
        let g = ModSquaring::bench_2048();
        let block = solve(c, &hex::encode(wallet), &g);
        Submission { height: c.height, wallet: hex::encode(wallet), block }
    }

    #[test]
    fn challenge_binds_to_the_frontier_parent() {
        let parent_a = [0x11u8; 32];
        let parent_b = [0x22u8; 32];
        let b = bridge_at(7, parent_a);
        let c1 = b.challenge_for(None, 0).unwrap();
        assert_eq!(c1.height, 7);
        assert_eq!(c1.vdf_input, mining_seed(&parent_a, 7));

        // A different frontier parent at the same height is different work —
        // this is what stops precompute against a fork that never wins.
        b.publish_tip(7, parent_b);
        let c2 = b.challenge_for(None, 0).unwrap();
        assert_ne!(c1.vdf_input, c2.vdf_input);
    }

    #[test]
    fn valid_solve_is_accepted_and_queued_for_the_producer() {
        let wallet: WalletId = [0xABu8; 32];
        let b = bridge_at(3, [0x99u8; 32]);
        let c = b.challenge_for(Some(wallet), 0).unwrap();
        let sub = solve_for(&c, &wallet);

        assert_eq!(b.submit(&sub), SubmitOutcome::Block { height: 3 });
        let got = b.take_solve().expect("the producer finds a solve waiting");
        assert_eq!(got.wallet, wallet);
        assert_eq!(got.height, 3);
        assert_eq!(got.nonce, sub.block.nonce);
        assert!(b.take_solve().is_none(), "the queue drains");
    }

    #[test]
    fn a_mined_header_is_reverifiable_by_a_follower() {
        // The whole point: a follower holding ONLY the header can rebuild the
        // challenge and check both lanes.
        let wallet: WalletId = [0x5Au8; 32];
        let parent = [0x77u8; 32];
        let b = bridge_at(11, parent);
        let c = b.challenge_for(Some(wallet), 0).unwrap();
        let sub = solve_for(&c, &wallet);
        assert!(matches!(b.submit(&sub), SubmitOutcome::Block { .. }));
        let s = b.take_solve().unwrap();

        let carrier = pack_nonce_carrier(s.nonce, s.blake4_hash);
        assert!(
            verify_header_pow(&parent, 11, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "header material alone must re-verify the work"
        );

        // Tampering with any bound field breaks it.
        assert!(!verify_header_pow(&[0x00u8; 32], 11, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "a different parent is different work");
        assert!(!verify_header_pow(&parent, 12, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "a different height is different work");
        assert!(!verify_header_pow(&parent, 11, &[0x01u8; 32], &carrier, &s.vdf, s.bits, c.vdf_t),
            "a share cannot be re-pointed at another miner");
        let bad = pack_nonce_carrier(s.nonce.wrapping_add(1), s.blake4_hash);
        assert!(!verify_header_pow(&parent, 11, &wallet, &bad, &s.vdf, s.bits, c.vdf_t),
            "a forged nonce fails Lane A");
    }

    #[test]
    fn stale_height_and_replay_are_rejected() {
        let wallet: WalletId = [0x0Fu8; 32];
        let b = bridge_at(5, [0x42u8; 32]);
        let c = b.challenge_for(Some(wallet), 0).unwrap();
        let sub = solve_for(&c, &wallet);

        // replay of an accepted solve
        assert!(matches!(b.submit(&sub), SubmitOutcome::Block { .. }));
        assert!(matches!(
            b.submit(&sub),
            SubmitOutcome::Rejected { kind: RejectKind::Duplicate, .. }
        ));

        // work for a height that is no longer the frontier
        b.publish_tip(6, [0x43u8; 32]);
        assert!(matches!(
            b.submit(&sub),
            SubmitOutcome::Rejected { kind: RejectKind::StaleHeight, .. }
        ));
    }

    #[test]
    fn non_canonical_wallet_is_rejected_with_a_fixable_reason() {
        let wallet: WalletId = [0xC3u8; 32];
        let b = bridge_at(2, [0x01u8; 32]);
        let c = b.challenge_for(Some(wallet), 0).unwrap();
        let mut sub = solve_for(&c, &wallet);
        sub.wallet = hex::encode(wallet).to_uppercase();

        match b.submit(&sub) {
            SubmitOutcome::Rejected { kind, detail } => {
                assert_eq!(kind, RejectKind::NonCanonicalWallet);
                assert!(detail.contains("lowercase"), "the reason says how to fix it");
            }
            other => panic!("uppercase wallet must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_rejected_without_consuming_the_replay_slot() {
        let wallet: WalletId = [0x7Eu8; 32];
        let b = bridge_at(4, [0x55u8; 32]);
        let c = b.challenge_for(Some(wallet), 0).unwrap();
        let good = solve_for(&c, &wallet);

        // same nonce, broken proof
        let mut forged = good.clone();
        forged.block.blake4_hash = 0;
        assert!(matches!(
            b.submit(&forged),
            SubmitOutcome::Rejected { kind: RejectKind::VerifyMismatch, .. }
        ));
        // the honest solve carrying that nonce still lands
        assert!(matches!(b.submit(&good), SubmitOutcome::Block { .. }));
    }

    #[test]
    fn no_frontier_means_no_mining() {
        let b = MiningBridge::new();
        assert!(b.challenge_for(None, 0).is_none());
        let sub = Submission {
            height: 0,
            wallet: hex::encode([0u8; 32]),
            block: DualLaneBlock {
                header: vec![],
                nonce: 0,
                blake4_hash: 0,
                vdf: VdfProof { y: vec![], pi: vec![], t: 0 },
            },
        };
        assert!(matches!(
            b.submit(&sub),
            SubmitOutcome::Rejected { kind: RejectKind::NoTip, .. }
        ));
    }

    #[test]
    fn net_hps_sums_live_miners_and_prunes_idle_ones() {
        let b = bridge_at(1, [0u8; 32]);
        b.report_hps(Some([1u8; 32]), Some(1000.0), 0);
        b.report_hps(Some([2u8; 32]), Some(500.0), 0);
        assert_eq!(b.challenge_for(None, 0).unwrap().net_hps, 1500.0);

        // one keeps reporting, the other goes silent past the idle window
        let later = HPS_IDLE_MS + 1;
        b.report_hps(Some([1u8; 32]), Some(1000.0), later);
        assert_eq!(b.challenge_for(None, later).unwrap().net_hps, 1000.0);
    }
}
