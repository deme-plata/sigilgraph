//! `CowVec<T>` — a `Vec<T>` behind an `Arc`, cloned by reference, copied on write.
//!
//! # Why (2026-09-12, rocky-bps100-0912)
//!
//! The producer clones the whole `SigilState` three times per block (the
//! speculative frontier, the mint's working copy, the money-API publish), and a
//! `SigilState` carries the shielded pool's `note_ciphertexts: Vec<Option<String>>`
//! — one heap string per note, 3,288 notes live today, 32,768 at pool capacity
//! (~20 MB per sealed epoch, kept in `archive`). MEASURED on Epsilon at 47 blk/s
//! (perf, produce thread): `drop_in_place<ShieldedPool>` 27% + `SigilState::clone`
//! 22% + malloc/memcpy — i.e. ~10 ms of every ~20 ms tick was allocating and
//! freeing copies of ciphertexts that no clone ever modifies.
//!
//! A `CowVec` clone is one atomic increment. Writes go through
//! [`CowVec::push`] / [`CowVec::take`] / [`CowVec::make_mut`], which copy the
//! vector only when another clone is still alive (`Arc::make_mut`) — that
//! happens once per block that actually appends a note, on the copy being
//! written, and never on a read.
//!
//! # Wire / snapshot compatibility
//!
//! `Serialize` and `Deserialize` delegate to the inner `Vec<T>`, so every
//! encoding (rmp_serde snapshots, serde_json, bincode) is BYTE-IDENTICAL to the
//! plain `Vec<T>` it replaces. `PartialEq`/`Eq`/`Debug`/`Default` delegate too.
//! Nothing that hashes, signs or commits to state can observe the change.

use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Reference-counted, copy-on-write vector. See the module doc.
#[derive(Clone)]
pub struct CowVec<T>(Arc<Vec<T>>);

impl<T> CowVec<T> {
    pub fn new() -> Self {
        Self(Arc::new(Vec::new()))
    }
    pub fn from_vec(v: Vec<T>) -> Self {
        Self(Arc::new(v))
    }
    /// True if no other clone shares this vector (a write here copies nothing).
    pub fn is_unique(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}

impl<T: Clone> CowVec<T> {
    /// Mutable access; copies the vector first if another clone is alive.
    pub fn make_mut(&mut self) -> &mut Vec<T> {
        Arc::make_mut(&mut self.0)
    }
    pub fn push(&mut self, v: T) {
        self.make_mut().push(v);
    }
    /// Move the contents out, leaving this empty (the `std::mem::take` shape).
    pub fn take(&mut self) -> Vec<T> {
        std::mem::take(self.make_mut())
    }
}

impl<T> Default for CowVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for CowVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<'a, T> IntoIterator for &'a CowVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T> From<Vec<T>> for CowVec<T> {
    fn from(v: Vec<T>) -> Self {
        Self::from_vec(v)
    }
}

impl<T: PartialEq> PartialEq for CowVec<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}
impl<T: Eq> Eq for CowVec<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for CowVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize> Serialize for CowVec<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Delegate to Vec<T> so the encoding is exactly what a Vec<T> field produced.
        (**self).serialize(s)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for CowVec<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::<T>::deserialize(d).map(Self::from_vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_is_shared_until_written_then_diverges() {
        let mut a: CowVec<String> = CowVec::from_vec(vec!["x".into()]);
        let b = a.clone();
        assert!(!a.is_unique() && !b.is_unique(), "a clone shares the buffer");
        a.push("y".into());
        assert_eq!(a.len(), 2, "writer sees its write");
        assert_eq!(b.len(), 1, "the other clone is untouched");
        assert!(a.is_unique() && b.is_unique(), "the write split them");
    }

    #[test]
    fn take_empties_only_this_handle() {
        let mut a: CowVec<u8> = vec![1, 2, 3].into();
        let b = a.clone();
        let got = a.take();
        assert_eq!(got, vec![1, 2, 3]);
        assert!(a.is_empty());
        assert_eq!(*b, vec![1, 2, 3]);
    }

    #[test]
    fn serde_is_byte_identical_to_plain_vec() {
        let v: Vec<Option<String>> = vec![None, Some("ct".into()), None];
        let c: CowVec<Option<String>> = v.clone().into();
        assert_eq!(serde_json::to_vec(&v).unwrap(), serde_json::to_vec(&c).unwrap());
        assert_eq!(rmp_serde::to_vec(&v).unwrap(), rmp_serde::to_vec(&c).unwrap());
        let back: CowVec<Option<String>> = rmp_serde::from_slice(&rmp_serde::to_vec(&v).unwrap()).unwrap();
        assert_eq!(back, c);
        assert_eq!(*back, v);
    }
}
