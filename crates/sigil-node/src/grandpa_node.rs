//! Roadmap step 1, M2.2 (2026-10-08): GRANDPA finality in the node, behind
//! `SIGIL_FINALITY_PROTOCOL=grandpa` (rehearsal network only; unset = nothing here runs).
//!
//! The round logic is the reference voter (`finality-grandpa`, via `sigil-grandpa`). This
//! module is the plumbing to the node, and nothing more:
//!
//! * **Blocks in.** Every block the node accepts into its DAG store is reported by
//!   `dag::BLOCK_OBSERVER` and added to the finality tree (parked until its parent is
//!   there). The tree is the `parent_hash` tree: settlement applies spine blocks only.
//! * **Fork choice out.** [`GrandpaNode::set_preferred`] gives the voter the braid's
//!   selected tip, held back below any block still waiting for parents, so the voter only
//!   prevotes for what this node can fully validate (review 10-08, Q3).
//! * **Messages.** GRANDPA's JSON (tagged `"grandpa"`) rides the existing finality-votes
//!   topic; the old per-height votes are not cast while this runs.
//! * **Finality in.** A finalized `(hash, height)` arrives on a channel; the node raises the
//!   braid's line with `set_certified` (fork choice follows it with
//!   `SIGIL_DAG_CERT_FORK_CHOICE=1`) and settles. With `SIGIL_DAG_DEPTH_FINALITY=0` this is
//!   the ONLY thing that finalizes, so a split stops finality instead of forking it.
//! * **Restart.** The signer guard (`<snapshot dir>/grandpa-guard.json`) is loaded before
//!   anything is signed; the voter restarts ABOVE the newest round it ever signed in, from
//!   the node's settled tip, and catches up from peers. A voter error is logged and the
//!   voter restarted the same way.
//!
//! The voter runs on its own OS thread (`futures::executor::block_on`) so a slow round can
//! never delay block production on the tokio loop.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sigil_grandpa::parking_lot::Mutex;
use sigil_dagknight::Braid;
use sigil_grandpa::fg::voter::Voter;
use sigil_grandpa::fg::voter_set::VoterSet;
use sigil_grandpa::gossip::GossipTransport;
use sigil_grandpa::{Hash, Number, SigilEnv, SignerGuard, SpineTree};

/// Voter-set id. Fixed committee until roadmap step 2 (committee change).
const SET_ID: u64 = 1;

pub fn enabled() -> bool {
    std::env::var("SIGIL_FINALITY_PROTOCOL").ok().as_deref() == Some("grandpa")
}

pub struct GrandpaNode {
    env: Arc<SigilEnv<GossipTransport>>,
    net: Arc<GossipTransport>,
}

/// `(hash, height, round)` of every block the voter finalizes.
pub type FinalizedRx = tokio::sync::mpsc::UnboundedReceiver<(Hash, Number, u64)>;

impl GrandpaNode {
    /// Start the voter. `base` must be a FINAL block the node holds (its settled tip).
    /// `None` (with a log line) if the committee or this node's key is not configured.
    pub fn start(
        mgr: Arc<flux_p2p::NetworkManager>,
        genesis: Hash,
        base: (Hash, Number),
        snap_dir: &Path,
    ) -> Option<(Self, FinalizedRx)> {
        let (committee, key, _) = match sigil_finality::observer::env_config::from_env() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("🏁 grandpa: NOT started — SIGIL_FINALITY_* config: {e:?}");
                return None;
            }
        };
        let Some(key) = key else {
            eprintln!("🏁 grandpa: NOT started — this node holds no committee key (SIGIL_FINALITY_SIGN_SEED)");
            return None;
        };
        let voters = VoterSet::new(committee.members_iter().map(|(_, pk)| (pk, 1u64)))?;
        let gossip = Duration::from_millis(
            std::env::var("SIGIL_GRANDPA_GOSSIP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(250),
        );
        let guard_path = snap_dir.join("grandpa-guard.json");
        let guard = SignerGuard::load(guard_path.clone(), SET_ID);
        if guard.is_poisoned() {
            eprintln!("🛑 grandpa: {} is unreadable — this validator signs NOTHING until it is repaired", guard_path.display());
        }
        let guard = Arc::new(Mutex::new(guard));
        let net = GossipTransport::new(
            SET_ID,
            Box::new(move |bytes| {
                let _ = mgr.publish(sigil_net::TOPIC_FINALITY_VOTES, bytes);
            }),
        );
        // 2026-10-17: the last completed round survives a restart, so this voter can still
        // catch lagging peers up after EVERY voter restarted (see `use_round_file`).
        let round_path = snap_dir.join("grandpa-round.json");
        match net.use_round_file(round_path.clone()) {
            Some(r) => eprintln!("🏁 grandpa: loaded completed round {r} from {} (catch-up source after restart)", round_path.display()),
            None => eprintln!("🏁 grandpa: no saved completed round yet ({})", round_path.display()),
        }
        let env = Arc::new(SigilEnv::new(
            SpineTree::with_root(base.0, base.1),
            key,
            genesis,
            SET_ID,
            net.clone(),
            guard.clone(),
            gossip,
            base,
            Arc::new(Mutex::new(Vec::new())),
        ));
        let (tx, finalized_rx) = tokio::sync::mpsc::unbounded_channel();
        env.set_on_finalize(Box::new(move |h, n, r| {
            let _ = tx.send((h, n, r));
        }));

        // Blocks in: one hook for every block the DAG store accepts.
        {
            let env = env.clone();
            let _ = crate::dag::BLOCK_OBSERVER.set(Box::new(move |hash, parent, height| {
                env.tree.lock().insert_or_park(hash, parent, height);
            }));
        }

        // The voter, on its own thread; restarted above its guard round after any error.
        {
            let (env, guard, net) = (env.clone(), guard.clone(), net.clone());
            std::thread::Builder::new()
                .name("sigil-grandpa".into())
                .spawn(move || loop {
                    let fin = env.last_finalized();
                    // 2026-10-17: resume in the round AFTER the last COMPLETED one, with its votes,
                    // as Substrate does; the signer guard repeats whatever was already signed in
                    // the rounds that follow. Starting above the newest SIGNED round skipped rounds
                    // this voter had only prevoted in, and after every voter restarted no round
                    // could finish (r1, 10-17). No saved round yet: the old way.
                    let (last_round, last_votes, last_base) = match net.completed() {
                        Some((r, base, votes)) if r > 0 => {
                            if env.tree.lock().contains(&base.0) { (r, votes, base) } else { (r, Vec::new(), fin) }
                        }
                        _ => (guard.lock().newest_round(), Vec::new(), fin),
                    };
                    eprintln!("🏁 grandpa: voter starting above round {last_round} ({} of its votes, guard newest {}) from finalized H={} (n={}, gossip {} ms)",
                        last_votes.len(), guard.lock().newest_round(), fin.1, voters.len(), gossip.as_millis());
                    // 2026-10-17: and say again what we signed in our last two rounds, for peers
                    // still in them (identical messages — the guard decides, Ed25519 repeats).
                    let n = env.reannounce_recorded_votes(2);
                    if n > 0 {
                        eprintln!("🏁 grandpa: re-announced {n} vote(s) this validator signed before the restart");
                    }
                    let voter = Voter::new(env.clone(), voters.clone(), env.global_comms(), last_round, last_votes, last_base, fin);
                    match sigil_grandpa::futures::executor::block_on(voter) {
                        Ok(()) => eprintln!("🏁 grandpa: voter ended; restarting"),
                        Err(e) => eprintln!("⚠ grandpa: voter error {e:?}; restarting in 1 s"),
                    }
                    std::thread::sleep(Duration::from_secs(1));
                })
                .ok()?;
        }
        // Rebroadcast + neighbor lines (repairs loss, exposes hidden equivocation, triggers
        // catch-up for peers that fell behind).
        {
            let net = net.clone();
            std::thread::Builder::new()
                .name("sigil-grandpa-tick".into())
                .spawn(move || loop {
                    std::thread::sleep(gossip * 4);
                    net.tick();
                })
                .ok()?;
        }
        eprintln!("🏁 grandpa: finality protocol ON (set {SET_ID}, base H={}, guard {})", base.1, guard_path.display());
        Some((Self { env, net }, finalized_rx))
    }

    /// A message from the finality-votes topic. Non-GRANDPA payloads are ignored.
    pub fn on_gossip(&self, data: &[u8]) {
        self.net.on_wire(data);
    }

    /// The voter's fork choice: the braid's selected tip, or — while the braid is still
    /// fetching parents — the spine block below where a missing parent can sit, so the node
    /// never prevotes for a block it cannot fully validate.
    pub fn set_preferred(&self, br: &Braid) {
        let Some(tip) = br.selected_tip() else { return };
        let Some(tip_h) = br.height_of(&tip) else { return };
        let reach = sigil_dagknight::braid::CERT_HOLD_PARENT_REACH;
        let pick = match br.pending_floor_height() {
            Some(floor) if floor.saturating_sub(1 + reach) < tip_h => {
                let h = floor.saturating_sub(1 + reach);
                br.spine_block_at(h).map(|b| (b, h))
            }
            _ => Some((tip, tip_h)),
        };
        *self.env.preferred.lock() = pick;
    }

    pub fn last_finalized(&self) -> (Hash, Number) {
        self.env.last_finalized()
    }
}

/// Raise the braid's finality line to the newest GRANDPA-finalized block, if the braid can
/// place it yet (it may not hold the block, or its fork choice may not have followed yet).
/// Keeps it pending otherwise; callers retry on every braid change.
pub fn apply_final(br: &mut Braid, pending: &mut Option<(Number, Hash)>) {
    use sigil_dagknight::CertifyOutcome as C;
    let Some((n, h)) = *pending else { return };
    match br.set_certified(n, h) {
        C::Raised | C::AlreadyCovered => *pending = None,
        C::Followed => {
            eprintln!("🧭 grandpa: finalized H={n} was off our spine — fork choice now follows it");
            *pending = None;
        }
        C::ConflictsWithFinal => {
            eprintln!("🚨 grandpa: finalized H={n} CONFLICTS with this node's finalized prefix — refused");
            *pending = None;
        }
        C::AheadOfTip | C::NotOnSpine => {}
    }
}
