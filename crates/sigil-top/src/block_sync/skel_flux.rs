//! block_sync/skel_flux.rs — adopt the NATIVE flux-db skeleton extension.
//!
//! Viktor generalized the flat 72-B skeleton store into flux-db itself
//! (`flux_db::skeleton::SkeletonStore<R: flux_db::skeleton::SkeletonRecord>`), with
//! torn-tail recovery + an `archive_root()` BLAKE3 commitment for light-client anchor
//! checks. SIGIL should use THAT, not the local `block_sync::skeleton_store` duplicate.
//!
//! Orphan rule: flux-db owns the trait, `sigil-header` owns `SkeletonRecord`, and
//! neither deps the other — so the impl lives here, on a sigil-top-owned newtype that
//! wraps SIGIL's record and serializes it in the canonical
//! `height:u64(LE) ‖ block_hash:[u8;32] ‖ parent_hash:[u8;32]` 72-B layout.

use sigil_header::SkeletonRecord as SigilSkel;

/// sigil-top-owned adapter so SIGIL's 72-B record drives the flux-db skeleton store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkelRec(pub SigilSkel);

impl flux_db::skeleton::SkeletonRecord for SkelRec {
    /// height(8) + block_hash(32) + parent_hash(32) — the cross-crate 72-B invariant.
    const WIDTH: usize = 72;

    #[inline]
    fn seq(&self) -> u64 {
        self.0.height
    }

    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.0.height.to_le_bytes());
        out[8..40].copy_from_slice(&self.0.block_hash);
        out[40..72].copy_from_slice(&self.0.parent_hash);
    }

    fn decode(buf: &[u8]) -> Result<Self, String> {
        if buf.len() != Self::WIDTH {
            return Err(format!("SkelRec::decode: need {}B, got {}", Self::WIDTH, buf.len()));
        }
        let mut h = [0u8; 8];
        h.copy_from_slice(&buf[0..8]);
        let mut block_hash = [0u8; 32];
        block_hash.copy_from_slice(&buf[8..40]);
        let mut parent_hash = [0u8; 32];
        parent_hash.copy_from_slice(&buf[40..72]);
        Ok(SkelRec(SigilSkel {
            height: u64::from_le_bytes(h),
            block_hash,
            parent_hash,
        }))
    }
}

/// SIGIL's skeleton store, backed by the NATIVE flux-db extension. Drop-in for the
/// local `block_sync::skeleton_store::SkeletonStore` (same open/append/read_at/count
/// surface), plus `archive_root()` for the anchor commitment.
#[allow(dead_code)]
pub(crate) type FluxSkeletonStore = flux_db::skeleton::SkeletonStore<SkelRec>;

#[cfg(test)]
mod tests {
    use super::*;
    use flux_db::skeleton::SkeletonRecord as _;

    fn rec(h: u64) -> SkelRec {
        SkelRec(SigilSkel { height: h, block_hash: [h as u8; 32], parent_hash: [(h.wrapping_sub(1)) as u8; 32] })
    }

    #[test]
    fn sigil_record_roundtrips_through_the_flux_db_skeleton_trait() {
        let r = rec(42);
        let mut buf = [0u8; 72];
        r.encode(&mut buf);
        assert_eq!(SkelRec::WIDTH, 72);
        assert_eq!(r.seq(), 42);
        let back = SkelRec::decode(&buf).unwrap();
        assert_eq!(back, r, "72-B encode/decode is byte-exact + round-trips");
    }

    #[test]
    fn drives_the_native_flux_db_store() {
        let p = std::env::temp_dir().join(format!("sigil-skelflux-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s: FluxSkeletonStore = flux_db::skeleton::SkeletonStore::open(&p, 1).unwrap();
        let run: Vec<SkelRec> = (1..=1000).map(rec).collect();
        assert_eq!(s.append(&run).unwrap(), 1000);
        assert_eq!(s.tip_seq(), Some(1000));
        assert_eq!(s.count(), 1000);
        assert_eq!(s.read_at(500).unwrap().unwrap(), rec(500));
        // archive_root is the light-client commitment over the stored prefix.
        let root = s.archive_root().unwrap();
        assert_ne!(root, [0u8; 32], "archive_root produces a real BLAKE3 commitment");
        let _ = std::fs::remove_file(&p);
    }
}
