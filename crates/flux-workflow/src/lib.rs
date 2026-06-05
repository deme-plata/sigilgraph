//! flux-workflow — editorial approval gates: Draft → Review → Approved → Published (+ Rejected), role-gated.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State { Draft, Review, Approved, Published, Rejected }
#[derive(Serialize, Deserialize)]
pub struct Workflow { pub state: State, pub log: Vec<(String, String)> } // (role, transition)
impl Default for Workflow { fn default() -> Self { Self { state: State::Draft, log: vec![] } } }
impl Workflow {
    pub fn new() -> Self { Self::default() }
    // who may move FROM a state
    fn role_ok(from: State, role: &str) -> bool { match from { State::Draft => role == "author" || role == "editor", State::Review => role == "editor" || role == "admin", State::Approved => role == "editor" || role == "admin", _ => false } }
    fn allowed(from: State, to: State) -> bool { matches!((from, to), (State::Draft, State::Review) | (State::Review, State::Approved) | (State::Review, State::Rejected) | (State::Approved, State::Published) | (State::Rejected, State::Draft) | (State::Published, State::Draft)) }
    pub fn transition(&mut self, to: State, role: &str) -> Result<(), String> {
        if !Self::allowed(self.state, to) { return Err(format!("illegal {:?}->{:?}", self.state, to)); }
        if !Self::role_ok(self.state, role) { return Err(format!("role '{}' may not move from {:?}", role, self.state)); }
        self.log.push((role.into(), format!("{:?}->{:?}", self.state, to))); self.state = to; Ok(())
    }
    pub fn live(&self) -> bool { self.state == State::Published }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn happy_path() { let mut w=Workflow::new(); w.transition(State::Review,"author").unwrap(); w.transition(State::Approved,"editor").unwrap(); w.transition(State::Published,"admin").unwrap(); assert!(w.live()); assert_eq!(w.log.len(),3); }
 #[test] fn illegal_and_role_gated() { let mut w=Workflow::new(); assert!(w.transition(State::Published,"admin").is_err()); assert!(w.transition(State::Review,"random").is_err()); }
}