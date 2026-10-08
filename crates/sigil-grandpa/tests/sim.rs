//! M2.0 acceptance (2026-10-08): the reference GRANDPA voter driven through SIGIL's
//! environment, four validators in memory, real timers (gossip T = 20 ms).
//!
//! What each test must show, in the words of the roadmap and the 10-08 review:
//! * honest 4: finality follows a growing chain;
//! * two producers' branches seen differently: no conflicting finality, then convergence;
//! * a 2|2 partition: NO finality while split (neither side has 3 of 4), finality resumes
//!   after the heal, and every block finalized before the split stays finalized;
//! * a Byzantine voter equivocating in every round, HIDDEN (each honest voter sees only one
//!   of its two votes): never two conflicting finalized blocks;
//! * a crash after the precommit was persisted but before it was sent, then a restart from
//!   the persisted round state: the voter re-sends what it signed, nobody sees it equivocate.
//!
//! Safety is judged across ALL honest voters at once: every block anyone finalized must lie
//! on one chain of a global "truth" tree holding every block ever made.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::executor::LocalPool;
use futures::future::{abortable, AbortHandle};
use futures::task::LocalSpawnExt;
use futures::FutureExt;
use parking_lot::Mutex;
use sigil_grandpa::fg::voter::Voter;
use sigil_grandpa::fg::voter_set::VoterSet;
use sigil_grandpa::fg::{Chain, Message, Precommit, Prevote};
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

// ─── in-memory transport with partitions and targeted (Byzantine) delivery ─────────

#[derive(Default)]
struct NetInner {
    round_hist: HashMap<u64, Vec<(Id, SigilSignedMessage)>>,
    targeted: Vec<(u64, Id, SigilSignedMessage)>,
    round_subs: HashMap<u64, Vec<(Id, UnboundedSender<SigilSignedMessage>)>>,
    global_hist: Vec<(Id, u64, SigilCommit)>,
    global_subs: Vec<(Id, UnboundedSender<(u64, SigilCommit)>)>,
    groups: Option<HashMap<Id, u8>>,
    held_round: Vec<(u64, Id, SigilSignedMessage)>,
    held_global: Vec<(Id, u64, SigilCommit)>,
    /// Targeted (Byzantine) messages that honest receivers will gossip on: `(due, round, msg)`.
    relay: Vec<(Instant, u64, SigilSignedMessage)>,
}

impl NetInner {
    fn can_talk(&self, a: &Id, b: &Id) -> bool {
        match &self.groups {
            None => true,
            Some(g) => g.get(a) == g.get(b),
        }
    }
}

#[derive(Default)]
struct MemNet(Mutex<NetInner>);

impl MemNet {
    fn partition(&self, groups: &[(&Id, u8)]) {
        self.0.lock().groups = Some(groups.iter().map(|(i, g)| (**i, *g)).collect());
    }
    fn heal(&self) {
        let mut n = self.0.lock();
        n.groups = None;
        for (round, to, m) in std::mem::take(&mut n.held_round) {
            for (id, tx) in n.round_subs.get(&round).into_iter().flatten() {
                if *id == to {
                    let _ = tx.unbounded_send(m.clone());
                }
            }
        }
        for (to, round, c) in std::mem::take(&mut n.held_global) {
            for (id, tx) in &n.global_subs {
                if *id == to {
                    let _ = tx.unbounded_send((round, c.clone()));
                }
            }
        }
    }
    /// Like `inject`, but after `after` the honest receivers' gossip relays it to everyone —
    /// what gossipsub does on the real network. A hidden equivocation stays hidden only
    /// that long.
    fn inject_relayed(&self, round: u64, to: &[Id], msg: SigilSignedMessage, after: Duration) {
        self.inject(round, to, msg.clone());
        self.0.lock().relay.push((Instant::now() + after, round, msg));
    }
    fn pump(&self) {
        let due: Vec<(u64, SigilSignedMessage)> = {
            let mut n = self.0.lock();
            let now = Instant::now();
            let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut n.relay).into_iter().partition(|(t, _, _)| *t <= now);
            n.relay = keep;
            due.into_iter().map(|(_, r, m)| (r, m)).collect()
        };
        for (r, m) in due {
            let from = m.id;
            self.broadcast_round(r, from, m);
        }
    }
    /// Byzantine delivery: `msg` reaches only `to` (now and on any later subscription).
    fn inject(&self, round: u64, to: &[Id], msg: SigilSignedMessage) {
        let mut n = self.0.lock();
        for t in to {
            n.targeted.push((round, *t, msg.clone()));
            for (id, tx) in n.round_subs.get(&round).into_iter().flatten() {
                if id == t {
                    let _ = tx.unbounded_send(msg.clone());
                }
            }
        }
    }
}

impl Transport for MemNet {
    fn subscribe_round(&self, round: u64, me: Id) -> UnboundedReceiver<SigilSignedMessage> {
        let (tx, rx) = unbounded();
        let mut n = self.0.lock();
        let hist = n.round_hist.get(&round).cloned().unwrap_or_default();
        for (from, m) in hist {
            if n.can_talk(&from, &me) {
                let _ = tx.unbounded_send(m);
            } else {
                n.held_round.push((round, me, m));
            }
        }
        for (r, t, m) in &n.targeted {
            if *r == round && *t == me {
                let _ = tx.unbounded_send(m.clone());
            }
        }
        n.round_subs.entry(round).or_default().push((me, tx));
        rx
    }
    fn broadcast_round(&self, round: u64, from: Id, msg: SigilSignedMessage) {
        let mut n = self.0.lock();
        n.round_hist.entry(round).or_default().push((from, msg.clone()));
        let subs: Vec<(Id, UnboundedSender<SigilSignedMessage>)> =
            n.round_subs.get(&round).cloned().unwrap_or_default();
        for (id, tx) in subs {
            if n.can_talk(&from, &id) {
                let _ = tx.unbounded_send(msg.clone());
            } else {
                n.held_round.push((round, id, msg.clone()));
            }
        }
    }
    fn subscribe_global(&self, me: Id) -> UnboundedReceiver<(u64, SigilCommit)> {
        let (tx, rx) = unbounded();
        let mut n = self.0.lock();
        for (from, r, c) in n.global_hist.clone() {
            if n.can_talk(&from, &me) {
                let _ = tx.unbounded_send((r, c));
            } else {
                n.held_global.push((me, r, c));
            }
        }
        n.global_subs.push((me, tx));
        rx
    }
    fn broadcast_global(&self, from: Id, round: u64, commit: SigilCommit) {
        let mut n = self.0.lock();
        n.global_hist.push((from, round, commit.clone()));
        for (id, tx) in n.global_subs.clone() {
            if n.can_talk(&from, &id) {
                let _ = tx.unbounded_send((round, commit.clone()));
            } else {
                n.held_global.push((id, round, commit.clone()));
            }
        }
    }
}

// ─── harness ───────────────────────────────────────────────────────────────────────

struct Harness {
    pool: LocalPool,
    net: Arc<MemNet>,
    envs: Vec<Arc<SigilEnv<MemNet>>>,
    aborts: Vec<Option<AbortHandle>>,
    voters: VoterSet<Id>,
    truth: SpineTree,
    log: Arc<Mutex<Vec<(Id, Hash, Number)>>>,
    tips: HashMap<u8, (Hash, Number)>,
}

impl Harness {
    /// `honest` voters run; the committee always has 4 members (keys 0..4).
    fn new(honest: usize) -> Self {
        let net = Arc::new(MemNet::default());
        let voters = VoterSet::new((0..4u8).map(|i| (key(i).verifying_key().to_bytes(), 1))).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut h = Harness {
            pool: LocalPool::new(),
            net: net.clone(),
            envs: Vec::new(),
            aborts: Vec::new(),
            voters,
            truth: SpineTree::new(GENESIS),
            log: log.clone(),
            tips: HashMap::new(),
        };
        for i in 0..honest as u8 {
            let env = Arc::new(SigilEnv::new(
                SpineTree::new(GENESIS),
                key(i),
                GENESIS,
                SET_ID,
                net.clone(),
                Arc::new(Mutex::new(SignerGuard::in_memory(SET_ID))),
                GOSSIP,
                (GENESIS, 0),
                log.clone(),
            ));
            h.envs.push(env);
            h.aborts.push(None);
        }
        for i in 0..honest {
            h.start(i, None);
        }
        h
    }

    fn start(&mut self, i: usize, restart: Option<RestartState>) {
        let env = self.envs[i].clone();
        let (last_round, votes, base, fin) = match restart {
            Some(r) => (r.last_round, r.last_round_votes, r.last_round_base, r.last_finalized),
            None => (0, Vec::new(), (GENESIS, 0), (GENESIS, 0)),
        };
        let voter = Voter::new(env.clone(), self.voters.clone(), env.global_comms(), last_round, votes, base, fin);
        let (fut, handle) = abortable(voter);
        self.pool.spawner().spawn_local(fut.map(|_| ())).unwrap();
        self.aborts[i] = Some(handle);
    }

    fn crash(&mut self, i: usize) {
        if let Some(h) = self.aborts[i].take() {
            h.abort();
        }
    }

    /// Extend `branch` (from `parent` if it has no blocks yet) by one block, delivered to `to`.
    fn grow(&mut self, branch: u8, parent: (Hash, Number), to: &[usize]) -> (Hash, Number) {
        let (ph, pn) = *self.tips.get(&branch).unwrap_or(&parent);
        let b = (bh(branch, pn + 1), pn + 1);
        assert!(self.truth.insert(b.0, ph, b.1));
        for &i in to {
            self.deliver(i, b.0);
        }
        self.tips.insert(branch, b);
        b
    }

    /// Give voter `i` block `h` and every ancestor it lacks.
    fn deliver(&self, i: usize, h: Hash) {
        let mut chain = vec![h];
        chain.extend(self.truth.ancestry(GENESIS, h).unwrap_or_default());
        let mut t = self.envs[i].tree.lock();
        for b in chain.into_iter().rev() {
            if let Some(n) = self.truth.number(&b) {
                let parent = self.parent_of(b);
                t.insert(b, parent, n);
            }
        }
    }

    fn parent_of(&self, h: Hash) -> Hash {
        match self.truth.ancestry(GENESIS, h) {
            Ok(a) => a.first().copied().unwrap_or(GENESIS),
            Err(_) => GENESIS,
        }
    }

    fn sync_all(&self) {
        let tips: Vec<Hash> = self.tips.values().map(|t| t.0).collect();
        for i in 0..self.envs.len() {
            for t in &tips {
                self.deliver(i, *t);
            }
        }
    }

    /// Run the pool for `d`, calling `tick` every 10 ms (block production lives there).
    fn run_for(&mut self, d: Duration, mut tick: impl FnMut(&mut Harness)) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.net.pump();
            tick(self);
            let nap = futures_timer::Delay::new(Duration::from_millis(10));
            self.pool.run_until(nap);
        }
    }

    fn finalized(&self) -> Vec<Number> {
        self.envs.iter().map(|e| e.last_finalized().1).collect()
    }

    /// Every block any honest voter finalized lies on ONE chain of the truth tree.
    fn assert_safe(&self) {
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
    }

    fn equivocators(&self) -> Vec<Id> {
        let mut v: Vec<Id> = self.envs.iter().flat_map(|e| e.equivocations.lock().iter().map(|(_, id)| *id).collect::<Vec<_>>()).collect();
        v.sort();
        v.dedup();
        v
    }
}

// ─── the tests ─────────────────────────────────────────────────────────────────────

#[test]
fn four_honest_voters_finalize_a_growing_chain() {
    let mut h = Harness::new(4);
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let tip = h.tips[&1].1;
    let fin = h.finalized();
    eprintln!("tip {tip}, finalized {fin:?}");
    assert!(fin.iter().all(|f| *f + 40 >= tip && *f > 0), "finality must follow the tip: tip {tip}, finalized {fin:?}");
    h.assert_safe();
    assert!(h.equivocators().is_empty());
}

#[test]
fn two_producers_seen_differently_never_conflict_and_then_converge() {
    let mut h = Harness::new(4);
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(300), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]));
    // Two producers race from `root`: voters 0,1 hear branch 2 first, voters 2,3 branch 3;
    // each side learns the other's branch only every ~100 ms (a slow merge).
    let mut k = 0u32;
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(2, root, &[0, 1]);
        h.grow(3, root, &[2, 3]);
        k += 1;
        if k % 10 == 0 {
            h.sync_all();
        }
    });
    h.assert_safe();
    // Then one branch wins everywhere (the other producer stops) and finality follows it.
    h.sync_all();
    h.run_for(Duration::from_millis(1200), |h| {
        h.grow(2, root, &[0, 1, 2, 3]);
    });
    let tip = h.tips[&2].1;
    let fin = h.finalized();
    eprintln!("after convergence: tip {tip}, finalized {fin:?}");
    assert!(fin.iter().all(|f| *f + 40 >= tip), "finality resumes on the winning branch: tip {tip}, {fin:?}");
    h.assert_safe();
    assert!(h.equivocators().is_empty(), "honest voters never equivocate");
}

#[test]
fn a_two_two_partition_halts_finality_and_it_resumes_without_changing_the_prefix() {
    let mut h = Harness::new(4);
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(800), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]));
    let ids: Vec<Id> = h.envs.iter().map(|e| e.id()).collect();
    h.net.partition(&[(&ids[0], 0), (&ids[1], 0), (&ids[2], 1), (&ids[3], 1)]);
    // Let in-flight rounds settle, then measure: nothing may be finalized during the split.
    h.run_for(Duration::from_millis(300), |h| {
        h.grow(2, root, &[0, 1]);
        h.grow(3, root, &[2, 3]);
    });
    let before: Vec<(Hash, Number)> = h.envs.iter().map(|e| e.last_finalized()).collect();
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(2, root, &[0, 1]);
        h.grow(3, root, &[2, 3]);
    });
    let during: Vec<(Hash, Number)> = h.envs.iter().map(|e| e.last_finalized()).collect();
    eprintln!("split: finalized before {:?} during {:?}", before.iter().map(|x| x.1).collect::<Vec<_>>(), during.iter().map(|x| x.1).collect::<Vec<_>>());
    assert_eq!(before, during, "2 of 4 is below quorum: no side may finalize while split");
    // Heal: messages held during the split arrive, every voter learns both branches, and
    // the producers keep going on branch 2.
    h.net.heal();
    h.sync_all();
    h.run_for(Duration::from_millis(2000), |h| {
        h.grow(2, root, &[0, 1, 2, 3]);
    });
    let tip = h.tips[&2].1;
    let after = h.finalized();
    eprintln!("healed: tip {tip}, finalized {after:?}");
    assert!(after.iter().zip(&during).all(|(a, d)| *a > d.1), "finality resumes after the heal: {after:?}");
    for (e, d) in h.envs.iter().zip(&during) {
        assert!(h.truth.is_equal_or_descendent_of(d.0, e.last_finalized().0), "the pre-split finalized block stays an ancestor");
    }
    h.assert_safe();
}

#[test]
fn a_hidden_byzantine_equivocator_never_splits_finality() {
    // Three honest voters (keys 0,1,2); key 3 is Byzantine and runs no voter. In EVERY round
    // it prevotes and precommits branch 2's tip to voter 0, and branch 3's tip to voters 1, 2
    // — each honest voter sees only one of its two votes ("hidden equivocation").
    let mut h = Harness::new(3);
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(300), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2]));
    let mut a = root;
    let mut b = root;
    for _ in 0..30 {
        a = h.grow(2, root, &[0, 1, 2]);
        b = h.grow(3, root, &[0, 1, 2]);
    }
    let byz = key(3);
    let ids: Vec<Id> = h.envs.iter().map(|e| e.id()).collect();
    for round in 1..400u64 {
        for (t, to) in [(a, vec![ids[0]]), (b, vec![ids[1], ids[2]])] {
            for m in [Message::Prevote(Prevote::new(t.0, t.1)), Message::Precommit(Precommit::new(t.0, t.1))] {
                h.net.inject(round, &to, sign(&byz, &GENESIS, SET_ID, round, m));
            }
        }
    }
    h.run_for(Duration::from_millis(2500), |h| {
        h.grow(2, root, &[0, 1, 2]);
        h.grow(3, root, &[0, 1, 2]);
    });
    let fin = h.finalized();
    eprintln!("byzantine: tips {:?}, finalized {fin:?}", (h.tips[&2].1, h.tips[&3].1));
    h.assert_safe();
    assert!(fin.iter().all(|f| *f >= root.1), "honest voters still finalize: {fin:?}");
}

#[test]
fn a_crash_between_persisting_and_sending_a_precommit_does_not_equivocate() {
    let mut h = Harness::new(4);
    h.run_for(Duration::from_millis(600), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    // Voter 0 persists its next precommit, "crashes" before sending it, and is restarted
    // from what it persisted: last completed round, its votes, its base, its finalized block.
    h.envs[0].faults.lock().drop_next_precommit = true;
    h.run_for(Duration::from_millis(200), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    h.crash(0);
    h.run_for(Duration::from_millis(200), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let restart = h.envs[0].restart.lock().clone();
    assert!(restart.is_some(), "voter 0 completed at least one round before the crash");
    h.start(0, restart);
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(1, (GENESIS, 0), &[0, 1, 2, 3]);
    });
    let tip = h.tips[&1].1;
    let fin = h.finalized();
    eprintln!("restart: tip {tip}, finalized {fin:?}, equivocators {}", h.equivocators().len());
    assert!(h.equivocators().is_empty(), "the restarted voter re-sent what it signed; nobody saw a double vote");
    assert!(fin[0] + 40 >= tip, "the restarted voter finalizes again: tip {tip}, {fin:?}");
    h.assert_safe();
}

#[test]
fn a_hidden_byzantine_equivocator_cannot_exploit_split_honest_views() {
    // Harder: honest voter 0 holds ONLY branch 2, voters 1 and 2 hold ONLY branch 3, and the
    // Byzantine voter backs each side with the votes that side wants to see. Voter 0 can count
    // only {0, byz} = 2 for branch 2; voters 1, 2 can count {1, 2, byz} = 3 for branch 3 and may
    // legitimately finalize it. What must NEVER happen is voter 0 finalizing branch 2.
    let mut h = Harness::new(3);
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(300), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2]));
    let byz = key(3);
    let ids: Vec<Id> = h.envs.iter().map(|e| e.id()).collect();
    let mut a = root;
    let mut b = root;
    for _ in 0..20 {
        a = h.grow(2, root, &[0]);
        b = h.grow(3, root, &[1, 2]);
    }
    for round in 1..400u64 {
        for (t, to) in [(a, vec![ids[0]]), (b, vec![ids[1], ids[2]])] {
            for m in [Message::Prevote(Prevote::new(t.0, t.1)), Message::Precommit(Precommit::new(t.0, t.1))] {
                h.net.inject_relayed(round, &to, sign(&byz, &GENESIS, SET_ID, round, m), Duration::from_millis(300));
            }
        }
    }
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(2, root, &[0]);
        h.grow(3, root, &[1, 2]);
    });
    let split = h.finalized();
    eprintln!("split views: finalized {split:?} (root {})", root.1);
    h.assert_safe();
    // Data heals: everyone learns both branches; branch 3 keeps growing.
    h.sync_all();
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(3, root, &[0, 1, 2]);
    });
    let fin = h.finalized();
    let rounds: Vec<Option<u64>> = h.envs.iter().map(|e| e.restart.lock().as_ref().map(|r| r.last_round)).collect();
    eprintln!("views healed: tip {}, finalized {fin:?}, last completed rounds {rounds:?}, equivocations seen {}", h.tips[&3].1, h.equivocators().len());
    h.assert_safe();
    assert!(!h.equivocators().is_empty(), "relayed, the equivocation is seen");
    let tip = h.tips[&3].1;
    assert!(fin.iter().all(|f| *f + 40 >= tip), "after data and gossip heal, finality reaches the tip again: tip {tip}, {fin:?}");
}

#[test]
fn a_permanently_hidden_equivocation_stalls_finality_safely() {
    // The liveness ASSUMPTION, pinned as a test: GRANDPA needs every vote an honest node sees
    // to reach every honest node eventually (gossip relay). If the Byzantine voter's two
    // votes are NEVER relayed, the voter that saw the "other" vote cannot rule out a hidden
    // double vote, its round never becomes completable, and finality stops — safely.
    // Measured 2026-10-08: voter 0 stuck in round 5, voters 1-2 at round 33, finality frozen
    // at the fork point + 20, no conflict. The real network relays (gossipsub); this test
    // says what happens when it does not.
    let mut h = Harness::new(3);
    let mut root = (GENESIS, 0);
    h.run_for(Duration::from_millis(300), |h| root = h.grow(1, (GENESIS, 0), &[0, 1, 2]));
    let byz = key(3);
    let ids: Vec<Id> = h.envs.iter().map(|e| e.id()).collect();
    let (mut a, mut b) = (root, root);
    for _ in 0..20 {
        a = h.grow(2, root, &[0]);
        b = h.grow(3, root, &[1, 2]);
    }
    for round in 1..400u64 {
        for (t, to) in [(a, vec![ids[0]]), (b, vec![ids[1], ids[2]])] {
            for m in [Message::Prevote(Prevote::new(t.0, t.1)), Message::Precommit(Precommit::new(t.0, t.1))] {
                h.net.inject(round, &to, sign(&byz, &GENESIS, SET_ID, round, m));
            }
        }
    }
    h.run_for(Duration::from_millis(1000), |h| {
        h.grow(2, root, &[0]);
        h.grow(3, root, &[1, 2]);
    });
    h.sync_all();
    h.run_for(Duration::from_millis(1500), |h| {
        h.grow(3, root, &[0, 1, 2]);
    });
    let tip = h.tips[&3].1;
    let fin = h.finalized();
    eprintln!("never relayed: tip {tip}, finalized {fin:?}");
    h.assert_safe();
    assert!(fin.iter().all(|f| *f + 40 < tip), "without relay finality must stall (if this starts passing, the assumption changed — find out why): tip {tip}, {fin:?}");
}
