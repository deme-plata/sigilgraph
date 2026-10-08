//! The real-network [`Transport`]: GRANDPA messages over a gossip topic.
//!
//! The in-memory test network keeps every message forever, so a voter that starts a round
//! late still sees it all. A gossip network does not. Three things make up the difference,
//! all as in Substrate's GRANDPA networking, in their simplest form:
//!
//! 1. **Round buffers.** Votes of the last [`KEEP_ROUNDS`] rounds are kept and replayed to
//!    a round the local voter opens later.
//! 2. **Rebroadcast.** [`GossipTransport::tick`] re-publishes every vote seen in the current
//!    and previous round, plus a `neighbor` line saying which round we are in. A lost
//!    message is repaired, and a Byzantine voter's two different votes reach everyone, so a
//!    hidden equivocation becomes visible (the liveness assumption pinned by the simulation
//!    test `a_permanently_hidden_equivocation_stalls_finality_safely`).
//! 3. **Catch-up.** A peer whose votes or neighbor line show an older round than our last
//!    completed one gets that completed round (all its prevotes and precommits); the
//!    reference voter validates it (supermajority, completable) and jumps forward.
//!
//! The wire is JSON tagged `"grandpa"`, so it shares the old finality-votes topic without
//! confusing either decoder. Nothing here holds a lock while publishing: `publish` may run
//! arbitrary code (a test bus, the node's gossip).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::fg::{CatchUp, Message, Precommit, Prevote, SignedPrecommit, SignedPrevote, SignedMessage};
use crate::{GlobalMsg, Hash, Id, Number, Sig, SigilCommit, SigilSignedMessage, Transport};

/// Rounds of votes kept for replay and catch-up.
pub const KEEP_ROUNDS: u64 = 8;
/// At most one catch-up per this interval (it carries a whole round of votes).
const CATCH_UP_EVERY: Duration = Duration::from_millis(1000);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WireVote {
    pub hash: String,
    pub number: u64,
    pub id: String,
    pub sig: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "grandpa", rename_all = "snake_case")]
pub enum Wire {
    Vote { set: u64, round: u64, phase: u8, v: WireVote },
    Commit { set: u64, round: u64, hash: String, number: u64, pc: Vec<WireVote> },
    CatchUp { set: u64, round: u64, base_hash: String, base_number: u64, pv: Vec<WireVote>, pc: Vec<WireVote> },
    Neighbor { set: u64, round: u64 },
}

fn h32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn sig64(s: &str) -> Option<Sig> {
    Some(Sig(hex::decode(s).ok()?.try_into().ok()?))
}

fn wv(hash: &Hash, number: Number, id: &Id, sig: &Sig) -> WireVote {
    WireVote { hash: hex::encode(hash), number, id: hex::encode(id), sig: hex::encode(sig.0) }
}

fn parts(w: &WireVote) -> Option<(Hash, Number, Id, Sig)> {
    Some((h32(&w.hash)?, w.number, h32(&w.id)?, sig64(&w.sig)?))
}

/// Encode one signed round message.
pub fn encode_vote(set: u64, round: u64, m: &SigilSignedMessage) -> Vec<u8> {
    let (phase, h, n) = match &m.message {
        Message::Prevote(v) => (0u8, v.target_hash, v.target_number),
        Message::Precommit(v) => (1, v.target_hash, v.target_number),
        Message::PrimaryPropose(v) => (2, v.target_hash, v.target_number),
    };
    serde_json::to_vec(&Wire::Vote { set, round, phase, v: wv(&h, n, &m.id, &m.signature) }).unwrap_or_default()
}

fn decode_vote(phase: u8, v: &WireVote) -> Option<SigilSignedMessage> {
    let (h, n, id, sig) = parts(v)?;
    let message = match phase {
        0 => Message::Prevote(Prevote::new(h, n)),
        1 => Message::Precommit(Precommit::new(h, n)),
        2 => Message::PrimaryPropose(crate::fg::PrimaryPropose::new(h, n)),
        _ => return None,
    };
    Some(SignedMessage { message, signature: sig, id })
}

#[derive(Default)]
struct RoundBuf {
    msgs: Vec<SigilSignedMessage>,
    subs: Vec<UnboundedSender<SigilSignedMessage>>,
}

#[derive(Default)]
struct Inner {
    rounds: BTreeMap<u64, RoundBuf>,
    global_subs: Vec<UnboundedSender<GlobalMsg>>,
    my_round: u64,
    completed: Option<(u64, (Hash, Number), Vec<SigilSignedMessage>)>,
    last_catch_up: Option<Instant>,
}

impl Inner {
    fn prune(&mut self) {
        let floor = self.my_round.saturating_sub(KEEP_ROUNDS);
        self.rounds.retain(|r, _| *r >= floor);
    }

    /// Store and deliver a round message; `false` if it was a duplicate or too old.
    fn accept(&mut self, round: u64, m: SigilSignedMessage) -> bool {
        if round + KEEP_ROUNDS < self.my_round {
            return false;
        }
        let buf = self.rounds.entry(round).or_default();
        if buf.msgs.contains(&m) {
            return false;
        }
        buf.subs.retain(|tx| tx.unbounded_send(m.clone()).is_ok());
        buf.msgs.push(m);
        true
    }
}

pub struct GossipTransport {
    set_id: u64,
    publish: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    inner: Mutex<Inner>,
}

impl GossipTransport {
    pub fn new(set_id: u64, publish: Box<dyn Fn(Vec<u8>) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { set_id, publish, inner: Mutex::new(Inner::default()) })
    }

    /// The round the local voter is in (highest round it opened).
    pub fn my_round(&self) -> u64 {
        self.inner.lock().my_round
    }

    /// `(phase, target number, voter id prefix)` of every vote held for `round` — diagnostics.
    pub fn round_summary(&self, round: u64) -> Vec<(u8, Number, u8)> {
        let n = self.inner.lock();
        n.rounds.get(&round).map(|b| b.msgs.iter().map(|m| match &m.message {
            Message::Prevote(v) => (0, v.target_number, m.id[0]),
            Message::Precommit(v) => (1, v.target_number, m.id[0]),
            Message::PrimaryPropose(v) => (2, v.target_number, m.id[0]),
        }).collect()).unwrap_or_default()
    }

    /// Everything received on the gossip topic. Malformed input and other voter sets are
    /// ignored; signatures are checked by the environment, not here.
    pub fn on_wire(&self, bytes: &[u8]) {
        let Ok(w) = serde_json::from_slice::<Wire>(bytes) else { return };
        match w {
            Wire::Vote { set, round, phase, v } if set == self.set_id => {
                if let Some(m) = decode_vote(phase, &v) {
                    self.inner.lock().accept(round, m);
                    self.maybe_catch_up(round);
                }
            }
            Wire::Commit { set, round, hash, number, pc } if set == self.set_id => {
                let Some(target_hash) = h32(&hash) else { return };
                let precommits: Option<Vec<_>> = pc
                    .iter()
                    .map(|v| parts(v).map(|(h, n, id, sig)| SignedPrecommit { precommit: Precommit::new(h, n), signature: sig, id }))
                    .collect();
                let Some(precommits) = precommits else { return };
                let c = SigilCommit { target_hash, target_number: number, precommits };
                self.inner.lock().global_subs.retain(|tx| tx.unbounded_send(GlobalMsg::Commit(round, c.clone())).is_ok());
            }
            Wire::CatchUp { set, round, base_hash, base_number, pv, pc } if set == self.set_id => {
                if round <= self.my_round() {
                    return;
                }
                let Some(base_hash) = h32(&base_hash) else { return };
                let prevotes: Option<Vec<_>> = pv
                    .iter()
                    .map(|v| parts(v).map(|(h, n, id, sig)| SignedPrevote { prevote: Prevote::new(h, n), signature: sig, id }))
                    .collect();
                let precommits: Option<Vec<_>> = pc
                    .iter()
                    .map(|v| parts(v).map(|(h, n, id, sig)| SignedPrecommit { precommit: Precommit::new(h, n), signature: sig, id }))
                    .collect();
                let (Some(prevotes), Some(precommits)) = (prevotes, precommits) else { return };
                let c = CatchUp { round_number: round, prevotes, precommits, base_hash, base_number };
                self.inner.lock().global_subs.retain(|tx| tx.unbounded_send(GlobalMsg::CatchUp(c.clone())).is_ok());
            }
            Wire::Neighbor { set, round } if set == self.set_id => self.maybe_catch_up(round),
            _ => {}
        }
    }

    /// A peer at `peer_round` is behind our last completed round: send it that round.
    fn maybe_catch_up(&self, peer_round: u64) {
        let msg = {
            let mut n = self.inner.lock();
            let Some((r, base, votes)) = n.completed.clone() else { return };
            if peer_round >= r || n.last_catch_up.is_some_and(|t| t.elapsed() < CATCH_UP_EVERY) {
                return;
            }
            n.last_catch_up = Some(Instant::now());
            let (mut pv, mut pc) = (Vec::new(), Vec::new());
            for m in &votes {
                match &m.message {
                    Message::Prevote(v) => pv.push(wv(&v.target_hash, v.target_number, &m.id, &m.signature)),
                    Message::Precommit(v) => pc.push(wv(&v.target_hash, v.target_number, &m.id, &m.signature)),
                    Message::PrimaryPropose(_) => {}
                }
            }
            Wire::CatchUp { set: self.set_id, round: r, base_hash: hex::encode(base.0), base_number: base.1, pv, pc }
        };
        (self.publish)(serde_json::to_vec(&msg).unwrap_or_default());
    }

    /// Call every few gossip periods: say which round we are in, and re-publish every vote
    /// of the current and previous round (repairs losses; exposes hidden equivocation).
    pub fn tick(&self) {
        let (round, out) = {
            let n = self.inner.lock();
            let mut out = Vec::new();
            for (r, buf) in n.rounds.range(n.my_round.saturating_sub(1)..) {
                for m in &buf.msgs {
                    out.push(encode_vote(self.set_id, *r, m));
                }
            }
            (n.my_round, out)
        };
        (self.publish)(serde_json::to_vec(&Wire::Neighbor { set: self.set_id, round }).unwrap_or_default());
        for b in out {
            (self.publish)(b);
        }
    }
}

impl Transport for GossipTransport {
    fn subscribe_round(&self, round: u64, _me: Id) -> UnboundedReceiver<SigilSignedMessage> {
        let (tx, rx) = unbounded();
        let mut n = self.inner.lock();
        n.my_round = n.my_round.max(round);
        n.prune();
        let buf = n.rounds.entry(round).or_default();
        for m in &buf.msgs {
            let _ = tx.unbounded_send(m.clone());
        }
        buf.subs.push(tx);
        rx
    }

    fn broadcast_round(&self, round: u64, _from: Id, msg: SigilSignedMessage) {
        let bytes = encode_vote(self.set_id, round, &msg);
        self.inner.lock().accept(round, msg); // our own vote counts in our own round too
        (self.publish)(bytes);
    }

    fn subscribe_global(&self, _me: Id) -> UnboundedReceiver<GlobalMsg> {
        let (tx, rx) = unbounded();
        self.inner.lock().global_subs.push(tx);
        rx
    }

    fn broadcast_global(&self, _from: Id, round: u64, commit: SigilCommit) {
        let pc = commit
            .precommits
            .iter()
            .map(|p| wv(&p.precommit.target_hash, p.precommit.target_number, &p.id, &p.signature))
            .collect();
        let w = Wire::Commit { set: self.set_id, round, hash: hex::encode(commit.target_hash), number: commit.target_number, pc };
        (self.publish)(serde_json::to_vec(&w).unwrap_or_default());
    }

    fn note_completed(&self, round: u64, base: (Hash, Number), votes: &[SigilSignedMessage]) {
        let mut n = self.inner.lock();
        if n.completed.as_ref().is_none_or(|(r, _, _)| round > *r) {
            n.completed = Some((round, base, votes.to_vec()));
        }
    }
}
