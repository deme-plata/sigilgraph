//! flux-scheduler — scheduled publish/unpublish across time zones (everything in UTC unix seconds).
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Job { pub id: u64, pub at: u64, pub action: String, pub target: String, pub done: bool }
#[derive(Default, Serialize, Deserialize)]
pub struct Scheduler { pub jobs: Vec<Job>, next: u64 }
impl Scheduler {
    pub fn new() -> Self { Self::default() }
    pub fn schedule(&mut self, at_unix: u64, action: &str, target: &str) -> u64 { self.next += 1; let id = self.next; self.jobs.push(Job { id, at: at_unix, action: action.into(), target: target.into(), done: false }); id }
    /// Jobs due at `now` (UTC unix), not yet run, soonest first.
    pub fn due(&self, now: u64) -> Vec<&Job> { let mut d: Vec<&Job> = self.jobs.iter().filter(|j| !j.done && j.at <= now).collect(); d.sort_by_key(|j| j.at); d }
    pub fn mark_done(&mut self, id: u64) { if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) { j.done = true; } }
    pub fn pending(&self) -> usize { self.jobs.iter().filter(|j| !j.done).count() }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn due_filtering() { let mut s=Scheduler::new(); s.schedule(100,"publish","post-1"); let later=s.schedule(300,"unpublish","post-1"); assert_eq!(s.due(150).len(),1); assert_eq!(s.due(150)[0].action,"publish"); assert_eq!(s.due(400).len(),2); s.mark_done(1); assert_eq!(s.due(400).len(),1); assert_eq!(s.pending(),1); let _=later; }
}