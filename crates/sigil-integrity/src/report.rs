//! The status everyone reads: per node, what it last said and how it has behaved; per fleet, the last
//! height at which all voices agree, and the alarms. Written as JSON every few seconds and published.
use crate::ledger::{Ledger, Verdict};
use crate::node::now_ms;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct NodeStatus {
    pub url: String,
    pub applied_height: Option<u64>,
    pub finalized_height: Option<u64>,
    pub last_cert_height: Option<u64>,
    pub last_block_height: Option<u64>,
    pub last_sample_ms: u64,
    pub last_event_ms: u64,
    pub lag_blocks: Option<i64>,
    pub agreements: u64,
    pub disagreements: u64,
    pub last_agree_height: Option<u64>,
    pub last_disagree: Option<Disagreement>,
    pub errors: u64,
    pub last_error: Option<String>,
    pub transport: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Disagreement {
    pub height: u64,
    pub what: String,
    pub against: Vec<String>,
    pub ts_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Alarm {
    pub kind: String,
    pub node: String,
    pub detail: String,
    pub ts_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub ts_ms: u64,
    pub started_ms: u64,
    pub nodes: BTreeMap<String, NodeStatus>,
    /// highest height where every recorded voice agrees, and how many voices spoke there
    pub last_common_agreement: Option<(u64, usize)>,
    pub heights_tracked: usize,
    pub verdict: String,
    pub alarms: Vec<Alarm>,
    /// Per node, the newest 512 `(height, block_hash_hex)` this monitor saw from it — the
    /// spine hash of its finality certificate for committee nodes, its finalized block hash
    /// otherwise. A client that holds the chain compares its OWN hash at any of these heights:
    /// equal hash ⇒ equal header ⇒ equal committed roots, exact at that height, no race.
    pub spine_hashes: BTreeMap<String, Vec<(u64, String)>>,
    pub caveat: &'static str,
}

pub const CAVEAT: &str = "Verdicts are drawn only at EQUAL height and only from BLOCK HASHES (exact; a hash commits to the header's state roots). Live roots from /v1/integrity are read racily against the height and are reported as ROOTS-RACE, never as a fork. A hash mismatch that persists across ≥3 distinct heights is a fork. `lag_blocks` is that node's applied height against the highest applied height seen; a follower syncing from genesis lags by design.";

pub struct Tracker {
    pub nodes: BTreeMap<String, NodeStatus>,
    pub alarms: Vec<Alarm>,
    pub started_ms: u64,
    /// distinct heights at which `node` disagreed, most recent first — the fork rule counts these
    disagree_heights: BTreeMap<String, Vec<u64>>,
}

impl Tracker {
    pub fn new(specs: &[crate::node::NodeSpec]) -> Self {
        let mut nodes = BTreeMap::new();
        for s in specs {
            nodes.insert(s.name.clone(), NodeStatus { url: s.url.clone(), transport: "poll".into(), ..Default::default() });
        }
        Self { nodes, alarms: Vec::new(), started_ms: now_ms(), disagree_heights: BTreeMap::new() }
    }

    pub fn note_verdict(&mut self, node: &str, height: u64, what: &str, v: &Verdict) {
        let mut pending: Option<(String, String)> = None;
        {
            let st = self.nodes.entry(node.to_string()).or_default();
            match v {
                Verdict::First => {}
                Verdict::Agree { .. } => {
                    st.agreements += 1;
                    st.last_agree_height = Some(height.max(st.last_agree_height.unwrap_or(0)));
                }
                Verdict::Disagree { against } => {
                    st.disagreements += 1;
                    st.last_disagree = Some(Disagreement { height, what: what.into(), against: against.clone(), ts_ms: now_ms() });
                    // 2026-09-17: only a BLOCK-HASH disagreement can call a fork. `/v1/integrity`
                    // reads its height and its live roots separately, so at 8–110 blk/s the pair
                    // is routinely a block apart and "roots differ" fired FORK on all three nodes
                    // while their finalized spines agreed at 180/180 common heights. The hash is
                    // exact at its height and commits to the roots; roots stay an inconclusive
                    // side channel (kind ROOTS-RACE) that never counts toward the fork rule.
                    if what == "roots" {
                        pending = Some(("ROOTS-RACE".to_string(), format!("live roots at height {height} differ from {} — inconclusive (height/roots read racily); the hash rule decides", against.join(","))));
                    } else {
                        let hs = self.disagree_heights.entry(node.to_string()).or_default();
                        if !hs.contains(&height) {
                            hs.insert(0, height);
                            hs.truncate(16);
                        }
                        pending = Some(if hs.len() >= 3 {
                            ("FORK".to_string(), format!("{what} hash differs from {} at {} distinct heights (latest {height})", against.join(","), hs.len()))
                        } else {
                            ("MISMATCH".to_string(), format!("{what} hash at height {height} differs from {} — inconclusive until it repeats", against.join(",")))
                        });
                    }
                }
            }
        }
        if let Some((kind, detail)) = pending {
            self.alarm(&kind, node, &detail);
        }
    }

    pub fn alarm(&mut self, kind: &str, node: &str, detail: &str) {
        // one alarm per (kind,node) per minute
        let now = now_ms();
        if self.alarms.iter().rev().take(50).any(|a| a.kind == kind && a.node == node && now - a.ts_ms < 60_000) {
            return;
        }
        eprintln!("🚨 {kind} {node}: {detail}");
        self.alarms.push(Alarm { kind: kind.into(), node: node.into(), detail: detail.into(), ts_ms: now });
        if self.alarms.len() > 200 {
            self.alarms.remove(0);
        }
    }

    /// Silence and lag alarms, computed from what each node last said.
    pub fn sweep(&mut self, silent_after_ms: u64, lag_alarm_blocks: i64) {
        let now = now_ms();
        let top = self.nodes.values().filter_map(|n| n.applied_height).max().unwrap_or(0);
        let names: Vec<String> = self.nodes.keys().cloned().collect();
        for name in names {
            let (lag, silent) = {
                let st = self.nodes.get_mut(&name).unwrap();
                st.lag_blocks = st.applied_height.map(|h| top as i64 - h as i64);
                let last = st.last_sample_ms.max(st.last_event_ms);
                (st.lag_blocks, last > 0 && now - last > silent_after_ms)
            };
            if silent {
                self.alarm("SILENT", &name, &format!("no sample or event for more than {} s", silent_after_ms / 1000));
            }
            if let Some(l) = lag {
                if l > lag_alarm_blocks {
                    self.alarm("LAGGING", &name, &format!("{l} blocks behind the highest applied height"));
                }
            }
        }
    }

    pub fn report(&self, ledger: &Ledger) -> Report {
        let voices = self.nodes.len();
        let lca = ledger.last_common_agreement(voices.min(2));
        let recent_fork = self.alarms.iter().rev().take(20).any(|a| a.kind == "FORK" && now_ms() - a.ts_ms < 600_000);
        let verdict = if recent_fork {
            "FORK — at least one node holds a different chain at the same height".to_string()
        } else if let Some((h, v)) = lca {
            format!("{v} voices agree at height {h}")
        } else {
            "no equal-height comparison yet".to_string()
        };
        let mut spine_hashes: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        for name in self.nodes.keys() {
            let v: Vec<(u64, String)> = ledger.rows.iter().rev()
                .filter_map(|(h, row)| row.spine.get(name).or_else(|| row.block.get(name)).map(|x| (*h, x.clone())))
                .take(512)
                .collect();
            spine_hashes.insert(name.clone(), v);
        }
        Report { ts_ms: now_ms(), started_ms: self.started_ms, nodes: self.nodes.clone(), last_common_agreement: lca, heights_tracked: ledger.rows.len(), verdict, alarms: self.alarms.iter().rev().take(30).cloned().collect(), spine_hashes, caveat: CAVEAT }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeSpec;

    fn tr() -> Tracker {
        Tracker::new(&[NodeSpec { name: "a".into(), url: "x".into() }, NodeSpec { name: "b".into(), url: "y".into() }])
    }

    #[test]
    fn three_distinct_disagreeing_heights_make_a_fork() {
        let mut t = tr();
        for h in [10u64, 11, 12] {
            t.note_verdict("b", h, "spine", &Verdict::Disagree { against: vec!["a".into()] });
        }
        assert!(t.alarms.iter().any(|a| a.kind == "FORK"));
        // the same height repeated does not count twice
        let mut t2 = tr();
        for _ in 0..5 {
            t2.note_verdict("b", 10, "spine", &Verdict::Disagree { against: vec!["a".into()] });
        }
        assert!(!t2.alarms.iter().any(|a| a.kind == "FORK"));
        assert!(t2.alarms.iter().any(|a| a.kind == "MISMATCH"));
    }

    #[test]
    fn lag_and_silence() {
        let mut t = tr();
        t.nodes.get_mut("a").unwrap().applied_height = Some(1000);
        t.nodes.get_mut("b").unwrap().applied_height = Some(100);
        t.nodes.get_mut("b").unwrap().last_sample_ms = now_ms() - 120_000;
        t.sweep(60_000, 500);
        assert_eq!(t.nodes["b"].lag_blocks, Some(900));
        assert!(t.alarms.iter().any(|a| a.kind == "LAGGING" && a.node == "b"));
        assert!(t.alarms.iter().any(|a| a.kind == "SILENT" && a.node == "b"));
        assert!(!t.alarms.iter().any(|a| a.node == "a"));
    }
}
