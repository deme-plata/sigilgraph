//! Restart storms over a FORKING spine (2026-10-18, rocky-chronos-3tb-1018).
//!
//! `gossip_sim.rs` proves fix 6 (saved completed round + re-announce) against one
//! hand-written spread restart on a single straight branch. The r1 freeze that followed
//! the fix (10-17 05:16) had a different shape: two producers braiding, so voters held
//! pre-restart votes for blocks on DIFFERENT branches, and the signer guard must repeat a
//! vote it already signed. This file drives the same reference voter and the same real
//! transport (`GossipTransport`) through randomized restart storms while the spine forks
//! and blocks reach nodes late, and records, per run: safety (never violated, or the test
//! fails), and liveness (did finality resume after the last restart, and how fast).
//!
//! Exploratory sweep, `#[ignore]`d. Run:
//!   STORM_SEEDS=40 STORM_THREADS=6 TMPDIR=<scratch> \
//!   fluxc test -p sigil-grandpa --profile release-fast --test restart_storm -- --ignored --nocapture
//! Each run is ~5 s of WALL CLOCK (20 ms gossip ticks): CPU starvation looks like a slow net.
//! 2026-10-20 (D3): `STORM_FETCH=1` gives every voter the block fetcher (sigil-grandpa bb0fb3af);
//! off = the 10-18 sweeps exactly. Copied from the chronos lane (sigil-chronos-3tb-1018, untracked).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use futures::executor::LocalPool;
use futures::future::{abortable, AbortHandle};
use futures::task::LocalSpawnExt;
use futures::FutureExt;
use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sigil_grandpa::fg::voter::Voter;
use sigil_grandpa::fg::voter_set::VoterSet;
use sigil_grandpa::fg::Chain;
use sigil_grandpa::gossip::GossipTransport;
use sigil_grandpa::*;

const GOSSIP: Duration = Duration::from_millis(20);
const SET_ID: u64 = 1;
const GENESIS: Hash = [0u8; 32];
const N: usize = 4;

fn bh(branch: u32, n: u64) -> Hash {
    let mut h = [0u8; 32];
    h[0..4].copy_from_slice(&branch.to_le_bytes());
    h[4..12].copy_from_slice(&n.to_le_bytes());
    h[31] = 1;
    h
}

fn key(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i + 1; 32])
}

#[derive(Default)]
struct BusState {
    queue: VecDeque<(usize, Vec<u8>)>,
    online: Vec<bool>,
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
    /// Every block ever made, in creation order: (hash, parent, number).
    made: Vec<(Hash, Hash, Number)>,
    log: Arc<Mutex<Vec<(Id, Hash, Number)>>>,
    errors: Arc<Mutex<Vec<String>>>,
    last_tick: Instant,
    /// Blocks not yet delivered to a node: (due tick, node, block).
    late: Vec<(u64, usize, (Hash, Hash, Number))>,
    main_tip: (Hash, Number),
    next_branch: u32,
    ticks: u64,
    round_dir: Option<PathBuf>,
    /// Re-fetch model (STORM_BACKFILL_DEPTH > 0): blocks at least this far below the main tip
    /// reach every node that lacks them, every STORM_BACKFILL_EVERY ticks and on restart — the
    /// real node's by-hash re-fetch + range backfill. 0 = off (the first sweeps).
    backfill_depth: u64,
    backfill_every: u64,
    /// D3 (2026-10-20, STORM_FETCH=1): voters ask for the blocks their held votes name; asks
    /// are answered like sigil-node's by-hash server. Off = the 10-18 sweeps exactly.
    fetch_on: bool,
    fetch_q: Arc<Mutex<Vec<(usize, Vec<Hash>)>>>,
    /// Every block made: hash -> (parent, number).
    parent_of: HashMap<Hash, (Hash, Number)>,
    served: u64,
}

/// What one by-hash answer carries at most (`sigil-node/src/parent_fetch.rs`, `BY_HASH_MAX`).
const BY_HASH_MAX: usize = 64;

impl Harness {
    fn new(round_dir: Option<PathBuf>) -> Self {
        let bus = Arc::new(Bus::default());
        bus.0.lock().online = vec![true; N];
        let voters = VoterSet::new((0..N as u8).map(|i| (key(i).verifying_key().to_bytes(), 1))).unwrap();
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
            made: Vec::new(),
            log: log.clone(),
            errors: Arc::new(Mutex::new(Vec::new())),
            last_tick: Instant::now(),
            late: Vec::new(),
            main_tip: (GENESIS, 0),
            next_branch: 2,
            ticks: 0,
            round_dir,
            backfill_depth: std::env::var("STORM_BACKFILL_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            backfill_every: std::env::var("STORM_BACKFILL_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(20),
            fetch_on: std::env::var("STORM_FETCH").ok().as_deref() == Some("1"),
            fetch_q: Arc::new(Mutex::new(Vec::new())),
            parent_of: HashMap::new(),
            served: 0,
        };
        for i in 0..N {
            let b = bus.clone();
            let net = GossipTransport::new(SET_ID, Box::new(move |bytes| b.0.lock().queue.push_back((i, bytes))));
            if let Some(d) = &h.round_dir {
                net.use_round_file(d.join(format!("round-{i}.json")));
            }
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
        for i in 0..N {
            let fin = h.envs[i].last_finalized();
            h.spawn_voter(i, 0, Vec::new(), fin);
        }
        h
    }

    fn wire_fetch(&self, i: usize) {
        if !self.fetch_on {
            return;
        }
        let q = self.fetch_q.clone();
        self.envs[i].set_block_fetcher(Box::new(move |hs| q.lock().push((i, hs))));
    }

    /// Answer queued asks like a peer answers a by-hash request (see gossip_sim.rs).
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
                    while let Some(&(p, n)) = self.parent_of.get(&cur) {
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

    fn spawn_voter(&mut self, i: usize, last_round: u64, votes: Vec<SigilSignedMessage>, base: (Hash, Number)) {
        let env = self.envs[i].clone();
        let fin = env.last_finalized();
        let voter = Voter::new(env.clone(), self.voters.clone(), env.global_comms(), last_round, votes, base, fin);
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

    fn pump(&self) {
        loop {
            let item = self.bus.0.lock().queue.pop_front();
            let Some((from, bytes)) = item else { break };
            let online = self.bus.0.lock().online.clone();
            if !online[from] {
                continue;
            }
            for (j, net) in self.nets.iter().enumerate() {
                if j != from && online[j] {
                    net.on_wire(&bytes);
                }
            }
        }
    }

    fn deliver(&self, i: usize, b: (Hash, Hash, Number)) {
        self.envs[i].tree.lock().insert_or_park(b.0, b.1, b.2);
    }

    /// One braid step: the main spine grows by one block, reaching a random subset at once
    /// and the rest up to `max_delay` ticks later; with probability `p_fork` a short side
    /// branch (1–3 blocks, off the parent of the new tip) reaches 1–2 nodes first.
    fn braid_step(&mut self, rng: &mut StdRng, p_fork: f64, max_delay: u64) {
        let parent = self.main_tip;
        let b = (bh(1, parent.1 + 1), parent.0, parent.1 + 1);
        assert!(self.truth.insert(b.0, b.1, b.2));
        self.made.push(b);
        self.parent_of.insert(b.0, (b.1, b.2));
        self.main_tip = (b.0, b.2);
        let mut early = vec![false; N];
        early[rng.gen_range(0..N)] = true;
        for e in early.iter_mut() {
            if rng.gen_bool(0.5) {
                *e = true;
            }
        }
        for (i, e) in early.iter().enumerate() {
            if *e || max_delay == 0 {
                self.deliver(i, b);
            } else {
                let due = self.ticks + rng.gen_range(1..=max_delay);
                self.late.push((due, i, b));
            }
        }
        if p_fork > 0.0 && rng.gen_bool(p_fork) && parent.1 > 0 {
            let br = self.next_branch;
            self.next_branch += 1;
            let len = rng.gen_range(1..=3u64);
            let mut p = parent;
            let first = rng.gen_range(0..N);
            let second = if rng.gen_bool(0.5) { Some(rng.gen_range(0..N)) } else { None };
            for _ in 0..len {
                let fb = (bh(br, p.1 + 1), p.0, p.1 + 1);
                if !self.truth.insert(fb.0, fb.1, fb.2) {
                    break;
                }
                self.made.push(fb);
                self.parent_of.insert(fb.0, (fb.1, fb.2));
                self.deliver(first, fb);
                if let Some(s) = second {
                    self.deliver(s, fb);
                }
                // everyone else learns the losing branch late (as gossip would bring it)
                for i in 0..N {
                    if i != first && Some(i) != second {
                        let due = self.ticks + rng.gen_range(1..=max_delay.max(1) * 2);
                        self.late.push((due, i, fb));
                    }
                }
                p = (fb.0, fb.2);
            }
        }
        let now = self.ticks;
        let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.late).into_iter().partition(|(d, _, _)| *d <= now);
        self.late = keep;
        for (_, i, blk) in due {
            self.deliver(i, blk);
        }
        if self.backfill_depth > 0 && self.ticks % self.backfill_every.max(1) == 0 {
            self.backfill_all();
        }
    }

    /// Every block at least `backfill_depth` below the main tip reaches every node lacking it.
    fn backfill_all(&self) {
        let floor = self.main_tip.1.saturating_sub(self.backfill_depth);
        for i in 0..N {
            let missing: Vec<(Hash, Hash, Number)> = {
                let t = self.envs[i].tree.lock();
                self.made.iter().copied().filter(|b| b.2 <= floor && !t.contains(&b.0)).collect()
            };
            let mut missing = missing;
            missing.sort_by_key(|b| b.2);
            for b in missing {
                self.deliver(i, b);
            }
        }
    }

    /// A PROCESS restart (memory lost): new transport + environment; survives the signer
    /// guard and, with `round_dir`, the saved completed round. `fix` = the node's 10-17 rule
    /// (resume after the saved round + re-announce); otherwise the old rule (above the guard).
    fn restart_process(&mut self, i: usize, fix: bool) {
        if let Some(h) = self.aborts[i].take() {
            h.abort();
        }
        let b = self.bus.clone();
        let net = GossipTransport::new(SET_ID, Box::new(move |bytes| b.0.lock().queue.push_back((i, bytes))));
        if fix {
            if let Some(d) = &self.round_dir {
                net.use_round_file(d.join(format!("round-{i}.json")));
            }
        }
        let fin = self.envs[i].last_finalized();
        // The restarted node re-learns what it held before through sync: what it had in its
        // tree. Blocks still in flight to it stay in flight.
        // The voter prunes finalized ancestors from its tree, so rebuild the FULL ancestry of
        // everything it held (its block store has it), not just the surviving tree entries.
        let parent: HashMap<Hash, (Hash, Number)> = self.made.iter().map(|(h, p, n)| (*h, (*p, *n))).collect();
        let mut need: std::collections::HashSet<Hash> = std::collections::HashSet::new();
        {
            let old = self.envs[i].tree.lock();
            for (h, _, _) in &self.made {
                if old.contains(h) {
                    let mut cur = *h;
                    while cur != GENESIS && need.insert(cur) {
                        cur = parent[&cur].0;
                    }
                }
            }
        }
        let mut held: Vec<(Hash, Hash, Number)> = need.iter().map(|h| (*h, parent[h].0, parent[h].1)).collect();
        held.sort_by_key(|b| b.2);
        let mut tree = SpineTree::new(GENESIS);
        for (h, p, n) in held {
            tree.insert_or_park(h, p, n);
        }
        let env = Arc::new(SigilEnv::new(tree, key(i as u8), GENESIS, SET_ID, net.clone(), self.guards[i].clone(), GOSSIP, fin, self.log.clone()));
        self.nets[i] = net;
        self.envs[i] = env;
        self.wire_fetch(i);
        if self.backfill_depth > 0 {
            self.backfill_all();
        }
        if fix {
            self.envs[i].reannounce_recorded_votes(2);
        }
        match self.nets[i].completed().filter(|_| fix && self.round_dir.is_some()) {
            Some((r, base, votes)) => self.spawn_voter(i, r, votes, base),
            None => {
                let last_round = self.guards[i].lock().newest_round();
                self.spawn_voter(i, last_round, Vec::new(), fin);
            }
        }
    }

    fn step(&mut self) {
        self.ticks += 1;
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

    fn finalized(&self) -> Vec<Number> {
        self.envs.iter().map(|e| e.last_finalized().1).collect()
    }

    /// Safety findings (empty = safe): task errors, local violations, conflicting finality,
    /// an honest voter seen double-voting.
    fn safety(&self) -> Vec<String> {
        let mut out: Vec<String> = self.errors.lock().clone();
        for e in &self.envs {
            out.extend(e.safety_violations.lock().iter().cloned());
            if !e.equivocations.lock().is_empty() {
                out.push(format!("equivocation seen: {:?}", e.equivocations.lock().len()));
            }
        }
        let log = self.log.lock();
        let mut by_n: HashMap<Number, Hash> = HashMap::new();
        for (_, h, n) in log.iter() {
            if let Some(prev) = by_n.insert(*n, *h) {
                if prev != *h {
                    out.push(format!("CONFLICTING FINALITY at {n}"));
                }
            }
        }
        let mut uniq: Vec<(Hash, Number)> = by_n.into_iter().map(|(n, h)| (h, n)).collect();
        uniq.sort_by_key(|x| x.1);
        for w in uniq.windows(2) {
            if !self.truth.is_equal_or_descendent_of(w[0].0, w[1].0) {
                out.push(format!("finalized {} does not descend from finalized {}", w[1].1, w[0].1));
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    NoRestart,
    Rolling,
    Pair,
    AllAtOnce,
    AllDuringFork,
    BehindThenAll,
    DoubleRolling,
}

const SCENARIOS: [Scenario; 7] =
    [Scenario::NoRestart, Scenario::Rolling, Scenario::Pair, Scenario::AllAtOnce, Scenario::AllDuringFork, Scenario::BehindThenAll, Scenario::DoubleRolling];

struct RunResult {
    line: String,
    safe: bool,
    resumed: bool,
}

fn run_one(sc: Scenario, fix: bool, seed: u64, scratch: &Path) -> RunResult {
    let mut rng = StdRng::seed_from_u64(seed * 1_000 + sc as u64 * 10 + fix as u64);
    let dir = scratch.join(format!("storm-{sc:?}-{fix}-{seed}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::new(fix.then(|| dir.clone()));
    // STORM_PFORK / STORM_DELAY override the braid (0 / 0 = the straight, instant chain
    // gossip_sim.rs uses); AllDuringFork always forks at least half the steps.
    let env_f = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
    let base_p = env_f("STORM_PFORK").unwrap_or(0.15);
    let base_d = env_f("STORM_DELAY").map(|d| d as u64).unwrap_or(6);
    let (p_fork, delay) = match sc {
        Scenario::AllDuringFork => (base_p.max(0.5), base_d.max(12)),
        _ => (base_p, base_d),
    };
    // Warm up: finality running at the tip.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(800) {
        h.braid_step(&mut rng, p_fork, delay);
        h.step();
    }
    let warm = h.finalized();
    // Build the restart schedule (ms from now, node).
    let mut plan: Vec<(u64, usize)> = Vec::new();
    let mut offline: Option<(usize, u64)> = None;
    match sc {
        Scenario::NoRestart => {
            // Same wall time as a rolling storm, nobody restarts: the braid's own liveness.
            plan.push((900, usize::MAX));
        }
        Scenario::Rolling => {
            let gap = rng.gen_range(150..=600);
            for i in 0..N {
                plan.push((i as u64 * gap, i));
            }
        }
        Scenario::Pair => {
            let a = rng.gen_range(0..N);
            let b = (a + rng.gen_range(1..N)) % N;
            plan.push((0, a));
            plan.push((rng.gen_range(0..=100), b));
        }
        Scenario::AllAtOnce | Scenario::AllDuringFork => {
            let mut order: Vec<usize> = (0..N).collect();
            for k in (1..N).rev() {
                order.swap(k, rng.gen_range(0..=k));
            }
            let mut at = 0;
            for i in order {
                plan.push((at, i));
                at += rng.gen_range(0..=40);
            }
        }
        Scenario::BehindThenAll => {
            let k = rng.gen_range(0..N);
            let off_ms = rng.gen_range(600..=1200);
            offline = Some((k, off_ms));
            for i in 0..N {
                plan.push((off_ms + rng.gen_range(0..=60), i));
            }
        }
        Scenario::DoubleRolling => {
            let gap = rng.gen_range(150..=400);
            for i in 0..N {
                plan.push((i as u64 * gap, i));
            }
            let second = N as u64 * gap + rng.gen_range(200..=600);
            for i in 0..N {
                plan.push((second + i as u64 * gap, (i + 1) % N));
            }
        }
    }
    plan.sort();
    if let Some((k, _)) = offline {
        h.bus.0.lock().online[k] = false;
    }
    let t0 = Instant::now();
    let mut next = 0;
    let last_at = plan.last().map(|p| p.0).unwrap_or(0);
    while next < plan.len() {
        let el = t0.elapsed().as_millis() as u64;
        if let Some((k, off_ms)) = offline {
            if el >= off_ms {
                h.bus.0.lock().online[k] = true;
                offline = None;
            }
        }
        while next < plan.len() && plan[next].0 <= el {
            if plan[next].1 < N {
                h.restart_process(plan[next].1, fix);
            }
            next += 1;
        }
        h.braid_step(&mut rng, p_fork, delay);
        h.step();
    }
    let at_last = h.finalized();
    let bar = *at_last.iter().max().unwrap();
    let t1 = Instant::now();
    let mut resumed_ms: Option<u128> = None;
    let observe = std::env::var("STORM_OBSERVE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(4000u64);
    while t1.elapsed() < Duration::from_millis(observe) {
        h.braid_step(&mut rng, p_fork, delay);
        h.step();
        if resumed_ms.is_none() && h.finalized().iter().all(|f| *f > bar) {
            resumed_ms = Some(t1.elapsed().as_millis());
        }
    }
    let end = h.finalized();
    let tip = h.main_tip.1;
    let issues = h.safety();
    let rounds: Vec<u64> = h.nets.iter().map(|n| n.my_round()).collect();
    let lag = tip.saturating_sub(*end.iter().min().unwrap());
    let line = format!(
        "{{\"scenario\":\"{sc:?}\",\"fix\":{fix},\"seed\":{seed},\"p_fork\":{p_fork},\"delay\":{delay},\"backfill\":{},\"fetch\":{},\"asked\":{},\"served\":{},\"plan_ms\":{last_at},\"warm\":{warm:?},\"at_last_restart\":{at_last:?},\"end\":{end:?},\"tip\":{tip},\"min_lag\":{lag},\"resumed_ms\":{},\"rounds\":{rounds:?},\"forks\":{},\"safety\":{:?}}}",
        h.backfill_depth,
        h.fetch_on,
        h.envs.iter().map(|e| e.blocks_requested.load(std::sync::atomic::Ordering::Relaxed)).sum::<u64>(),
        h.served,
        resumed_ms.map(|v| v.to_string()).unwrap_or("null".into()),
        h.next_branch - 2,
        issues
    );
    let _ = std::fs::remove_dir_all(&dir);
    RunResult { line, safe: issues.is_empty(), resumed: resumed_ms.is_some() }
}

#[test]
#[ignore]
fn restart_storms_over_a_forking_spine() {
    let seeds: u64 = std::env::var("STORM_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let threads: usize = std::env::var("STORM_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let only: Option<String> = std::env::var("STORM_ONLY").ok();
    let scratch = std::env::temp_dir();
    let out_path = std::env::var("STORM_OUT").ok();
    let mut jobs: Vec<(Scenario, bool, u64)> = Vec::new();
    for sc in SCENARIOS {
        if only.as_ref().is_some_and(|o| !format!("{sc:?}").eq_ignore_ascii_case(o)) {
            continue;
        }
        for fix in [true, false] {
            for seed in 0..seeds {
                jobs.push((sc, fix, seed));
            }
        }
    }
    let jobs = Arc::new(Mutex::new(jobs));
    let results = Arc::new(Mutex::new(Vec::<(Scenario, bool, RunResult)>::new()));
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (jobs, results, scratch) = (jobs.clone(), results.clone(), scratch.clone());
            s.spawn(move || loop {
                let Some((sc, fix, seed)) = jobs.lock().pop() else { break };
                let r = run_one(sc, fix, seed, &scratch);
                eprintln!("{}", r.line);
                results.lock().push((sc, fix, r));
            });
        }
    });
    let results = results.lock();
    if let Some(p) = out_path {
        let body: String = results.iter().map(|(_, _, r)| format!("{}\n", r.line)).collect();
        std::fs::write(p, body).unwrap();
    }
    eprintln!("\n=== restart storms: resumed / runs (unsafe runs) ===");
    let mut unsafe_total = 0;
    for sc in SCENARIOS {
        for fix in [true, false] {
            let v: Vec<_> = results.iter().filter(|(s, f, _)| format!("{s:?}") == format!("{sc:?}") && *f == fix).collect();
            if v.is_empty() {
                continue;
            }
            let res = v.iter().filter(|(_, _, r)| r.resumed).count();
            let uns = v.iter().filter(|(_, _, r)| !r.safe).count();
            unsafe_total += uns;
            eprintln!("  {:<14} fix={:<5} resumed {:>3}/{:<3}  unsafe {}", format!("{sc:?}"), fix, res, v.len(), uns);
        }
    }
    assert_eq!(unsafe_total, 0, "a restart storm broke SAFETY — see the lines above");
}
