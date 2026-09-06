//! Effective peers: `N_p^eff = sum_i u_i * v_i * r_i`, and the amplification
//! `P_P2P = ln(1 + N_p^eff)` it feeds into the functional.
//!
//! ## Why a raw peer count is the wrong number
//!
//! Counting peers rewards the one thing an attacker can manufacture for free.
//! A thousand idle nodes that never produce a verifiable result are worth less
//! than three that do, and any formula that says otherwise is a formula for
//! buying Sybils. So each peer is weighted by three factors, all in `[0, 1]`:
//!
//! * `u_i` — **useful contribution**: is this peer doing real work, on its own
//!   `[0, 1]` scale — e.g. `min(1, hash_rate / reference_rate)`. **Not a share
//!   of the fleet total.** That distinction is the whole term: if `u_i` were a
//!   share then `sum_i u_i = 1` by construction, `N_p^eff` could never exceed 1
//!   however large the network grew, and `ln(1 + N_p^eff)` would be pinned at
//!   `ln 2` forever. A test pins this
//!   ([`tests::effective_peers_must_grow_with_a_healthy_fleet`]) because the bug
//!   is invisible in every reading — the number just quietly stops moving.
//! * `v_i` — **verification confidence**: how much of what it claimed was
//!   independently checked and accepted. A peer that says "I solved it" and
//!   cannot show the work approaches 0.
//! * `r_i` — **reliability**: is it there right now, and has it been?
//!
//! The product, not the sum: a peer that fails any one of the three is worth
//! nothing regardless of the other two, which is exactly the intended semantics.
//! Loud, useless, and absent are all the same to a network.
//!
//! ## Why the logarithm
//!
//! Going from 10 to 100 effective peers changes what the network is. Going from
//! 1,000,000 to 1,000,100 changes nothing anybody can feel. `ln(1 + N)` encodes
//! that: it is strictly increasing (more real peers are always better), it is 0
//! at `N = 0` (a network of nobody has no amplification, and multiplies the
//! functional to zero rather than quietly contributing 1), and its derivative
//! falls as `1/N` so the marginal peer stops mattering exactly the way marginal
//! peers actually stop mattering.

use flux_kgauge::provenance::Provenance;
use serde::{Deserialize, Serialize};

/// One participant, already reduced to the three weights.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Stable identifier (wallet, rig name, peer id) — reported in the top-N
    /// contributors so an operator can see who is actually carrying the network.
    pub id: String,
    /// `u_i` — this peer's own useful-work score, `[0, 1]`. NOT a share of the
    /// fleet total; see the module doc for why that distinction matters.
    pub useful: f64,
    /// `v_i` — verification confidence, `[0, 1]`.
    pub verified: f64,
    /// `r_i` — reliability / liveness, `[0, 1]`.
    pub reliable: f64,
}

impl Peer {
    pub fn new(id: &str, useful: f64, verified: f64, reliable: f64) -> Self {
        Self { id: id.to_string(), useful, verified, reliable }
    }

    /// `u_i * v_i * r_i`, each factor clamped into `[0, 1]` first so a caller
    /// that hands us a ratio above 1 (a rounding artifact, a miner briefly
    /// reporting more than the network total) cannot manufacture amplification.
    pub fn weight(&self) -> f64 {
        let c = |x: f64| if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
        c(self.useful) * c(self.verified) * c(self.reliable)
    }
}

/// The amplification term and the evidence behind it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Amplification {
    /// `N_p^eff = sum_i u_i v_i r_i`.
    pub effective_peers: f64,
    /// How many peers were supplied at all.
    pub raw_peers: usize,
    /// `P_P2P = ln(1 + N_p^eff)`.
    pub p2p: f64,
    /// `effective_peers / raw_peers` — 1.0 means every peer is pulling full
    /// weight; 0.1 means the fleet is nine-tenths decoration. This ratio is the
    /// honest headline when someone asks "how big is the network".
    pub quality: f64,
    /// Highest-weight contributors, worst-to-best trimmed to the top 5.
    pub top: Vec<(String, f64)>,
    pub provenance: Provenance,
}

/// Reduce a peer set to `N_p^eff` and `P_P2P`.
pub fn amplification(peers: &[Peer], provenance: Provenance) -> Amplification {
    let mut weights: Vec<(String, f64)> = peers.iter().map(|p| (p.id.clone(), p.weight())).collect();
    let effective: f64 = weights.iter().map(|(_, w)| w).sum();
    weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    weights.truncate(5);
    Amplification {
        effective_peers: effective,
        raw_peers: peers.len(),
        p2p: (1.0 + effective).ln(),
        quality: if peers.is_empty() { 0.0 } else { effective / peers.len() as f64 },
        top: weights,
        provenance: if peers.is_empty() { Provenance::Unavailable } else { provenance },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_failing_any_one_factor_is_worth_nothing() {
        assert_eq!(Peer::new("loud", 1.0, 0.0, 1.0).weight(), 0.0, "unverifiable");
        assert_eq!(Peer::new("idle", 0.0, 1.0, 1.0).weight(), 0.0, "contributes no work");
        assert_eq!(Peer::new("gone", 1.0, 1.0, 0.0).weight(), 0.0, "not there");
    }

    #[test]
    fn sybils_do_not_amplify() {
        // 1000 peers that claim everything and prove nothing.
        let sybils: Vec<Peer> = (0..1000).map(|i| Peer::new(&format!("s{i}"), 1.0, 0.0, 1.0)).collect();
        let three_real = vec![
            Peer::new("a", 1.0, 1.0, 1.0),
            Peer::new("b", 1.0, 1.0, 1.0),
            Peer::new("c", 1.0, 1.0, 1.0),
        ];
        let fake = amplification(&sybils, Provenance::Measured);
        let real = amplification(&three_real, Provenance::Measured);
        assert_eq!(fake.effective_peers, 0.0);
        assert!(real.p2p > fake.p2p, "3 verified peers beat 1000 unverified ones");
        assert_eq!(fake.raw_peers, 1000, "the raw count is still reported — honestly, and uselessly");
    }

    #[test]
    fn logarithm_has_the_diminishing_shape_the_derivation_claims() {
        let mk = |n: usize| -> f64 {
            let ps: Vec<Peer> = (0..n).map(|i| Peer::new(&format!("p{i}"), 1.0, 1.0, 1.0)).collect();
            amplification(&ps, Provenance::Measured).p2p
        };
        let gain_10_to_100 = mk(100) - mk(10);
        let gain_1000_to_1090 = mk(1090) - mk(1000);
        assert!(gain_10_to_100 > 2.0, "10 -> 100 is a real change: {gain_10_to_100}");
        assert!(gain_1000_to_1090 < 0.1, "1000 -> 1090 is noise: {gain_1000_to_1090}");
        assert!(mk(1090) > mk(1000), "but still strictly increasing");
    }

    #[test]
    fn empty_network_has_zero_amplification_and_says_it_has_no_data() {
        let a = amplification(&[], Provenance::Measured);
        assert_eq!(a.p2p, 0.0);
        assert_eq!(a.quality, 0.0);
        assert_eq!(a.provenance, Provenance::Unavailable);
    }

    #[test]
    fn quality_exposes_a_fleet_that_is_mostly_decoration() {
        let mut ps = vec![Peer::new("real", 1.0, 1.0, 1.0)];
        ps.extend((0..9).map(|i| Peer::new(&format!("dead{i}"), 0.0, 0.0, 0.0)));
        let a = amplification(&ps, Provenance::Measured);
        assert_eq!(a.raw_peers, 10);
        assert!((a.quality - 0.1).abs() < 1e-12);
        assert_eq!(a.top[0].0, "real");
    }

    #[test]
    fn out_of_range_weights_cannot_manufacture_amplification() {
        let cheat = Peer::new("cheat", 1e9, 1e9, 1e9);
        assert_eq!(cheat.weight(), 1.0);
        assert_eq!(Peer::new("nan", f64::NAN, 1.0, 1.0).weight(), 0.0);
    }

    #[test]
    fn effective_peers_must_grow_with_a_healthy_fleet() {
        // The bug this pins: if `useful` is read as a SHARE of fleet work, then
        // sum(u_i) == 1 for any fleet size, N_p^eff is stuck at 1.0, and the
        // amplification term is frozen at ln(2) no matter how large the network
        // becomes. Nothing in a reading looks wrong — the number simply stops
        // responding, which is the worst kind of gauge failure.
        let mk = |n: usize| -> Amplification {
            let ps: Vec<Peer> = (0..n).map(|i| Peer::new(&format!("p{i}"), 1.0, 1.0, 1.0)).collect();
            amplification(&ps, Provenance::Measured)
        };
        assert!((mk(10).effective_peers - 10.0).abs() < 1e-12);
        assert!((mk(500).effective_peers - 500.0).abs() < 1e-12);
        assert!(mk(500).p2p > mk(10).p2p * 1.5, "the term must actually respond to fleet size");
        assert!((mk(500).quality - 1.0).abs() < 1e-12, "a fully healthy fleet is quality 1.0");
    }
}
