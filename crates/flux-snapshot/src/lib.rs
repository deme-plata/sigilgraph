//! flux-snapshot — snapshot + restore with integrity hash (Backup & Migrate).
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
pub struct Snapshot { pub hash: String, pub len: usize, pub blob: Vec<u8> }
impl Snapshot {
    pub fn take(blob: &[u8]) -> Self { Self { hash: blake3::hash(blob).to_hex().to_string(), len: blob.len(), blob: blob.to_vec() } }
    pub fn verify(&self) -> bool { blake3::hash(&self.blob).to_hex().to_string() == self.hash && self.blob.len() == self.len }
    pub fn restore(&self) -> Option<&[u8]> { if self.verify() { Some(&self.blob) } else { None } }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn roundtrip() { let s = Snapshot::take(b"site state"); assert!(s.verify()); assert_eq!(s.restore(), Some(&b"site state"[..])); }
 #[test] fn tamper_detected() { let mut s = Snapshot::take(b"x"); s.blob[0] ^= 0xff; assert!(!s.verify()); assert!(s.restore().is_none()); }
}