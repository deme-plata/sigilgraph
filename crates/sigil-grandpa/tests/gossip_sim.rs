//! M2.2 groundwork (2026-10-08): the same reference voter, but over the REAL transport
//! (`GossipTransport`) — no message history in the network, only what each node buffers,
//! rebroadcasts and hands out as catch-up. The bus between nodes can take a node offline,
//! split the net, and lose messages at random.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use futures::executor::LocalPool;
use futures::future::{abortable, AbortHandle};
use futures::task::LocalSpawnExt;
use futures::FutureExt;
use parking_lot::Mutex;
use rand::Rng;
use sigil_grandpa::fg::voter::Voter;
use sigil_grandpa::fg::voter_set::VoterSet;
use sigil_grandpa::fg::Chain;
use sigil_grandpa::gossip::GossipTransport;
use sigil_grandpa::*;

const GOSSIP: Duration = Duration::from_millis(20);
const SET_ID: u64 = 1;
const GENESIS: Hash = [0u8; 32];

fn bh(branch: u8, n: u64) -> Hash {
    let mut h = [0u8; 32];
    h[0] = branch;
    h[1..9].copy_from_slice(&n.to_le_bytes());
    h
}

fn key(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i + 1; 32])
}

#[derive(Default)]
struct BusState {
    queue: VecDeque<(usize, Vec<u8>)>,
    online: Vec<bool>,
    group: Option<Vec<u8>>,
    loss: f64,
    delivered: u64,
    dropped: u64,
}

#[derive(Default)]
struct Bus(Mutex<BusState>);

struct Harness {
    pool: LocalPool,
    bus: Arc<Bus>,
    nets: Vec<Arc<GossipTransport>>,
    envs: Vec<Arc<SigilEnv<GossipTransport>>>,
    guards: Vec<Arc<Mutex<SignerGuard>>>,
    aborts: Vec<Option<AbortHandle>>,
    voters: VoterSet<Id>,
    truth: SpineTree,
    log: Arc<Mutex<Vec<(Id, Hash, Number)>>>,
    tips: HashMap<u8, (Hash, Number)>,
    errors: Arc<Mutex<Vec<String>>>,
    last_tick: Instant,
}

impl Harness {
    fn new() -> Self {
        let bus = Arc::new(Bus::default());
        bus.0.lock().online = vec![true; 4];
        let voters = VoterSet::new((0..4u8).map(|i| (key(i).verifying_key().to_bytes(), 1))).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut h = Harness {
            pool: LocalPool::new(),
            bus: bus.clone(),
            nets: Vec::new(),
            envs: Vec::new(),
            guards: Vec::new(),
            aborts: Vec::new(),
            voters,
            truth: SpineTree::new(GENESIS),
            log: log.clone(),
            tips: HashMap::new(),
            errors: Arc::new(Mutex::new(Vec::new())),
            last_tick: Instant::now(),
        };
        for i in 0..4usize {
            let b = bus.clone();
            let net = GossipTransport::new(SET_ID, Box::new(move |bytes| b.0.lock().queue.push_back((i, bytes))));
            let guard = Arc::new(Mutex::new(SignerGuard::in_memory(SET_ID)));
            let env = Arc::new(SigilEnv::new(
                SpineTree::new(GENESIS),
                key(i as u8),
                GENESIS,
                SET_ID,
                net.clone(),
                guard.clone(),
                GOSSIP,
                (GENESIS, 0),
                log.clone(),
            ));
            h.nets.push(net);
            h.envs.push(env);
            h.guards.push(guard);
            h.aborts.push(None);
        }
        for i in 0..4 {
            h.start(i, false);
        }
        h
    }

    /// `from_guard`: restart the way the node will — above the newest round this voter ever
    /// signed in, from its last finalized block, with no votes in hand.
    fn start(&mut self, i: usize, from_guard: bool) {
        let env = self.envs[i].clone();
        let fin = env.last_finalized();
        let last_round = if from_guard { self.guards[i].lock().newest_round() } else { 0 };
        let voter = Voter::new(env.clone(), self.voters.clone(), env.global_comms(), last_round, Vec::new(), fin, fin);
        let (fut, handle) = abortable(voter);
        let errors = self.errors.clone();
        self.pool
            .spawner()
            .spawn_local(fut.map(move |r| {
                if let Ok(Err(e)) = r {
                    errors.lock().push(format!("voter {i} died: {e:?}"));
                }
            }))
            .unwrap();
        self.aborts[i] = Some(handle);
    }

    fn crash(&mut self, i: usize) {
        if let Some(h) = self.aborts[i].take() {
            h.abort();
        }
    }

    fn pump(&self) {
        let mut rng = rand::thread_rng();
        loop {
            let item = self.bus.0.lock().queue.pop_front();
            let Some((from, bytes)) = item else { break };
            let (online, group, loss) = {
                let s = self.bus.0.lock();
                (s.online.clone(), s.group.clone(), s.loss)
            };
            if !online[from] {
                continue;
            }
            for (j, net) in self.nets.iter().enumerate() {
                if j == from || !online[j] {
                    continue;
                }
                if group.as_ref().is_some_and(|g| g[j] != g[from]) {
                    continue;
                }
                if loss > 0.0 && rng.gen_bool(loss) {
                    self.bus.0.lock().dropped += 1;
                    continue;
                }
                self.bus.0.lock().delivered += 1;
                net.on_wire(&bytes);
            }
        }
    }

    fn grow(&mut self, branch: u8, parent: (Hash, Number), to: &[usize]) -> (Hash, Number) {
        let (ph, pn) = *self.tips.get(&branch).unwrap_or(&parent);
        let b = (bh(branch, pn + 1), pn + 1);
        assert!(self.truth.insert(b.0, ph, b.1));
        for &i in to {
            // Blocks arrive through the node's sync, not the vote gossip; parking handles order.
            self.envs[i].tree.lock().insert_or_park(b.0, ph, b.1);
        }
        self.tips.insert(branch, b);
        b
    }

    /// Restart voter `i` the way a PROCESS restart does (2026-10-17): new transport and
    /// environment, so the round buffers and the completed round held in memory are gone.
    /// Survives: the signer guard (on disk) and, with `round_dir`, the saved completed round.
    /// `reannounce`: re-send the votes the guard recorded in the last two rounds.
    fn restart_process(&mut self, i: usize, round_dir: Option<&std::path::Path>, reannounce: bool) {
        self.crash(i);
        let b = self.bus.clone();
        let net = GossipTransport::new(SET_ID, Box::new(move |bytes| b.0.lock().queue.push_back((i, bytes))));
        if let Some(d) = round_dir {
            net.use_round_file(d.join(format!("round-{i}.json")));
        }
        let fin = self.envs[i].last_finalized();
        let mut tree = SpineTree::new(GENESIS);
        for (h, p, n) in self.all_blocks() {
            tree.insert_or_park(h, p, n); // the node re-learns its blocks through sync
        }
        let env = Arc::new(SigilEnv::new(
            tree,
            key(i as u8),
            GENESIS,
            SET_ID,
            net.clone(),
            self.guards[i].clone(),
            GOSSIP,
            fin,
            self.log.clone(),
        ));
        self.nets[i] = net;
        self.envs[i] = env;
        if reannounce {
            self.envs[i].reannounce_recorded_votes(2);
        }
        // With a saved completed round: resume after it, with its votes (the node's rule since
        // 10-17). Without: above the newest round the guard signed in (the old rule).
        match self.nets[i].completed().filter(|_| round_dir.is_some()) {
            Some((r, base, votes)) => {
                let env = self.envs[i].clone();
                let voter = Voter::new(env.clone(), self.voters.clone(), env.global_comms(), r, votes, base, fin);
                let (fut, handle) = abortable(voter);
                let errors = self.errors.clone();
                self.pool
                    .spawner()
                    .spawn_local(fut.map(move |r| {
                        if let Ok(Err(e)) = r {
                            errors.lock().push(format!("voter {i} died: {e:?}"));
                        }
                    }))
                    .unwrap();
                self.aborts[i] = Some(handle);
            }
            None => self.start(i, true),
        }
    }

    fn all_blocks(&self) -> Vec<(Hash, Hash, Number)> {
        let mut all: Vec<(Hash, Hash, Number)> = Vec::new();
        for t in self.tips.values() {
            let mut cur = t.0;
            while let Some(n) = self.truth.number(&cur) {
                if n == 0 {
                    break;
                }
                let parent = self.truth.ancestry(GENESIS, cur).ok().and_then(|a| a.first().copied()).unwrap_or(GENESIS);
                all.push((cur, parent, n));
                cur = parent;
            }
        }
        all.reverse();
        all
    }

    fn sync_all(&self) {
        let mut all: Vec<(Hash, Hash, Number)> = Vec::new();
        for t in self.tips.values() {
            let mut cur = t.0;
            while let Some(n) = self.truth.number(&cur) {
                if n == 0 {
                    break;
                }
                let parent = self.truth.ancestry(GENESIS, cur).ok().and_then(|a| a.first().copied()).unwrap_or(GENESIS);
                all.push((cur, parent, n));
                cur = parent;
            }
        }
        for e in &self.envs {
            let mut t = e.tree.lock();
            for (h, p, n) in &all {
                t.insert_or_park(*h, *p, *n);
            }
        }
    }

    fn run_for(&mut self, d: Duration, mut tick: impl FnMut(&mut Harness)) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            tick(self);
            if self.last_tick.elapsed() >= GOSSIP * 4 {
                self.last_tick = Instant::now();
                for n in &self.nets {
                    n.tick();
                }
            }
            for _ in 0..4 {
                self.pump();
                self.pool.run_until_stalled();
            }
            self.pool.run_until(futures_timer::Delay::new(Duration::from_millis(5)));
        }
    }

    fn finalized(&self) -> Vec<Number> {
        self.envs.iter().map(|e| e.last_finalized().1).collect()
    }

    fn assert_safe(&self) {
        assert!(self.errors.lock().is_empty(), "voter task errors: {:?}", *self.errors.lock());
        for e in &self.envs {
            let v = e.safety_violations.lock();
            assert!(v.is_empty(), "local safety violation: {:?}", *v);
        }
        let log = self.log.lock();
        for (_, a, an) in log.iter() {
            for (_, b, bn) in log.iter() {
                let ok = if an <= bn { self.truth.is_equal_or_descendent_of(*a, *b) } else { self.truth.is_equal_or_descendent_of(*b, *a) };
                assert!(ok, "CONFLICTING FINALITY: {}@{an} vs {}@{bn}", hex::encode(&a[..2]), hex::encode(&b[..2]));
            }
        }
        for e in &self.envs {
            assert!(e.equivocations.lock().is_empty(), "an honest voter was seen double-voting");
        }
    }
}

#[test]
fn over_gossip_four_voters_finalize_a_growing_chain() {
    let mut h = Harness::new();
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (tip, fin) = (h.tips[&1].1, h.finalized());
    eprintln!("gossip: tip {tip}, finalized {fin:?}, rounds {:?}", h.nets.iter().map(|n| n.my_round()).collect::<Vec<_>>());
    assert!(fin.iter().all(|f| *f + 40 >= tip && *f > 0), "tip {tip}, {fin:?}");
    h.assert_safe();
}

#[test]
fn over_gossip_an_offline_voter_catches_up_when_it_returns() {
    let mut h = Harness::new();
    h.run_for(Duration::from_millis(500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    h.bus.0.lock().online[3] = false;
    let r_before = h.nets[3].my_round();
    h.run_for(Duration::from_millis(2000), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (tip, fin) = (h.tips[&1].1, h.finalized());
    let others = h.nets[0].my_round();
    eprintln!("offline: tip {tip}, finalized {fin:?}; voter 3 round {} vs others {others}", h.nets[3].my_round());
    assert!(fin[..3].iter().all(|f| *f + 40 >= tip), "3 of 4 is a quorum: finality continues without voter 3");
    h.bus.0.lock().online[3] = true;
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (tip, fin) = (h.tips[&1].1, h.finalized());
    let r_after = h.nets[3].my_round();
    eprintln!("back: tip {tip}, finalized {fin:?}; voter 3 round {r_before} -> {r_after} (others {})", h.nets[0].my_round());
    assert!(fin[3] + 40 >= tip, "voter 3 caught up: {fin:?} vs tip {tip}");
    assert!(r_after + 3 >= h.nets[0].my_round(), "voter 3 jumped to the current round");
    h.assert_safe();
}

#[test]
fn over_gossip_a_voter_restarted_above_its_guard_round_rejoins() {
    let mut h = Harness::new();
    h.run_for(Duration::from_millis(600), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    h.crash(0);
    h.run_for(Duration::from_millis(800), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let newest = h.guards[0].lock().newest_round();
    h.start(0, true);
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (tip, fin) = (h.tips[&1].1, h.finalized());
    eprintln!("restart: guard round {newest}, tip {tip}, finalized {fin:?}");
    assert!(fin[0] + 40 >= tip, "restarted voter finalizes again: {fin:?} vs {tip}");
    h.assert_safe();
}

/// The reference voter's own debug lines (target "grandpa"), when `SIM_LOG=1`.
struct SimLog;
impl log::Log for SimLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("grandpa")
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("    [fg {}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}
static SIM_LOG: SimLog = SimLog;
fn sim_log_on() {
    if std::env::var("SIM_LOG").ok().as_deref() == Some("1") {
        let _ = log::set_logger(&SIM_LOG).map(|()| log::set_max_level(log::LevelFilter::Trace));
    }
}

/// 2026-10-17, the r1 rehearsal end state: every voter restarted (memory lost) and they came
/// back spread over rounds — two in round R+1, one in R+2, one far behind. Returns
/// (finalized just after the restarts, finalized 2.5 s later, tip).
fn spread_restart(fix: bool) -> (Vec<Number>, Vec<Number>, Number) {
    let dir = std::env::temp_dir().join(format!("sigil-grandpa-spread-{}-{fix}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::new();
    if fix {
        for (i, n) in h.nets.iter().enumerate() {
            n.use_round_file(dir.join(format!("round-{i}.json")));
        }
    }
    h.run_for(Duration::from_millis(600), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    h.bus.0.lock().online[3] = false; // voter 3 falls far behind (r3 was ~220 rounds back)
    h.run_for(Duration::from_millis(600), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    for i in 0..4 {
        h.crash(i);
    }
    // r1's spread: voters 0 and 1 last signed in round R, voter 2 already in R+1.
    let r = (0..3).map(|i| h.guards[i].lock().newest_round()).max().unwrap();
    for (i, round) in [(0usize, r), (1, r), (2, r + 1)] {
        let fin = h.envs[i].last_finalized();
        if h.guards[i].lock().newest_round() < round {
            h.guards[i].lock().decide(round, fg::Message::Prevote(fg::Prevote::new(fin.0, fin.1)));
        }
    }
    h.bus.0.lock().online[3] = true;
    sim_log_on();
    for i in 0..4 {
        h.restart_process(i, fix.then_some(dir.as_path()), fix);
    }
    let before = h.finalized();
    h.run_for(Duration::from_millis(2500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (after, tip) = (h.finalized(), h.tips[&1].1);
    eprintln!("spread restart (fix={fix}): rounds {:?}, finalized {before:?} -> {after:?}, tip {tip}",
        h.nets.iter().map(|n| n.my_round()).collect::<Vec<_>>());
    for (i, n) in h.nets.iter().enumerate() {
        let rr = n.my_round();
        eprintln!("  node {i} round {rr}: (phase, target, voter) {:?} | {}: {:?} | {}: {:?}",
            n.round_summary(rr), rr - 1, n.round_summary(rr - 1), rr + 1, n.round_summary(rr + 1));
        let rec: Vec<(u64, u8)> = h.guards[i].lock().recorded(rr.saturating_sub(1)).iter().map(|(r, m)| (*r, match m {
            fg::Message::Prevote(_) => 0, fg::Message::Precommit(_) => 1, fg::Message::PrimaryPropose(_) => 2,
        })).collect();
        eprintln!("    guard {i} signed (round, phase) from {}: {rec:?} · last finalized {:?} · completed {:?}",
            rr.saturating_sub(1), h.envs[i].last_finalized().1, n.completed_round());
    }
    h.assert_safe();
    let _ = std::fs::remove_dir_all(&dir);
    (before, after, tip)
}

/// 2026-10-17 probe: after an offline voter returns and is caught up, does it precommit again?
#[test]
fn over_gossip_a_caught_up_voter_precommits_again() {
    let mut h = Harness::new();
    h.run_for(Duration::from_millis(500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    h.bus.0.lock().online[3] = false;
    h.run_for(Duration::from_millis(800), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let back_at = h.nets[0].my_round();
    h.bus.0.lock().online[3] = true;
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let rec = h.guards[3].lock().recorded(back_at + 2);
    let pc = rec.iter().filter(|(_, m)| matches!(m, fg::Message::Precommit(_))).count();
    let pv = rec.iter().filter(|(_, m)| matches!(m, fg::Message::Prevote(_))).count();
    eprintln!("caught-up voter 3: rounds {back_at}+2.. → {pv} prevotes, {pc} precommits signed; now round {}", h.nets[3].my_round());
    assert!(pc > 0, "a caught-up voter must precommit in later rounds ({pv} prevotes, {pc} precommits)");
}

#[test]
fn over_gossip_voters_restarted_into_different_rounds_stay_frozen_without_saved_rounds() {
    let (before, after, _) = spread_restart(false);
    assert_eq!(before, after, "the control: this is the r1 freeze — no round has 3 of 4 voters");
}

#[test]
fn over_gossip_voters_restarted_into_different_rounds_resume_with_saved_rounds() {
    let (before, after, tip) = spread_restart(true);
    assert!(after.iter().zip(&before).all(|(a, b)| a > b), "every voter finalizes again: {before:?} -> {after:?}");
    assert!(after.iter().all(|f| *f + 40 >= tip), "and keeps up with the tip: {after:?} vs {tip}");
}

#[test]
fn over_gossip_a_two_two_split_halts_and_resumes() {
    let mut h = Harness::new();
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(700), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]));
    h.bus.0.lock().group = Some(vec![0, 0, 1, 1]);
    h.run_for(Duration::from_millis(300), |h| {
        h.grow(2, root, &[0, 1]);
        h.grow(3, root, &[2, 3]);
    });
    let before = h.finalized();
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(2, root, &[0, 1]);
        h.grow(3, root, &[2, 3]);
    });
    let during = h.finalized();
    eprintln!("gossip split: before {before:?} during {during:?}");
    assert_eq!(before, during, "no side has 3 of 4: nothing is finalized while split");
    h.bus.0.lock().group = None;
    h.sync_all();
    h.run_for(Duration::from_millis(2000), |h| {
        h.grow(2, root, &[0, 1, 2, 3]);
    });
    let (tip, after) = (h.tips[&2].1, h.finalized());
    let rounds: Vec<u64> = h.nets.iter().map(|n| n.my_round()).collect();
    let completed: Vec<Option<u64>> = h.envs.iter().map(|e| e.restart.lock().as_ref().map(|r| r.last_round)).collect();
    let pref: Vec<Option<u64>> = h.envs.iter().map(|e| e.preferred.lock().map(|p| p.1)).collect();
    let trees: Vec<usize> = h.envs.iter().map(|e| e.tree.lock().len()).collect();
    eprintln!("gossip healed: tip {tip}, finalized {after:?}, rounds {rounds:?}, completed {completed:?}, preferred {pref:?}, tree sizes {trees:?}");
    for (i, n) in h.nets.iter().enumerate() {
        let r = n.my_round();
        eprintln!("  node {i} round {r}: {:?} | round {}: {:?}", n.round_summary(r), r - 1, n.round_summary(r - 1));
    }
    for (i, e) in h.envs.iter().enumerate() {
        let t = e.tree.lock();
        eprintln!("  node {i} tree: best_chain_containing(fin) = {:?}", t.best_chain_containing(e.last_finalized().0).map(|x| x.1));
    }
    assert!(after.iter().all(|f| *f + 40 >= tip), "finality resumes after the heal: {after:?} vs {tip}");
    h.assert_safe();
}

#[test]
fn over_gossip_thirty_percent_message_loss_is_repaired_by_rebroadcast() {
    let mut h = Harness::new();
    h.bus.0.lock().loss = 0.3;
    h.run_for(Duration::from_millis(2500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let (tip, fin) = (h.tips[&1].1, h.finalized());
    let (d, x) = { let s = h.bus.0.lock(); (s.delivered, s.dropped) };
    eprintln!("lossy: tip {tip}, finalized {fin:?}, delivered {d}, dropped {x}");
    assert!(fin.iter().all(|f| *f + 60 >= tip && *f > 0), "tip {tip}, {fin:?}");
    h.assert_safe();
}
