//! store.rs — flux-db-backed durable block store. THE fix for the JSON-snapshot
//! footprint problem that `footprint.rs` measured:
//!
//!   serde_json snapshot (today)        3850 B/block
//!   bincode + grouped-LZ4 (this store)  948 B/block   (4.06x smaller)
//!   witness-pruned core                 344 B/block  (11.20x smaller)
//!
//! Blocks are bincode-encoded (binary — the real lever, ~3.7x over JSON text)
//! and written into a flux-db column family. flux-db's RocksDB-style block-SSTs
//! LZ4-compress ~4 KB groups of values together, so a point `get()` reads +
//! decompresses ONE 4 KB block, not the whole chain — reads stay blazing fast
//! while the on-disk graph shrinks. Keys are `height.to_be_bytes()` so the CF
//! iterates in height order (cheap tip lookup + replay).

use flux_db::Database;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sigil_header::SigilBlockHeaderV0;
use std::path::{Path, PathBuf};

/// How much of each block a node retains on disk.
///
/// **SIGIL ships [`RetentionMode::Full`]** — archival, self-sufficient, zero
/// data-availability dependency, independently re-verifiable (see the footprint
/// analysis: full is the network's memory + auditor; pruning trades that for a
/// dependency on someone else keeping the data). That's the decision.
///
/// The pruned/history machinery still exists, but it's **off by default**. The
/// mode is a single dispatch point read from `SIGIL_RETENTION`, so the choice
/// stays *reversible* — flip it later without touching code. Crypto/storage
/// agility: never hardcode the assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionMode {
    /// Everything: header + sigs + proofs + transition + events. **The default.**
    Full,
    /// Drop the crypto witness, keep transition + events (full tx history, but
    /// trusts past verification; depends on an archive to bootstrap peers).
    History,
    /// Consensus core only: the 4 roots + parent-hash chain + identity. Smallest,
    /// but can only verify-given-a-proof + must fetch history from an archive.
    Pruned,
}

impl Default for RetentionMode {
    fn default() -> Self {
        RetentionMode::Full
    }
}

impl RetentionMode {
    /// Read the policy from `SIGIL_RETENTION` (default: Full). Unknown values
    /// fall back to Full loudly — we never silently prune.
    pub fn from_env() -> Self {
        Self::parse(std::env::var("SIGIL_RETENTION").ok().as_deref())
    }

    /// Pure parser (testable without touching the process environment).
    pub fn parse(v: Option<&str>) -> Self {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("full") | Some("archival") => RetentionMode::Full,
            Some("history") | Some("wallet") => RetentionMode::History,
            Some("pruned") | Some("state") | Some("state-only") => RetentionMode::Pruned,
            Some(other) => {
                eprintln!("⚠ unknown SIGIL_RETENTION={other:?} — defaulting to FULL (SIGIL is full-node-only by design)");
                RetentionMode::Full
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RetentionMode::Full => "full",
            RetentionMode::History => "history",
            RetentionMode::Pruned => "pruned",
        }
    }

    /// True iff this node keeps every block whole (the SIGIL default).
    pub fn is_full(&self) -> bool {
        matches!(self, RetentionMode::Full)
    }
}

/// Witness-pruned header core: everything a node needs to maintain and serve
/// state AFTER it has verified a block, dropping the verify-once witness data
/// — the two 292-byte SQIsign signatures (`nonce_sqisign`, `producer_sig`),
/// the VDF proof, and the STARK proof bytes. Those are needed exactly once, at
/// verification time; an archival node that has already checked the block can
/// store just this core (11x smaller than the full block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedHeader {
    pub version: u16,
    pub network_id: [u8; 8],
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub timestamp_ms: u64,
    pub vdf_input: [u8; 32],
    pub difficulty: u64,
    pub wallet_state_root: [u8; 32],
    pub dex_state_root: [u8; 32],
    pub event_log_root: [u8; 32],
    pub contract_state_root: [u8; 32],
    pub public_inputs_hash: [u8; 32],
    pub txs_merkle_root: [u8; 32],
    pub tx_count: u32,
    pub artifact_blake3: [u8; 32],
    pub producer: [u8; 32],
}

impl From<&SigilBlockHeaderV0> for PrunedHeader {
    fn from(h: &SigilBlockHeaderV0) -> Self {
        PrunedHeader {
            version: h.version,
            network_id: h.network_id,
            height: h.height,
            parent_hash: h.parent_hash,
            timestamp_ms: h.timestamp_ms,
            vdf_input: h.vdf_input,
            difficulty: h.difficulty as u64,
            wallet_state_root: h.wallet_state_root,
            dex_state_root: h.dex_state_root,
            event_log_root: h.event_log_root,
            contract_state_root: h.contract_state_root,
            public_inputs_hash: h.state_transition_proof.public_inputs_hash,
            txs_merkle_root: h.txs_merkle_root,
            tx_count: h.tx_count,
            artifact_blake3: h.fluxc_artifact_proof.artifact_blake3,
            producer: h.producer,
        }
    }
}

/// A height-keyed, bincode-encoded, LZ4-compressed block store backed by a
/// single flux-db column family. Generic over the value type so it serves both
/// `sigil_node`'s block and `sigil_chronos`'s identically-shaped block.
pub struct BlockStore {
    _root: Database,
    blocks: Database,
    path: PathBuf,
    retention: RetentionMode,
}

impl BlockStore {
    /// Open (or create) a store at `path`. Idempotent — re-opening loads the
    /// existing chain. Retention policy comes from `SIGIL_RETENTION` (default
    /// Full); override with [`BlockStore::with_retention`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let root = Database::open(&path)?;
        let blocks = match root.cf("blocks") {
            Some(cf) => cf,
            None => root.create_cf("blocks")?,
        };
        Ok(Self { _root: root, blocks, path, retention: RetentionMode::from_env() })
    }

    /// Override the retention policy (otherwise read from `SIGIL_RETENTION`).
    pub fn with_retention(mut self, mode: RetentionMode) -> Self {
        self.retention = mode;
        self
    }

    /// The active retention policy. Callers dispatch their persist path on this:
    /// `Full` → [`put_block`], `History` → store a history view, `Pruned` →
    /// [`put_pruned`]. SIGIL nodes run `Full`.
    pub fn retention(&self) -> RetentionMode {
        self.retention
    }

    #[inline]
    fn key(height: u64) -> [u8; 8] {
        height.to_be_bytes()
    }

    /// Persist a block at `height`. MessagePack-encode → flux-db (LZ4 in the SST).
    /// MessagePack (not bincode) because SIGIL's internally-tagged enums
    /// (`SigilEvent`/`SigilTx` `tag="kind"`) need a self-describing codec to
    /// round-trip — bincode serializes them but cannot decode (`deserialize_any`).
    pub fn put_block<T: Serialize>(&self, height: u64, block: &T) -> Result<(), String> {
        let bytes = rmp_serde::to_vec(block).map_err(|e| e.to_string())?;
        self.blocks.put(&Self::key(height), &bytes)
    }

    /// Persist the witness-pruned core of a block at `height`.
    pub fn put_pruned(&self, height: u64, pruned: &PrunedHeader) -> Result<(), String> {
        self.put_block(height, pruned)
    }

    /// Load + decode the block at `height`, if present.
    pub fn get_block<T: DeserializeOwned>(&self, height: u64) -> Result<Option<T>, String> {
        match self.blocks.get(&Self::key(height))? {
            Some(b) => rmp_serde::from_slice(&b).map(Some).map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    /// Highest stored height (keys are big-endian, so the max key = the tip).
    pub fn tip_height(&self) -> Option<u64> {
        self.blocks
            .scan()
            .into_iter()
            .filter_map(|(k, _)| <[u8; 8]>::try_from(k.as_slice()).ok().map(u64::from_be_bytes))
            .max()
    }

    /// Number of stored blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Replay-load every stored block in height order (boot path).
    pub fn load_all<T: DeserializeOwned>(&self) -> Result<Vec<T>, String> {
        let mut rows = self.blocks.scan();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.into_iter()
            .map(|(_, v)| rmp_serde::from_slice(&v).map_err(|e| e.to_string()))
            .collect()
    }

    /// Force memtable → SST compaction so an on-disk measurement is honest
    /// (otherwise recently-`put` values may still be buffered in RAM).
    pub fn compact(&self) {
        let _ = self._root.compact_async().join();
    }

    /// Actual bytes on disk under the store path — the honest footprint,
    /// including LSM overhead (SST metadata, bloom filters, WAL).
    pub fn disk_bytes(&self) -> u64 {
        dir_size(&self.path)
    }
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal serializable stand-in so the store test doesn't depend on the
    // full block type (which lives in the bin crate).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct FakeBlock {
        height: u64,
        roots: [[u8; 32]; 4],
        sig: Vec<u8>,
    }

    fn fake(height: u64) -> FakeBlock {
        let mut sig = vec![0u8; 292];
        sig.iter_mut().enumerate().for_each(|(i, b)| *b = (height as u8).wrapping_add(i as u8));
        FakeBlock { height, roots: [[height as u8; 32]; 4], sig }
    }

    #[test]
    fn retention_defaults_to_full_and_parses() {
        // The decision: SIGIL is full-node-only by default.
        assert_eq!(RetentionMode::default(), RetentionMode::Full);
        assert_eq!(RetentionMode::parse(None), RetentionMode::Full);
        assert_eq!(RetentionMode::parse(Some("")), RetentionMode::Full);
        assert_eq!(RetentionMode::parse(Some(" Full ")), RetentionMode::Full);
        // but it stays reversible via the env knob
        assert_eq!(RetentionMode::parse(Some("history")), RetentionMode::History);
        assert_eq!(RetentionMode::parse(Some("PRUNED")), RetentionMode::Pruned);
        assert_eq!(RetentionMode::parse(Some("state-only")), RetentionMode::Pruned);
        // unknown never silently prunes
        assert_eq!(RetentionMode::parse(Some("garbage")), RetentionMode::Full);
        assert!(RetentionMode::default().is_full());
    }

    #[test]
    fn store_default_retention_is_full() {
        let dir = std::env::temp_dir().join(format!("sigil-store-ret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // ensure no env override leaks in from the runner
        std::env::remove_var("SIGIL_RETENTION");
        let store = BlockStore::open(&dir).unwrap();
        assert_eq!(store.retention(), RetentionMode::Full);
        let store = store.with_retention(RetentionMode::History);
        assert_eq!(store.retention(), RetentionMode::History);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn put_get_roundtrip_and_tip() {
        let dir = std::env::temp_dir().join(format!("sigil-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = BlockStore::open(&dir).unwrap();

        for h in 0..50u64 {
            store.put_block(h, &fake(h)).unwrap();
        }
        assert_eq!(store.block_count(), 50);
        assert_eq!(store.tip_height(), Some(49));

        // exact round-trip
        let got: FakeBlock = store.get_block(7).unwrap().unwrap();
        assert_eq!(got, fake(7));

        // replay-load is height-ordered and complete
        let all: Vec<FakeBlock> = store.load_all().unwrap();
        assert_eq!(all.len(), 50);
        for (i, b) in all.iter().enumerate() {
            assert_eq!(b.height, i as u64);
        }

        // re-open loads the existing chain
        drop(store);
        let store2 = BlockStore::open(&dir).unwrap();
        assert_eq!(store2.tip_height(), Some(49));

        store2.compact();
        assert!(store2.disk_bytes() > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
