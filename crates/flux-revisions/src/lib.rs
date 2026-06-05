//! flux-revisions — content versioning + rollback with blake3 integrity (every edit is a revision).
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Revision { pub n: u32, pub hash: String, pub content: String }
#[derive(Default, Serialize, Deserialize)]
pub struct History { pub revisions: Vec<Revision> }
impl History {
    pub fn new() -> Self { Self::default() }
    pub fn commit(&mut self, content: &str) -> u32 { let n = self.revisions.len() as u32 + 1; self.revisions.push(Revision { n, hash: blake3::hash(content.as_bytes()).to_hex().to_string(), content: content.into() }); n }
    pub fn get(&self, n: u32) -> Option<&Revision> { self.revisions.iter().find(|r| r.n == n) }
    pub fn current(&self) -> Option<&Revision> { self.revisions.last() }
    pub fn verify(&self, n: u32) -> bool { self.get(n).map(|r| blake3::hash(r.content.as_bytes()).to_hex().to_string() == r.hash).unwrap_or(false) }
    /// Roll back to revision n by appending its content as a new revision (history is append-only).
    pub fn rollback(&mut self, n: u32) -> Result<u32, String> { let c = self.get(n).ok_or("no such revision")?.content.clone(); Ok(self.commit(&c)) }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn versions_and_rollback() { let mut h=History::new(); h.commit("v1"); h.commit("v2"); h.commit("v3"); assert_eq!(h.current().unwrap().content,"v3"); let r=h.rollback(1).unwrap(); assert_eq!(h.current().unwrap().content,"v1"); assert_eq!(r,4); assert!(h.verify(2)); }
 #[test] fn integrity() { let mut h=History::new(); h.commit("x"); assert!(h.verify(1)); h.revisions[0].content="tampered".into(); assert!(!h.verify(1)); }
}