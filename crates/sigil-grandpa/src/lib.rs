//! `sigil-grandpa` — round-based BFT finality for SIGIL's spine tree (roadmap step 1, M2.0).
//!
//! ## Why this crate exists (measured 2026-10-08 on the 4-node rehearsal net)
//!
//! 1. Per-block voting let two disjoint 3-of-4 quorums certify DIFFERENT blocks at one
//!    height (the followers voted for both producers' blocks) — a real settled fork.
//! 2. One vote per height plus a personal lock ("only vote for a block extending your last
//!    vote unless a certificate covers it") fixed that and ran cleanly for ~8 minutes, then
//!    DEADLOCKED: two validators locked on different branches, the other two were below
//!    quorum, and only a certificate could have released them.
//!
//! A single-phase vote has no safe way to release a lock. GRANDPA (Stewart &
//! Kokoris-Kogia, 2020) has none to release: each round has a prevote and a precommit,
//! and what binds the next round is each validator's running *estimate* of the previous
//! one, computed from votes everyone can see. The external review (ChatGPT, 2026-10-08)
//! recommended reusing the reference implementation rather than hand-writing the round
//! machine; three of the rules in my own draft were wrong. So the round logic here is
//! Parity's `finality-grandpa` (pinned `=0.16.3`, Apache-2.0, the code Polkadot runs), and
//! this crate supplies only SIGIL's side of the contract:
//!
//! * [`SpineTree`] — the ancestry oracle. Finality is over the `parent_hash` tree: SIGIL's
//!   settlement applies only spine blocks (side blocks are ordered but not state-applied, v0
//!   semantics), so finalizing `B` fixes the settled state up to `B`. Invariant this relies
//!   on: *same finalized spine prefix + same rules ⇒ same settled state root.*
//! * [`signing_payload`] / [`sign`] / [`verify`] — Ed25519 over a domain-separated encoding
//!   that binds genesis, voter-set id, round and phase.
//! * [`SignerGuard`] — at most ONE signed vote per `(round, phase)`, persisted before the
//!   vote leaves the node. Enforced in the SEND path, so a restarted voter that decides
//!   differently re-sends what it already signed instead of equivocating.
//! * [`SigilEnv`] — the `Environment` the reference voter drives, over a [`Transport`].
//!
//! Nothing here is wired into `sigil-node`. Ed25519 is not post-quantum; that is stated,
//! not solved.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
pub use finality_grandpa as fg;
// Re-exported so a caller (sigil-node) uses the exact versions the environment is built on.
pub use futures;
pub use parking_lot;
use fg::voter::{Callback, CommunicationIn, CommunicationOut, RoundData};
use fg::{
    Chain, Commit, Equivocation, Error, HistoricalVotes, Message, Precommit, Prevote,
    PrimaryPropose, SignedMessage,
};
use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use parking_lot::Mutex;

pub type Hash = [u8; 32];
pub type Number = u64;
/// A voter is its Ed25519 public key.
pub type Id = [u8; 32];

/// An Ed25519 signature (newtype so the reference crate's `Eq + Clone` bounds hold).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sig(pub [u8; 64]);

pub type SigilSignedMessage = SignedMessage<Hash, Number, Sig, Id>;
pub type SigilCommit = Commit<Hash, Number, Sig, Id>;
pub type SigilCatchUp = fg::CatchUp<Hash, Number, Sig, Id>;

/// Messages outside a round: a commit (justification) or a catch-up (a completed round
/// handed to a voter that fell behind, so it can jump forward instead of waiting forever).
#[derive(Clone, Debug)]
pub enum GlobalMsg {
    Commit(u64, SigilCommit),
    CatchUp(SigilCatchUp),
}

pub mod gossip;

// ─── the spine tree ────────────────────────────────────────────────────────────────

/// The `parent_hash` tree the finality gadget votes over.
#[derive(Debug, Clone)]
pub struct SpineTree {
    blocks: HashMap<Hash, (Option<Hash>, Number)>,
    children: HashMap<Hash, Vec<Hash>>,
    /// Blocks whose parent has not arrived yet: missing parent → `(hash, number)`.
    parked: HashMap<Hash, Vec<(Hash, Number)>>,
    parked_count: usize,
}

/// Most blocks parked waiting for a parent (a peer cannot grow this without bound).
const MAX_PARKED: usize = 65_536;

impl SpineTree {
    pub fn new(genesis: Hash) -> Self {
        Self::with_root(genesis, 0)
    }

    /// A tree rooted at an already-final block (a node that did not start at genesis).
    pub fn with_root(root: Hash, number: Number) -> Self {
        let mut blocks = HashMap::new();
        blocks.insert(root, (None, number));
        Self { blocks, children: HashMap::new(), parked: HashMap::new(), parked_count: 0 }
    }

    /// Add `hash` at `number` under `parent`. Refused (false) if the parent is unknown or
    /// the number is not `parent + 1` — a vote target must have a complete, checked
    /// ancestry ("vote only for what you hold"). Children parked on `hash` follow it in.
    pub fn insert(&mut self, hash: Hash, parent: Hash, number: Number) -> bool {
        if !self.insert_one(hash, parent, number) {
            return false;
        }
        let mut stack = vec![hash];
        while let Some(p) = stack.pop() {
            for (h, n) in self.parked.remove(&p).unwrap_or_default() {
                self.parked_count = self.parked_count.saturating_sub(1);
                if self.insert_one(h, p, n) {
                    stack.push(h);
                }
            }
        }
        true
    }

    fn insert_one(&mut self, hash: Hash, parent: Hash, number: Number) -> bool {
        if self.blocks.contains_key(&hash) {
            return true;
        }
        match self.blocks.get(&parent) {
            Some((_, pn)) if pn + 1 == number => {}
            _ => return false,
        }
        self.blocks.insert(hash, (Some(parent), number));
        self.children.entry(parent).or_default().push(hash);
        true
    }

    /// Like [`SpineTree::insert`], but a block whose parent is unknown is parked and joins
    /// when the parent does (blocks reach a node out of order). Returns whether it is in.
    pub fn insert_or_park(&mut self, hash: Hash, parent: Hash, number: Number) -> bool {
        if self.blocks.contains_key(&hash) {
            return true;
        }
        if self.blocks.contains_key(&parent) {
            return self.insert(hash, parent, number);
        }
        if self.parked_count < MAX_PARKED {
            self.parked.entry(parent).or_default().push((hash, number));
            self.parked_count += 1;
        }
        false
    }

    /// After `fin` is final: keep its ancestors down to `keep` heights below it and every
    /// descendant of it; everything else can never matter to finality again.
    pub fn prune_finalized(&mut self, fin: Hash, keep: Number) {
        let Some(fin_n) = self.number(&fin) else { return };
        let floor = fin_n.saturating_sub(keep);
        let mut kept: std::collections::HashSet<Hash> = std::collections::HashSet::new();
        let mut lowest;
        let mut cur = fin;
        loop {
            kept.insert(cur);
            lowest = cur;
            match self.blocks.get(&cur) {
                Some((Some(p), n)) if *n > floor && self.blocks.contains_key(p) => cur = *p,
                _ => break,
            }
        }
        let mut stack = vec![fin];
        while let Some(p) = stack.pop() {
            for k in self.children.get(&p).cloned().unwrap_or_default() {
                if kept.insert(k) {
                    stack.push(k);
                }
            }
        }
        self.blocks.retain(|h, _| kept.contains(h));
        if let Some(b) = self.blocks.get_mut(&lowest) {
            b.0 = None; // the new root
        }
        self.children.retain(|p, _| kept.contains(p));
        for kids in self.children.values_mut() {
            kids.retain(|k| kept.contains(k));
        }
        let before = self.parked_count;
        self.parked.retain(|_, v| {
            v.retain(|(_, n)| *n > fin_n);
            !v.is_empty()
        });
        self.parked_count = self.parked.values().map(|v| v.len()).sum::<usize>().min(before);
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn number(&self, h: &Hash) -> Option<Number> {
        self.blocks.get(h).map(|(_, n)| *n)
    }

    pub fn contains(&self, h: &Hash) -> bool {
        self.blocks.contains_key(h)
    }

    /// Fork choice for votes: the highest leaf that descends from (or is) `base`;
    /// ties broken by the smaller hash. The node will supply the braid's own fork
    /// choice restricted to descendants of `base`; this is the deterministic stand-in.
    pub fn best_chain_containing(&self, base: Hash) -> Option<(Hash, Number)> {
        let base_n = self.number(&base)?;
        let mut best: Option<(Hash, Number)> = None;
        for (h, (_, n)) in &self.blocks {
            if *n < base_n || self.children.get(h).is_some_and(|c| !c.is_empty()) {
                continue;
            }
            if !(h == &base || self.ancestry(base, *h).is_ok()) {
                continue;
            }
            best = match best {
                None => Some((*h, *n)),
                Some((bh, bn)) if *n > bn || (*n == bn && *h < bh) => Some((*h, *n)),
                keep => keep,
            };
        }
        best
    }
}

impl Chain<Hash, Number> for SpineTree {
    /// Blocks strictly between `base` and `block`, newest first (the reference crate's
    /// contract). `NotDescendent` if `block` does not descend from `base`.
    fn ancestry(&self, base: Hash, block: Hash) -> Result<Vec<Hash>, Error> {
        let base_n = self.number(&base).ok_or(Error::NotDescendent)?;
        let mut out = Vec::new();
        let mut cur = block;
        loop {
            let (parent, _) = *self.blocks.get(&cur).ok_or(Error::NotDescendent)?;
            let parent = parent.ok_or(Error::NotDescendent)?;
            if parent == base {
                return Ok(out);
            }
            if self.number(&parent).is_none_or(|n| n <= base_n) {
                return Err(Error::NotDescendent);
            }
            out.push(parent);
            cur = parent;
        }
    }
}

// ─── signing ───────────────────────────────────────────────────────────────────────

const DOMAIN: &[u8] = b"SIGIL_GRANDPA_V0";

fn phase_tag(msg: &Message<Hash, Number>) -> u8 {
    match msg {
        Message::Prevote(_) => 0,
        Message::Precommit(_) => 1,
        Message::PrimaryPropose(_) => 2,
    }
}

fn target(msg: &Message<Hash, Number>) -> (Hash, Number) {
    match msg {
        Message::Prevote(v) => (v.target_hash, v.target_number),
        Message::Precommit(v) => (v.target_hash, v.target_number),
        Message::PrimaryPropose(v) => (v.target_hash, v.target_number),
    }
}

fn with_target(phase: u8, (h, n): (Hash, Number)) -> Message<Hash, Number> {
    match phase {
        0 => Message::Prevote(Prevote::new(h, n)),
        1 => Message::Precommit(Precommit::new(h, n)),
        _ => Message::PrimaryPropose(PrimaryPropose::new(h, n)),
    }
}

/// `"SIGIL_GRANDPA_V0" || genesis || set_id || round || phase || target_hash || target_number`.
pub fn signing_payload(genesis: &Hash, set_id: u64, round: u64, msg: &Message<Hash, Number>) -> Vec<u8> {
    let (h, n) = target(msg);
    let mut p = Vec::with_capacity(DOMAIN.len() + 32 + 8 + 8 + 1 + 32 + 8);
    p.extend_from_slice(DOMAIN);
    p.extend_from_slice(genesis);
    p.extend_from_slice(&set_id.to_le_bytes());
    p.extend_from_slice(&round.to_le_bytes());
    p.push(phase_tag(msg));
    p.extend_from_slice(&h);
    p.extend_from_slice(&n.to_le_bytes());
    p
}

pub fn sign(key: &SigningKey, genesis: &Hash, set_id: u64, round: u64, msg: Message<Hash, Number>) -> SigilSignedMessage {
    let sig = key.sign(&signing_payload(genesis, set_id, round, &msg));
    SignedMessage { message: msg, signature: Sig(sig.to_bytes()), id: key.verifying_key().to_bytes() }
}

pub fn verify(genesis: &Hash, set_id: u64, round: u64, m: &SigilSignedMessage) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(&m.id) else { return false };
    vk.verify(&signing_payload(genesis, set_id, round, &m.message), &Signature::from_bytes(&m.signature.0)).is_ok()
}

/// Every precommit in `commit` is signed by its claimed voter for `round`.
/// (Weight, distinctness and ancestry are checked by the reference crate's commit logic.)
pub fn verify_commit_signatures(genesis: &Hash, set_id: u64, round: u64, commit: &SigilCommit) -> bool {
    commit.precommits.iter().all(|sp| {
        let m = SignedMessage {
            message: Message::Precommit(sp.precommit.clone()),
            signature: sp.signature.clone(),
            id: sp.id,
        };
        verify(genesis, set_id, round, &m)
    })
}

/// Every prevote and precommit in a catch-up is signed by its claimed voter for its round.
pub fn verify_catch_up_signatures(genesis: &Hash, set_id: u64, c: &SigilCatchUp) -> bool {
    let r = c.round_number;
    c.prevotes.iter().all(|p| {
        verify(genesis, set_id, r, &SignedMessage { message: Message::Prevote(p.prevote.clone()), signature: p.signature.clone(), id: p.id })
    }) && c.precommits.iter().all(|p| {
        verify(genesis, set_id, r, &SignedMessage { message: Message::Precommit(p.precommit.clone()), signature: p.signature.clone(), id: p.id })
    })
}

// ─── the signer guard ──────────────────────────────────────────────────────────────

/// At most one signed vote per `(round, phase)`, ever, for this voter-set id.
///
/// The reference voter calls `Environment::prevoted`/`precommitted` and then pushes the
/// vote to the outgoing sink; the guard sits in the sink, where the signature is made.
/// If a vote for this `(round, phase)` was already signed (before a crash, say), the
/// recorded one is sent again and the new one is dropped. The record is written (fsync +
/// rename) before the signed vote leaves. An unreadable record signs nothing.
#[derive(Debug)]
pub struct SignerGuard {
    path: Option<PathBuf>,
    set_id: u64,
    votes: BTreeMap<(u64, u8), (Hash, Number)>,
    poisoned: bool,
}

/// Rounds older than this many below the newest signed one are forgotten: the reference
/// voter only casts votes in its current round.
const GUARD_KEEP_ROUNDS: u64 = 256;

impl SignerGuard {
    pub fn in_memory(set_id: u64) -> Self {
        Self { path: None, set_id, votes: BTreeMap::new(), poisoned: false }
    }

    pub fn load(path: PathBuf, set_id: u64) -> Self {
        let mut g = Self { path: Some(path.clone()), set_id, votes: BTreeMap::new(), poisoned: false };
        match std::fs::read(&path) {
            Err(_) => g, // fresh voter
            Ok(bytes) => {
                let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| {
                    if v.get("set_id")?.as_u64()? != set_id {
                        return None;
                    }
                    let mut out = BTreeMap::new();
                    for e in v.get("votes")?.as_array()? {
                        let r = e.get(0)?.as_u64()?;
                        let p = e.get(1)?.as_u64()? as u8;
                        let h: Hash = hex::decode(e.get(2)?.as_str()?).ok()?.try_into().ok()?;
                        let n = e.get(3)?.as_u64()?;
                        out.insert((r, p), (h, n));
                    }
                    Some(out)
                });
                match parsed {
                    Some(v) => g.votes = v,
                    None => g.poisoned = true,
                }
                g
            }
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The highest round anything was signed in. A restarted voter starts ABOVE it, so it
    /// never re-enters a round whose record may have been pruned.
    pub fn newest_round(&self) -> u64 {
        self.votes.keys().map(|(r, _)| *r).max().unwrap_or(0)
    }

    /// What may be signed for `(round, msg)`: the recorded vote if one exists, else `msg`
    /// (recorded first). `None` = sign nothing (poisoned, or the record could not be saved).
    pub fn decide(&mut self, round: u64, msg: Message<Hash, Number>) -> Option<Message<Hash, Number>> {
        if self.poisoned {
            return None;
        }
        let phase = phase_tag(&msg);
        if let Some(t) = self.votes.get(&(round, phase)) {
            return Some(with_target(phase, *t));
        }
        self.votes.insert((round, phase), target(&msg));
        if let Some(newest) = self.votes.keys().map(|(r, _)| *r).max() {
            let floor = newest.saturating_sub(GUARD_KEEP_ROUNDS);
            self.votes.retain(|(r, _), _| *r >= floor);
        }
        if self.persist().is_err() {
            self.votes.remove(&(round, phase));
            return None;
        }
        Some(msg)
    }

    fn persist(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        use std::io::Write;
        let votes: Vec<serde_json::Value> = self
            .votes
            .iter()
            .map(|((r, p), (h, n))| serde_json::json!([r, p, hex::encode(h), n]))
            .collect();
        let body = serde_json::json!({ "set_id": self.set_id, "votes": votes }).to_string();
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    }
}

// ─── transport ─────────────────────────────────────────────────────────────────────

/// How votes and commits move between validators. The node will implement it over its
/// gossip topics; the tests implement it in memory with partitions and Byzantine injection.
pub trait Transport: Send + Sync + 'static {
    /// Every signed message of `round`, including ones sent before this call.
    fn subscribe_round(&self, round: u64, me: Id) -> UnboundedReceiver<SigilSignedMessage>;
    fn broadcast_round(&self, round: u64, from: Id, msg: SigilSignedMessage);
    /// Commits and catch-ups.
    fn subscribe_global(&self, me: Id) -> UnboundedReceiver<GlobalMsg>;
    fn broadcast_global(&self, from: Id, round: u64, commit: SigilCommit);
    /// A round this voter completed, with every vote it saw: what a catch-up is made of.
    fn note_completed(&self, _round: u64, _base: (Hash, Number), _votes: &[SigilSignedMessage]) {}
}

// ─── the environment ───────────────────────────────────────────────────────────────

/// What a voter needs to restart where it stopped: the last completed round, its votes,
/// its base, and the last finalized block (ChatGPT 10-08: "restore round state, vote
/// evidence and finalized base, not only the signer guard").
#[derive(Clone, Debug)]
pub struct RestartState {
    pub last_round: u64,
    pub last_round_votes: Vec<SigilSignedMessage>,
    pub last_round_base: (Hash, Number),
    pub last_finalized: (Hash, Number),
}

/// Test-only fault injection: drop this voter's next outgoing precommit AFTER the guard
/// recorded it (= crashed between persist and broadcast).
#[derive(Default, Debug)]
pub struct Faults {
    pub drop_next_precommit: bool,
}

pub struct SigilEnv<T: Transport> {
    pub tree: Arc<Mutex<SpineTree>>,
    key: SigningKey,
    id: Id,
    genesis: Hash,
    set_id: u64,
    transport: Arc<T>,
    guard: Arc<Mutex<SignerGuard>>,
    gossip: Duration,
    finalized: Mutex<(Hash, Number)>,
    /// `(voter, hash, number)` for every finalization, shared across voters in tests.
    pub finality_log: Arc<Mutex<Vec<(Id, Hash, Number)>>>,
    pub safety_violations: Mutex<Vec<String>>,
    pub equivocations: Mutex<Vec<(u64, Id)>>,
    pub restart: Mutex<Option<RestartState>>,
    pub faults: Arc<Mutex<Faults>>,
    /// The node's own fork choice (the braid's selected tip, held back to a fully
    /// validatable block). Used as the vote target when it descends from the round's base.
    pub preferred: Arc<Mutex<Option<(Hash, Number)>>>,
    on_finalize: Mutex<Option<Box<dyn Fn(Hash, Number, u64) + Send + Sync>>>,
    /// Bases of the last few completed rounds. A new round's base is the voter's last
    /// finalized block AT ITS START, which can lag far behind after a jump (a heal after a
    /// split finalized 114 → 485 at once); pruning must keep the chain down to the oldest
    /// base still in use, or every vote is "not a descendant of the round base" and ignored.
    recent_bases: Mutex<std::collections::VecDeque<Number>>,
}

impl<T: Transport> SigilEnv<T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tree: SpineTree,
        key: SigningKey,
        genesis: Hash,
        set_id: u64,
        transport: Arc<T>,
        guard: Arc<Mutex<SignerGuard>>,
        gossip: Duration,
        last_finalized: (Hash, Number),
        finality_log: Arc<Mutex<Vec<(Id, Hash, Number)>>>,
    ) -> Self {
        Self {
            tree: Arc::new(Mutex::new(tree)),
            id: key.verifying_key().to_bytes(),
            key,
            genesis,
            set_id,
            transport,
            guard,
            gossip,
            finalized: Mutex::new(last_finalized),
            finality_log,
            safety_violations: Mutex::new(Vec::new()),
            equivocations: Mutex::new(Vec::new()),
            restart: Mutex::new(None),
            faults: Arc::new(Mutex::new(Faults::default())),
            preferred: Arc::new(Mutex::new(None)),
            on_finalize: Mutex::new(None),
            recent_bases: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Called (hash, number, round) after every block this voter finalizes.
    pub fn set_on_finalize(&self, f: Box<dyn Fn(Hash, Number, u64) + Send + Sync>) {
        *self.on_finalize.lock() = Some(f);
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn last_finalized(&self) -> (Hash, Number) {
        *self.finalized.lock()
    }

    /// The `(GlobalIn, GlobalOut)` pair `Voter::new` takes: commits and catch-ups in
    /// (every signature checked, held until their blocks are known), commits out.
    #[allow(clippy::type_complexity)]
    pub fn global_comms(
        &self,
    ) -> (
        Pin<Box<dyn Stream<Item = Result<CommunicationIn<Hash, Number, Sig, Id>, Error>> + Send>>,
        Pin<Box<dyn Sink<CommunicationOut<Hash, Number, Sig, Id>, Error = Error> + Send>>,
    ) {
        let (genesis, set_id) = (self.genesis, self.set_id);
        let incoming = held_until_known(
            self.transport.subscribe_global(self.id),
            self.tree.clone(),
            self.gossip,
            move |g: &GlobalMsg| match g {
                GlobalMsg::Commit(round, commit) => verify_commit_signatures(&genesis, set_id, *round, commit),
                GlobalMsg::CatchUp(c) => verify_catch_up_signatures(&genesis, set_id, c),
            },
            |g: &GlobalMsg| match g {
                GlobalMsg::Commit(_, commit) => {
                    let mut v = vec![commit.target_hash];
                    v.extend(commit.precommits.iter().map(|p| p.precommit.target_hash));
                    v
                }
                GlobalMsg::CatchUp(c) => {
                    let mut v = vec![c.base_hash];
                    v.extend(c.prevotes.iter().map(|p| p.prevote.target_hash));
                    v.extend(c.precommits.iter().map(|p| p.precommit.target_hash));
                    v
                }
            },
        )
        .map(|g| {
            Ok(match g {
                GlobalMsg::Commit(round, commit) => CommunicationIn::Commit(round, commit.into(), Callback::Blank),
                GlobalMsg::CatchUp(c) => CommunicationIn::CatchUp(c, Callback::Blank),
            })
        });
        let (transport, me) = (self.transport.clone(), self.id);
        let outgoing = sink::unfold((), move |(), out: CommunicationOut<Hash, Number, Sig, Id>| {
            let CommunicationOut::Commit(round, commit) = out;
            transport.broadcast_global(me, round, commit);
            future::ready(Ok::<(), Error>(()))
        });
        (Box::pin(incoming), Box::pin(outgoing))
    }
}

impl<T: Transport> Chain<Hash, Number> for SigilEnv<T> {
    fn ancestry(&self, base: Hash, block: Hash) -> Result<Vec<Hash>, Error> {
        self.tree.lock().ancestry(base, block)
    }
}

type BoxTimer = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

/// Completed rounds whose bases bound pruning (see `SigilEnv::recent_bases`).
const RECENT_BASES: usize = 16;
/// Heights kept below the oldest base in use.
const PRUNE_MARGIN: Number = 64;

/// Most votes held back for blocks we do not have yet, per round stream. A peer that
/// floods votes for blocks that never arrive costs at most this much memory.
const MAX_HELD_VOTES: usize = 4096;

/// Incoming messages, checked by `valid`, with one addition to the reference crate's
/// contract: a message naming a block we do not hold yet is HELD (not dropped) and released
/// once every block it names (`needs`) has arrived. The reference voter requires this for
/// commits ("commits which correspond to known blocks only") and would otherwise lose a vote
/// to a failed ancestry check; on a real network votes and certificates outrun block data.
/// Measured in the split-view Byzantine test (2026-10-08): without it for commits, a voter
/// was asked to finalize a block it had never seen.
fn held_until_known<M: Send + 'static>(
    mut rx: UnboundedReceiver<M>,
    tree: Arc<Mutex<SpineTree>>,
    gossip: Duration,
    valid: impl Fn(&M) -> bool + Send + 'static,
    needs: impl Fn(&M) -> Vec<Hash> + Send + 'static,
) -> Pin<Box<dyn Stream<Item = M> + Send>> {
    let mut held: Vec<M> = Vec::new();
    let mut tick: Option<futures_timer::Delay> = None;
    let mut closed = false;
    let known = move |t: &SpineTree, m: &M| needs(m).iter().all(|h| t.contains(h));
    Box::pin(stream::poll_fn(move |cx| {
        use std::task::Poll;
        if let Some(i) = {
            let t = tree.lock();
            held.iter().position(|m| known(&t, m))
        } {
            return Poll::Ready(Some(held.remove(i)));
        }
        while !closed {
            match rx.poll_next_unpin(cx) {
                Poll::Ready(Some(m)) => {
                    if !valid(&m) {
                        continue;
                    }
                    if known(&tree.lock(), &m) {
                        return Poll::Ready(Some(m));
                    }
                    if held.len() >= MAX_HELD_VOTES {
                        held.remove(0);
                    }
                    held.push(m);
                }
                Poll::Ready(None) => closed = true,
                Poll::Pending => break,
            }
        }
        if held.is_empty() {
            return if closed { Poll::Ready(None) } else { Poll::Pending };
        }
        // Re-check what is held on a short timer: blocks arrive outside this stream.
        let t = tick.get_or_insert_with(|| futures_timer::Delay::new(gossip / 2));
        if t.poll_unpin(cx).is_ready() {
            tick = None;
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }))
}

impl<T: Transport> fg::voter::Environment<Hash, Number> for SigilEnv<T> {
    type Timer = BoxTimer;
    type BestChain = Pin<Box<dyn Future<Output = Result<Option<(Hash, Number)>, Error>> + Send>>;
    type Id = Id;
    type Signature = Sig;
    type In = Pin<Box<dyn Stream<Item = Result<SigilSignedMessage, Error>> + Send>>;
    type Out = Pin<Box<dyn Sink<Message<Hash, Number>, Error = Error> + Send>>;
    type Error = Error;

    fn best_chain_containing(&self, base: Hash) -> Self::BestChain {
        let tree = self.tree.lock();
        let pick = match *self.preferred.lock() {
            Some((h, n)) if tree.contains(&h) && (h == base || tree.ancestry(base, h).is_ok()) => Some((h, n)),
            _ => tree.best_chain_containing(base),
        };
        Box::pin(future::ready(Ok(pick)))
    }

    fn round_data(&self, round: u64) -> RoundData<Id, Self::Timer, Self::In, Self::Out> {
        let (genesis, set_id) = (self.genesis, self.set_id);
        let incoming = held_until_known(
            self.transport.subscribe_round(round, self.id),
            self.tree.clone(),
            self.gossip,
            move |m| verify(&genesis, set_id, round, m),
            |m: &SigilSignedMessage| vec![target(&m.message).0],
        )
        .map(Ok);
        let (transport, guard, key, me, faults) =
            (self.transport.clone(), self.guard.clone(), self.key.clone(), self.id, self.faults.clone());
        let outgoing = sink::unfold((), move |(), msg: Message<Hash, Number>| {
            // Guard first: the recorded vote for this (round, phase) wins over a new one.
            if let Some(m) = guard.lock().decide(round, msg) {
                let crash_before_send = matches!(m, Message::Precommit(_)) && {
                    let mut f = faults.lock();
                    std::mem::take(&mut f.drop_next_precommit)
                };
                if !crash_before_send {
                    transport.broadcast_round(round, me, sign(&key, &genesis, set_id, round, m));
                }
            }
            future::ready(Ok::<(), Error>(()))
        });
        RoundData {
            voter_id: Some(self.id),
            prevote_timer: Box::pin(futures_timer::Delay::new(self.gossip * 2).map(Ok)),
            precommit_timer: Box::pin(futures_timer::Delay::new(self.gossip * 4).map(Ok)),
            incoming: Box::pin(incoming),
            outgoing: Box::pin(outgoing),
        }
    }

    fn round_commit_timer(&self) -> Self::Timer {
        // Spread commit broadcasts so not every voter sends the same commit at once.
        let jitter = Duration::from_millis(u64::from(self.id[0]) % (self.gossip.as_millis() as u64).max(1));
        Box::pin(futures_timer::Delay::new(jitter).map(Ok))
    }

    fn proposed(&self, _round: u64, _propose: PrimaryPropose<Hash, Number>) -> Result<(), Error> {
        Ok(())
    }

    // Persistence of our own votes happens in the send path (`SignerGuard::decide`), which
    // is where the signature is made; these hooks run just before it.
    fn prevoted(&self, _round: u64, _prevote: Prevote<Hash, Number>) -> Result<(), Error> {
        Ok(())
    }

    fn precommitted(&self, _round: u64, _precommit: Precommit<Hash, Number>) -> Result<(), Error> {
        Ok(())
    }

    fn completed(
        &self,
        round: u64,
        state: fg::round::State<Hash, Number>,
        base: (Hash, Number),
        votes: &HistoricalVotes<Hash, Number, Sig, Id>,
    ) -> Result<(), Error> {
        let _ = state;
        {
            let mut b = self.recent_bases.lock();
            b.push_back(base.1);
            while b.len() > RECENT_BASES {
                b.pop_front();
            }
        }
        self.transport.note_completed(round, base, votes.seen());
        *self.restart.lock() = Some(RestartState {
            last_round: round,
            last_round_votes: votes.seen().to_vec(),
            last_round_base: base,
            last_finalized: self.last_finalized(),
        });
        Ok(())
    }

    fn concluded(
        &self,
        _round: u64,
        _state: fg::round::State<Hash, Number>,
        _base: (Hash, Number),
        _votes: &HistoricalVotes<Hash, Number, Sig, Id>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn finalize_block(&self, hash: Hash, number: Number, round: u64, _commit: SigilCommit) -> Result<(), Error> {
        let mut fin = self.finalized.lock();
        let tree = self.tree.lock();
        if !tree.contains(&hash) {
            self.safety_violations.lock().push(format!(
                "round {round}: asked to finalize {}@{number}, a block this node does not hold (environment contract)",
                hex::encode(&hash[..4])
            ));
            return Ok(());
        }
        let extends = hash == fin.0 || tree.ancestry(fin.0, hash).is_ok();
        if number <= fin.1 || !extends {
            self.safety_violations.lock().push(format!(
                "round {round}: finalize {}@{number} does not extend {}@{}",
                hex::encode(&hash[..4]), hex::encode(&fin.0[..4]), fin.1
            ));
            return Ok(());
        }
        *fin = (hash, number);
        drop(tree);
        self.finality_log.lock().push((self.id, hash, number));
        if let Some(r) = self.restart.lock().as_mut() {
            r.last_finalized = (hash, number);
        }
        // Off-chain branches below this block can never matter again; the finalized chain
        // itself is kept down to the oldest base a recent round still counts from.
        let base_floor = self.recent_bases.lock().iter().min().copied().unwrap_or(0);
        let keep = number.saturating_sub(base_floor.saturating_sub(PRUNE_MARGIN)).max(PRUNE_MARGIN);
        self.tree.lock().prune_finalized(hash, keep);
        if let Some(f) = self.on_finalize.lock().as_ref() {
            f(hash, number, round);
        }
        Ok(())
    }

    fn prevote_equivocation(&self, round: u64, e: Equivocation<Id, Prevote<Hash, Number>, Sig>) {
        self.equivocations.lock().push((round, e.identity));
    }

    fn precommit_equivocation(&self, round: u64, e: Equivocation<Id, Precommit<Hash, Number>, Sig>) {
        self.equivocations.lock().push((round, e.identity));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(i: u8) -> Hash {
        [i; 32]
    }

    #[test]
    fn ancestry_follows_the_parent_walk_and_refuses_other_branches() {
        let mut t = SpineTree::new(h(0));
        assert!(t.insert(h(1), h(0), 1));
        assert!(t.insert(h(2), h(1), 2));
        assert!(t.insert(h(3), h(2), 3));
        assert!(t.insert(h(12), h(1), 2)); // sibling branch
        assert!(!t.insert(h(9), h(8), 5), "unknown parent: refused");
        assert!(!t.insert(h(9), h(3), 7), "wrong number: refused");
        assert_eq!(t.ancestry(h(0), h(3)), Ok(vec![h(2), h(1)]));
        assert_eq!(t.ancestry(h(2), h(3)), Ok(vec![]));
        assert_eq!(t.ancestry(h(12), h(3)), Err(Error::NotDescendent));
        assert!(t.is_equal_or_descendent_of(h(1), h(12)));
        assert_eq!(t.best_chain_containing(h(0)), Some((h(3), 3)));
        assert_eq!(t.best_chain_containing(h(12)), Some((h(12), 2)));
    }

    #[test]
    fn a_vote_signature_binds_genesis_set_round_and_phase() {
        let k = SigningKey::from_bytes(&[7; 32]);
        let m = sign(&k, &h(0), 1, 5, Message::Prevote(Prevote::new(h(3), 3)));
        assert!(verify(&h(0), 1, 5, &m));
        assert!(!verify(&h(0), 1, 6, &m), "another round");
        assert!(!verify(&h(0), 2, 5, &m), "another voter set");
        assert!(!verify(&h(9), 1, 5, &m), "another chain");
        let mut as_precommit = m.clone();
        as_precommit.message = Message::Precommit(Precommit::new(h(3), 3));
        assert!(!verify(&h(0), 1, 5, &as_precommit), "another phase");
    }

    #[test]
    fn the_guard_resends_what_it_signed_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("sigil-grandpa-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("guard.json");
        let mut g = SignerGuard::load(path.clone(), 1);
        let first = Message::Precommit(Precommit::new(h(3), 3));
        assert_eq!(g.decide(4, first.clone()), Some(first.clone()));
        // Same round and phase, a different target: the recorded vote is sent instead.
        assert_eq!(g.decide(4, Message::Precommit(Precommit::new(h(12), 2))), Some(first.clone()));
        // Another phase in the same round is its own slot.
        let pv = Message::Prevote(Prevote::new(h(12), 2));
        assert_eq!(g.decide(4, pv.clone()), Some(pv));
        drop(g);
        let mut again = SignerGuard::load(path.clone(), 1);
        assert_eq!(again.decide(4, Message::Precommit(Precommit::new(h(1), 1))), Some(first), "restart keeps it");
        // Another voter set does not inherit (the file is for set 1): treated as unreadable.
        assert!(SignerGuard::load(path.clone(), 2).is_poisoned());
        std::fs::write(&path, b"{garbage").unwrap();
        let mut bad = SignerGuard::load(path, 1);
        assert_eq!(bad.decide(9, Message::Prevote(Prevote::new(h(1), 1))), None, "unreadable record signs nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
