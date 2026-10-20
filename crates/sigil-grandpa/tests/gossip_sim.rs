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
    /// Every block grown: hash → (parent, number). What the network as a whole holds.
    parents: HashMap<Hash, (Hash, Number)>,
    /// D3 (2026-10-20): `(voter, hashes)` asks from each voter's block fetcher, served by
    /// [`Harness::serve_fetches`] the way `sigil-node` answers a by-hash request.
    fetch_q: Arc<Mutex<Vec<(usize, Vec<Hash>)>>>,
    fetch_on: bool,
    /// Blocks handed out by `serve_fetches`.
    served: u64,
}

/// What one by-hash answer carries at most (`sigil-node/src/parent_fetch.rs`, `BY_HASH_MAX`).
const BY_HASH_MAX: usize = 64;

impl Harness {
    fn new() -> Self {
        Self::with_fetch(false)
    }

    /// `fetch`: give every voter a block fetcher (D3); `false` = the pre-D3 node, which never
    /// asks for the blocks its held votes name.
    fn with_fetch(fetch: bool) -> Self {
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
            parents: HashMap::new(),
            fetch_q: Arc::new(Mutex::new(Vec::new())),
            fetch_on: fetch,
            served: 0,
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
            h.wire_fetch(i);
        }
        for i in 0..4 {
            h.start(i, false);
        }
        h
    }

    /// D3: voter `i`'s fetcher puts its asks on the harness queue (the node's: a channel to
    /// its main loop, which sends a by-hash request to a peer).
    fn wire_fetch(&self, i: usize) {
        if !self.fetch_on {
            return;
        }
        let q = self.fetch_q.clone();
        self.envs[i].set_block_fetcher(Box::new(move |hs| q.lock().push((i, hs))));
    }

    /// Answer every queued ask like a peer answers a by-hash request: the asked blocks with
    /// their ancestors, oldest first, at most [`BY_HASH_MAX`] per answer. (A real peer cannot
    /// see what the asker holds; stopping at the asker's first known ancestor only saves this
    /// harness work — the cap is what bounds a hop.) An offline voter's ask goes nowhere; its
    /// fetcher asks again later.
    fn serve_fetches(&mut self) {
        let asks: Vec<(usize, Vec<Hash>)> = std::mem::take(&mut *self.fetch_q.lock());
        let online = self.bus.0.lock().online.clone();
        for (i, hashes) in asks {
            if !online[i] {
                continue;
            }
            let mut answer: Vec<(Hash, Hash, Number)> = Vec::new();
            {
                let tree = self.envs[i].tree.lock();
                'asked: for h in hashes {
                    let mut cur = h;
                    let mut run = Vec::new();
                    while let Some(&(p, n)) = self.parents.get(&cur) {
                        if answer.len() + run.len() >= BY_HASH_MAX {
                            answer.extend(run.into_iter().rev());
                            break 'asked;
                        }
                        run.push((cur, p, n));
                        if p == GENESIS || tree.contains(&p) {
                            break;
                        }
                        cur = p;
                    }
                    answer.extend(run.into_iter().rev());
                }
            }
            let mut tree = self.envs[i].tree.lock();
            for (hh, p, n) in &answer {
                tree.insert_or_park(*hh, *p, *n);
            }
            self.served += answer.len() as u64;
        }
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
        self.parents.insert(b.0, (ph, b.1));
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
        self.wire_fetch(i);
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
                self.serve_fetches();
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

// ─── S1: the 10-17 r1 wedge (restart-liveness design 2026-10-17, §2 and §6) ─────────────
//
// What the frozen r1 left on disk (read 2026-10-18 from each node's grandpa-round.json and
// grandpa-guard.json, nothing written):
//
//   r1, r2, r3  last completed round 1530, base 7,452, prevotes 7,626 / 7,642 / 7,645 (three
//               branches), precommits 7,452 ×3. The SAME file on all three.
//               Round 1531 in the guards: r1 prevote 7,672 + precommit 7,452; r3 prevote 7,668 +
//               precommit 7,452; r2 prevote 7,662 and NO precommit.
//               Their logs: "voter starting above round 1530 (6 of its votes) from finalized
//               H=7452" — the saved round was restored with all its votes.
//   r4          last completed 1441 (base 6,796, "0 of its votes" at restart), guard newest 1442,
//               finalized 7,452 by commits, its braid missing the strands above 7,452.
//
// The scenario below gets there the way r1 did, not by planting votes: four voters with saved
// rounds (as the node runs since f047a0cc); voter 3 drops off and falls ~90 rounds behind; the
// chain splits into three branches that every remaining voter HOLDS but each PREFERS a different
// one (two producers whose strands never merged), so rounds keep completing with prevotes on
// three branches and precommits for the fork point — r1's round 1530 exactly. Then every voter
// is restarted the way `sigil-node/src/grandpa_node.rs` restarts one: a new transport, the round
// file, a finality tree rebuilt from the settled tip plus the blocks the node holds, "with its
// votes" only if that tree holds the saved base, the last two rounds re-announced, `Voter::new`.
// After the restart the producers continue on one branch and fork choice follows it; voter 3
// holds the settled chain up to the fork point (what range backfill gives it) but not the
// branches above it — the blocks every catch-up it is offered names.

/// Every block grown, with its parent and the voters it was delivered to (a node rebuilds its
/// finality tree from what it holds).
type Grown = Vec<(Hash, Hash, Number, Vec<usize>)>;

fn grow_rec(h: &mut Harness, rec: &mut Grown, branch: u8, root: (Hash, Number), to: &[usize]) -> (Hash, Number) {
    let parent = *h.tips.get(&branch).unwrap_or(&root);
    let b = h.grow(branch, root, to);
    rec.push((b.0, parent.0, b.1, to.to_vec()));
    b
}

/// Restart voter `i` exactly as `GrandpaNode::start` + its voter loop do (2026-10-17 code).
/// `holds` = the blocks this node's DAG store reports after the restart. Returns
/// `(round it resumes after, saved votes it resumed with)`.
fn restart_like_node(h: &mut Harness, i: usize, dir: &std::path::Path, holds: &[(Hash, Hash, Number)]) -> (u64, usize) {
    h.crash(i);
    let b = h.bus.clone();
    let net = GossipTransport::new(SET_ID, Box::new(move |bytes| b.0.lock().queue.push_back((i, bytes))));
    net.use_round_file(dir.join(format!("round-{i}.json")));
    // The node's GRANDPA base is its settled tip; in the sim, settled = finalized.
    let fin = h.envs[i].last_finalized();
    let mut tree = SpineTree::with_root(fin.0, fin.1);
    for (hh, p, n) in holds {
        if *n > fin.1 {
            tree.insert_or_park(*hh, *p, *n);
        }
    }
    let env = Arc::new(SigilEnv::new(tree, key(i as u8), GENESIS, SET_ID, net.clone(), h.guards[i].clone(), GOSSIP, fin, h.log.clone()));
    h.nets[i] = net.clone();
    h.envs[i] = env.clone();
    h.wire_fetch(i);
    let (last_round, last_votes, last_base) = match net.completed() {
        Some((r, base, votes)) if r > 0 => {
            if env.tree.lock().contains(&base.0) {
                (r, votes, base)
            } else {
                (r, Vec::new(), fin)
            }
        }
        _ => (h.guards[i].lock().newest_round(), Vec::new(), fin),
    };
    let n_votes = last_votes.len();
    env.reannounce_recorded_votes(2);
    let voter = Voter::new(env.clone(), h.voters.clone(), env.global_comms(), last_round, last_votes, last_base, fin);
    let (fut, handle) = abortable(voter);
    let errors = h.errors.clone();
    h.pool
        .spawner()
        .spawn_local(fut.map(move |r| {
            if let Ok(Err(e)) = r {
                errors.lock().push(format!("voter {i} died: {e:?}"));
            }
        }))
        .unwrap();
    h.aborts[i] = Some(handle);
    (last_round, n_votes)
}

/// Which branch a block is on (the harness puts the branch id in byte 0).
fn branch_of(hh: &Hash) -> u8 {
    hh[0]
}

fn describe_votes(votes: &[SigilSignedMessage]) -> String {
    let mut v: Vec<String> = votes
        .iter()
        .map(|m| {
            let (p, t, n) = match &m.message {
                fg::Message::Prevote(x) => ("pv", x.target_hash, x.target_number),
                fg::Message::Precommit(x) => ("pc", x.target_hash, x.target_number),
                fg::Message::PrimaryPropose(x) => ("pp", x.target_hash, x.target_number),
            };
            format!("{p} {n}/b{} by {:02x}", branch_of(&t), m.id[0])
        })
        .collect();
    v.sort();
    v.join(", ")
}

fn describe_guard(h: &Harness, i: usize, from: u64) -> String {
    h.guards[i]
        .lock()
        .recorded(from)
        .iter()
        .map(|(r, m)| match m {
            fg::Message::Prevote(x) => format!("r{r} pv {}/b{}", x.target_number, branch_of(&x.target_hash)),
            fg::Message::Precommit(x) => format!("r{r} pc {}/b{}", x.target_number, branch_of(&x.target_hash)),
            fg::Message::PrimaryPropose(x) => format!("r{r} pp {}", x.target_number),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

struct S1Outcome {
    fork_at: Number,
    before: Vec<Number>,
    after: Vec<Number>,
    tip: Number,
    rounds_after: Vec<u64>,
}

/// `hole`: after the restart, voter 1 no longer holds branch 4 (the branch voter 2 prevoted on
/// in the round they were restarted in) — S1b, see the second test.
/// `fetch`: the voters ask for the blocks their held messages name (D3, 2026-10-20).
fn s1_wedge(hole: bool, fetch: bool) -> S1Outcome {
    sim_log_on();
    let dir = std::env::temp_dir().join(format!("sigil-grandpa-s1-{}-{hole}-{fetch}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::with_fetch(fetch);
    for (i, n) in h.nets.iter().enumerate() {
        n.use_round_file(dir.join(format!("round-{i}.json")));
    }
    let mut rec: Grown = Vec::new();

    // 1. All four finalize a common chain.
    h.run_for(Duration::from_millis(600), |h| {
        grow_rec(h, &mut rec, 1, (GENESIS, 0), &[0, 1, 2, 3]);
    });

    // 2. Voter 3 drops off the network (votes AND blocks) and falls ~90 rounds behind.
    h.bus.0.lock().online[3] = false;
    let lag_target = 90u64;
    let t0 = Instant::now();
    while h.nets[0].my_round() < h.nets[3].my_round() + lag_target && t0.elapsed() < Duration::from_secs(60) {
        h.run_for(Duration::from_millis(200), |h| {
            grow_rec(h, &mut rec, 1, (GENESIS, 0), &[0, 1, 2]);
        });
    }
    let lag = h.nets[0].my_round().saturating_sub(h.nets[3].my_round());
    eprintln!("S1(hole={hole}) step 2: voter 3 in round {}, others {:?} → {lag} rounds behind ({:.1} s)",
        h.nets[3].my_round(), (0..3).map(|i| h.nets[i].my_round()).collect::<Vec<_>>(), t0.elapsed().as_secs_f64());
    assert!(lag >= lag_target, "precondition: voter 3 must be ~{lag_target} rounds behind, is {lag}");

    // 3. The chain splits at F into branches 2, 3, 4. Voters 0-2 hold all three; voter i PREFERS
    //    branch 2+i (its braid's selected tip). Rounds keep completing: prevotes on three
    //    branches, precommits for F (their only common ancestor).
    let fork = h.tips[&1];
    let fork_round = h.nets[0].my_round();
    let t1 = Instant::now();
    loop {
        h.run_for(Duration::from_millis(200), |h| {
            for br in 2..=4u8 {
                grow_rec(h, &mut rec, br, fork, &[0, 1, 2]);
            }
            for i in 0..3usize {
                let t = h.tips[&(2 + i as u8)];
                *h.envs[i].preferred.lock() = Some(t);
            }
        });
        let all_final_f = (0..3).all(|i| h.envs[i].last_finalized() == fork);
        let saved_split = (0..3).all(|i| {
            h.nets[i].completed().is_some_and(|(_, base, votes)| {
                base == fork
                    && votes.iter().filter_map(|m| match &m.message {
                        fg::Message::Prevote(p) => Some(branch_of(&p.target_hash)),
                        _ => None,
                    }).collect::<std::collections::HashSet<u8>>().len() == 3
            })
        });
        if (all_final_f && saved_split && h.nets[0].my_round() >= fork_round + 3) || t1.elapsed() > Duration::from_secs(15) {
            break;
        }
    }
    // r1's exact moment: each of voters 0-2 has completed round C and has ALREADY prevoted in
    // C+1 (r1-r3: completed 1530, guard newest 1531), so after the restart its guard repeats that
    // prevote in the very round it resumes into. (First run, 2026-10-18: the S1 crash landed with
    // guard newest == completed, so nothing was repeated — the precondition was not met.)
    let signed_ahead = |h: &Harness, i: usize| {
        let g = h.guards[i].lock();
        let newest = g.newest_round();
        h.nets[i].completed_round() == Some(newest.saturating_sub(1))
            && g.recorded(newest).iter().any(|(r, m)| *r == newest && matches!(m, fg::Message::Prevote(_)))
    };
    let t3 = Instant::now();
    while !(0..3).all(|i| signed_ahead(&h, i)) && t3.elapsed() < Duration::from_secs(10) {
        h.run_for(Duration::from_millis(5), |h| {
            for br in 2..=4u8 {
                grow_rec(h, &mut rec, br, fork, &[0, 1, 2]);
            }
            for i in 0..3usize {
                let t = h.tips[&(2 + i as u8)];
                *h.envs[i].preferred.lock() = Some(t);
            }
        });
    }
    eprintln!("S1(hole={hole}) step 3: fork at H={} in round {fork_round}; now rounds {:?}, finalized {:?}",
        fork.1, (0..4).map(|i| h.nets[i].my_round()).collect::<Vec<_>>(), h.finalized());
    for i in 0..3 {
        match h.nets[i].completed() {
            Some((r, base, votes)) => eprintln!("  voter {i} saved round {r} base {} : {}", base.1, describe_votes(&votes)),
            None => eprintln!("  voter {i} saved round: none"),
        }
        let newest = h.guards[i].lock().newest_round();
        eprintln!("  voter {i} guard (newest {newest}): {}", describe_guard(&h, i, newest.saturating_sub(1)));
    }
    assert!((0..3).all(|i| h.envs[i].last_finalized() == fork), "precondition: voters 0-2 finalized the fork point");
    assert!((0..3).all(|i| signed_ahead(&h, i)), "precondition: voters 0-2 prevoted in the round after their last completed one");

    // 4. Rolling restart compressed to one instant: every voter restarts like the node.
    let all: Vec<(Hash, Hash, Number)> = rec.iter().map(|(a, p, n, _)| (*a, *p, *n)).collect();
    let holds_for = |i: usize| -> Vec<(Hash, Hash, Number)> {
        all.iter()
            .copied()
            .filter(|(a, _, n)| match i {
                // the settled chain up to F (range backfill), none of the branches above it
                3 => branch_of(a) == 1 && *n <= fork.1,
                1 if hole => branch_of(a) != 4,
                _ => true,
            })
            .collect()
    };
    for i in 0..4 {
        h.crash(i);
    }
    h.bus.0.lock().online[3] = true;
    for i in [3usize, 0, 1, 2] {
        let holds = holds_for(i);
        let (r, nv) = restart_like_node(&mut h, i, &dir, &holds);
        eprintln!("S1(hole={hole}) step 4: voter {i} restarted above round {r} with {nv} of its votes from finalized H={} (guard newest {}, holds {} blocks)",
            h.envs[i].last_finalized().1, h.guards[i].lock().newest_round(), holds.len());
    }
    let before = h.finalized();

    // 5. Producers continue on branch 2 (delivered to everyone; voter 3 parks them — their
    //    parents are the split-era blocks it never received). Fork choice follows the growing
    //    branch (preferred = none → highest leaf). 6 s ≈ 30 s at r1's 100 ms gossip.
    let t2 = Instant::now();
    h.run_for(Duration::from_millis(6000), |h| {
        grow_rec(h, &mut rec, 2, fork, &[0, 1, 2, 3]);
    });
    let after = h.finalized();
    let tip = h.tips[&2].1;
    let rounds_after: Vec<u64> = h.nets.iter().map(|n| n.my_round()).collect();
    eprintln!("S1(hole={hole}, fetch={fetch}) step 5 after {:.1} s: tip H={tip}, finalized {before:?} → {after:?}, rounds {rounds_after:?}; blocks asked {:?}, served {}",
        t2.elapsed().as_secs_f64(),
        h.envs.iter().map(|e| e.blocks_requested.load(std::sync::atomic::Ordering::Relaxed)).collect::<Vec<_>>(), h.served);
    for i in 0..4 {
        let r = h.nets[i].my_round();
        eprintln!("  voter {i} round {r} (completed {:?}), finalized {}: transport holds {:?}",
            h.nets[i].completed_round(), after[i], h.nets[i].round_summary(r));
        eprintln!("    guard: {}", describe_guard(&h, i, r.saturating_sub(1)));
        let t = h.envs[i].tree.lock();
        eprintln!("    finality tree: {} blocks, holds b2 tip {} · b3 tip {} · b4 tip {}",
            t.len(), t.contains(&h.tips[&2].0), t.contains(&h.tips[&3].0), t.contains(&h.tips[&4].0));
    }
    h.assert_safe();
    let _ = std::fs::remove_dir_all(&dir);
    S1Outcome { fork_at: fork.1, before, after, tip, rounds_after }
}

/// S1 — the 10-17 wedge as the design specifies it: voters restarted after prevoting on different
/// branches, and a fourth ~90 rounds behind that lacks the blocks of the catch-up round. With D3
/// (2026-10-20) the voters fetch those blocks, and all four must reach the tip again.
///
/// Without D3 — MEASURED 2026-10-18 (5 runs, 4 with the r1-shaped crash moment enforced): this does NOT
/// reproduce the r1 freeze. The three restarted voters resume "above round C with 6 of its votes"
/// exactly as r1-r3 logged, repeat their round-C+1 prevotes on three branches, precommit the fork
/// point, complete the round and finalize the growing branch again — every run to within 5-19
/// blocks of the tip within the 6 s budget (e.g. 1,493 → 2,487, tip 2,506). It fails only on
/// voter 3, which stays in its old round (8 while the others reach ~174): every catch-up and
/// commit it is offered names blocks it does not hold, and nothing fetches them — D3's job. The
/// design's explanation of the third validator (its estimate of the last round "points higher")
/// cannot apply here or on r1: the saved round is identical on all three (precommits ×3 for the
/// fork point), so every estimate IS the fork point. The quorum freeze needs a second voter that
/// cannot place a vote's block: S1b.
#[test]
fn s1_restart_wedge_finality_resumes_to_the_tip() {
    let o = s1_wedge(false, true);
    let near = |f: &Number| *f + 40 >= o.tip;
    let quorum = o.after.iter().filter(|f| near(*f)).count();
    assert!(quorum >= 3, "QUORUM FROZEN: fork H={}, tip H={}, finalized {:?} -> {:?}, rounds {:?}",
        o.fork_at, o.tip, o.before, o.after, o.rounds_after);
    assert!(o.after.iter().all(|f| near(f)), "voter(s) left behind: fork H={}, tip H={}, finalized {:?} -> {:?}, rounds {:?}",
        o.fork_at, o.tip, o.before, o.after, o.rounds_after);
}

/// S1b — S1 plus what the evidence points to for r2: after its restart one of the three voters
/// no longer holds the branch another voter prevoted on in the round they were restarted in.
/// (On the node, a restart rebuilds the finality tree from the settled tip plus what the DAG
/// store reports, and reseeds the braid from the settled window, so unsettled side strands have
/// to be fetched again; on the frozen r1 every node still reports parents missing after 95,000+
/// by-hash answers, with hundreds of thousands of parked blocks dropped to make room. Which
/// block r2 lacks is NOT proven — no route exposes its finality tree.) That voter then sees two
/// placeable prevotes, no prevote-GHOST, and cannot precommit; with the fourth voter unable to
/// catch up, no round has 3 of 4 again. Same ignore rule as S1.
///
/// MEASURED 2026-10-18: frozen in 5 of 5 runs, finality pinned at the fork point for the whole
/// 6 s while the tip grew ~1,200 blocks; runs 2-5 (r1-shaped crash moment) end with all three
/// voters in one round. Run 2's full dump is r1's status line:
/// all three voters in round C+1 (last completed C), transport holding three prevotes on three
/// branches and two precommits for the fork point; guards: voter 0 pv+pc, voter 2 pv+pc,
/// voter 1 pv only (r1: r1 pv+pc, r3 pv+pc, r2 pv only); voter 3 in round 5, finalized the fork
/// point by commits, never caught up (r4: round 1442, finalized 7,452 by commits). Voter 1's
/// finality tree lacks branch 4, so voter 2's prevote stays held and voter 1 never sees a GHOST.
///
/// With D3 (2026-10-20): voter 1 asks for branch 4 and voter 3 for the branches its catch-ups
/// name; both must reach the tip again. The control below keeps the pre-D3 freeze on record.
#[test]
fn s1b_restart_wedge_with_a_missing_branch_finality_resumes_to_the_tip() {
    let o = s1_wedge(true, true);
    let near = |f: &Number| *f + 40 >= o.tip;
    let quorum = o.after.iter().filter(|f| near(*f)).count();
    assert!(quorum >= 3, "QUORUM FROZEN: fork H={}, tip H={}, finalized {:?} -> {:?}, rounds {:?}",
        o.fork_at, o.tip, o.before, o.after, o.rounds_after);
    assert!(o.after.iter().all(|f| near(f)), "voter(s) left behind: fork H={}, tip H={}, finalized {:?} -> {:?}, rounds {:?}",
        o.fork_at, o.tip, o.before, o.after, o.rounds_after);
}

/// S1b WITHOUT D3 — the control: the same wedge with no block fetcher must stay frozen (quorum
/// pinned at the fork point). Kept so the fix is shown to be the fetch, not a change in the
/// scenario. Slow and timing-dependent, so opt-in:
/// `fluxc test -p sigil-grandpa --profile release-fast --test gossip_sim -- --ignored s1b_control`.
#[test]
#[ignore = "control (pre-D3 behaviour): slow, run explicitly"]
fn s1b_control_without_block_fetch_stays_frozen() {
    let o = s1_wedge(true, false);
    let near = |f: &Number| *f + 40 >= o.tip;
    let quorum = o.after.iter().filter(|f| near(*f)).count();
    assert!(quorum < 3, "the pre-D3 wedge did NOT freeze this time: fork H={}, tip H={}, finalized {:?} -> {:?}",
        o.fork_at, o.tip, o.before, o.after);
}
