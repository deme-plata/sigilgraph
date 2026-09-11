//! Choosing a peer that can actually answer.
//!
//! # Why this is its own module
//!
//! Backfill peer selection used to be six inline lines repeated in two loops: sort every
//! connected peer by score, rotate among the top three, send. It never asked whether the peer
//! held the blocks being requested, so a peer we had never heard a height from ranked exactly
//! like one we had measured serving 400 KB chunks a second earlier.
//!
//! Measured live on 2026-09-11, and it is the whole redundancy story of this chain: happysrv knew
//! exactly ONE peer's height — Epsilon, 6M blocks ahead and demonstrably serving — and spent every
//! request on two OTHER peers whose heights it had never heard. Those answered with timeouts and
//! short frames the decoder reported as `io error: unexpected end of file`, which reads like a
//! transport fault and is nothing of the kind. It is the shape of asking someone a question they
//! cannot answer. The node sat at height 7,504 for days while the answer was one peer away.
//!
//! A node that cannot fetch history cannot join, and a chain whose second node cannot join has no
//! redundancy however many nodes are listed in it. That makes this rule consensus-adjacent
//! infrastructure, not a heuristic, so it lives in one place with tests rather than twice inline.
//!
//! # The rule
//!
//! 1. Prefer peers whose **advertised height covers the range end**. They are the only ones that
//!    can answer.
//! 2. If **no peer has advertised any height**, fall back to all connected peers. This is the
//!    bootstrap case: asking is the only way to learn, and a fresh node must never be deadlocked
//!    into sending no requests at all.
//! 3. If heights **are** known and none covers the range, choose **nobody**. Asking anyway burns a
//!    request slot here and one there, and the reply is a timeout or a short frame either way.
//!
//! Within the candidates, lowest score first (score counts failures), rotating among the best few
//! so a healthy set shares the load and rotation never hands work back to a peer known to fail.

use std::collections::HashMap;

/// How many equally-good candidates the rotation spreads across.
const ROTATE_ACROSS: usize = 3;

/// Pick a peer able to serve a range ending at `range_end`, or `None` when none can.
///
/// `peers` are the currently connected peer ids, `heights` their last advertised chain heights
/// (from the peer-heights gossip topic), `scores` the failure counts, and `rr` a
/// monotonically-increasing rotation counter owned by the caller.
pub fn choose_capable_peer(
    peers: &[String],
    heights: &HashMap<String, u64>,
    scores: &HashMap<String, i32>,
    range_end: u64,
    rr: usize,
) -> Option<String> {
    if peers.is_empty() {
        return None;
    }
    let known_any = peers.iter().any(|p| heights.contains_key(p));
    let mut cand: Vec<String> = peers
        .iter()
        .filter(|p| heights.get(*p).is_some_and(|h| *h >= range_end))
        .cloned()
        .collect();

    if cand.is_empty() {
        if known_any {
            // Rule 3: somebody's height is known and nobody holds this. Silence beats noise.
            return None;
        }
        // Rule 2: bootstrap — no gossip has arrived yet.
        cand = peers.to_vec();
    }

    cand.sort_by_key(|p| *scores.get(p).unwrap_or(&0));
    let best = cand.len().min(ROTATE_ACROSS);
    Some(cand[rr % best].clone())
}

/// How long a range that just failed must wait before it is asked for again.
///
/// 250 ms doubling per consecutive failure, capped at 8 s. The cap matters as much as the
/// growth: a range must keep being retried, because the chain cannot become contiguous without
/// it, but it must stop drowning the link while it does. Measured on happysrv before this
/// existed: the same range re-requested about ten times a SECOND, every failure triggering an
/// instant retry, which is how one stalled range became a storm that looked like a transport
/// fault.
pub fn retry_hold(consecutive_failures: u32) -> std::time::Duration {
    std::time::Duration::from_millis(250u64.saturating_mul(1u64 << consecutive_failures.min(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }
    fn sc(pairs: &[(&str, i32)]) -> HashMap<String, i32> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }
    fn peers(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|p| (*p).to_string()).collect()
    }

    /// THE HAPPYSRV CASE, exactly as measured. One peer is known to be 6M blocks ahead; two
    /// others have never advertised anything. The old code rotated across all three and mostly
    /// asked the two that could not answer. The capable one must win every time.
    #[test]
    fn a_peer_known_to_hold_the_range_beats_peers_of_unknown_height() {
        let ps = peers(&["epsilon", "unknown_a", "unknown_b"]);
        let heights = h(&[("epsilon", 6_012_551)]);
        let scores = sc(&[]);
        for rr in 0..10 {
            assert_eq!(
                choose_capable_peer(&ps, &heights, &scores, 7_760, rr).as_deref(),
                Some("epsilon"),
                "the only peer known to hold the range must be chosen on every rotation"
            );
        }
    }

    /// Rule 3. If every height is known and none reaches the range, asking is pure noise — it
    /// costs a slot on both sides and cannot succeed.
    #[test]
    fn nobody_is_chosen_when_known_heights_do_not_cover_the_range() {
        let ps = peers(&["a", "b"]);
        let heights = h(&[("a", 100), ("b", 250)]);
        assert_eq!(choose_capable_peer(&ps, &heights, &sc(&[]), 5_000, 0), None);
        // …and the same peers ARE acceptable for a range they do cover.
        assert!(choose_capable_peer(&ps, &heights, &sc(&[]), 200, 0).is_some());
    }

    /// Rule 2. A fresh node has heard no gossip yet; refusing to ask anyone would deadlock it
    /// into never syncing, which would be a worse bug than the one this module fixes.
    #[test]
    fn with_no_advertised_heights_at_all_it_still_asks_someone() {
        let ps = peers(&["a", "b"]);
        let chosen = choose_capable_peer(&ps, &h(&[]), &sc(&[]), 5_000, 0);
        assert!(chosen.is_some(), "bootstrap must not be deadlocked by the capability rule");
    }

    /// A failing peer must not keep getting the work, even when it is capable.
    #[test]
    fn among_capable_peers_the_least_failing_is_preferred() {
        let ps = peers(&["flaky", "good"]);
        let heights = h(&[("flaky", 9_000), ("good", 9_000)]);
        let scores = sc(&[("flaky", 40), ("good", 0)]);
        assert_eq!(
            choose_capable_peer(&ps, &heights, &scores, 8_000, 0).as_deref(),
            Some("good")
        );
    }

    /// Load is shared across equally-good capable peers rather than piled on the first one.
    #[test]
    fn rotation_spreads_work_across_equally_good_capable_peers() {
        let ps = peers(&["a", "b", "c"]);
        let heights = h(&[("a", 9_000), ("b", 9_000), ("c", 9_000)]);
        let seen: std::collections::HashSet<_> = (0..6)
            .filter_map(|rr| choose_capable_peer(&ps, &heights, &sc(&[]), 8_000, rr))
            .collect();
        assert!(seen.len() > 1, "rotation must not pin every request on one peer");
    }

    /// The backoff must actually widen, and must stop widening. An uncapped doubling would
    /// eventually park a required range for hours and stall the chain permanently; no widening
    /// at all is the storm this was written to stop.
    #[test]
    fn retry_hold_widens_then_caps() {
        let ms = |n| retry_hold(n).as_millis();
        assert_eq!(ms(0), 250);
        assert_eq!(ms(1), 500);
        assert_eq!(ms(2), 1_000);
        assert_eq!(ms(5), 8_000);
        assert_eq!(ms(50), 8_000, "capped — a needed range must keep being retried");
        assert!(ms(3) > ms(2), "each consecutive failure waits longer");
    }

    /// No peers, no choice. Guards the indexing below the rotation.
    #[test]
    fn no_peers_means_no_choice() {
        assert_eq!(choose_capable_peer(&[], &h(&[]), &sc(&[]), 10, 0), None);
    }
}
