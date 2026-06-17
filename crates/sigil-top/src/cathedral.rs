//! Cathedral — vaulted DagKnight view for sigil-top.
//!
//! "Cathedral" codename for the layered, 4-root-committed, provenance-attested
//! DagKnight realization. Each vault is a certified DagKnight segment that
//! commits the full Quillon-fix (4 state roots) + STARK + fluxc proof +
//! deterministic linearization (divergence = 0).
//!
//! Dev note: builds for this module are monitored via MCP webhook
//! "grok-cathedral-sigiltop" (registered to fluxc :8084/api/build_event).
//!
//! This is the first wiring slice: types + basic ingest + certify using the
//! same primitives already present in sigil-top (StateRoots, TipProof, header
//! data). Real flux-narwhal-core / flux-consensus linearizer can be dropped
//! in the `run_dagknight_linearize` slot later without changing the surface.

use sigil_header::SigilBlockHeaderV0;
use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;

use std::collections::HashMap;

/// A single certified DagKnight "vault" (a finalized segment of the DAG).
#[derive(Clone, Debug, serde::Serialize)]
pub struct CathedralVault {
    pub start_height: u64,
    pub end_height: u64,
    pub roots: StateRoots,
    pub tip_proof: Option<TipProof>, // 10ms verify artifact
    pub braid_hash: [u8; 32],        // content of the DagKnight braid for this vault
    pub producers: Vec<[u8; 32]>,    // validator ids active in the segment
    pub flux_proof_present: bool,    // fluxc_artifact_proof was non-empty
    pub certified: bool,             // DagKnight + roots + proof passed
    pub divergence: u64,             // must be 0 for healthy Cathedral
}

/// Live Cathedral view (the "top" sees one of these).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Cathedral {
    pub vaults: Vec<CathedralVault>,
    pub current_open_start: Option<u64>,
    pub last_certified_height: u64,
    pub total_vaults: usize,
    /// "Divergence: 0" is the DagKnight invariant we surface everywhere.
    pub last_divergence: u64,
}

impl Cathedral {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a header that already passed basic parent/spine checks.
    /// This is the primary wire point from block_sync / chain_verify.
    pub fn ingest_header(&mut self, h: &SigilBlockHeaderV0) {
        // For the first slice we treat each block as its own micro-vault until
        // we have wave/braid aggregation from real narwhal-core.  The structure
        // is ready for the upgrade.
        let roots = StateRoots {
            wallet_state_root: h.wallet_state_root,
            dex_state_root: h.dex_state_root,
            event_log_root: h.event_log_root,
            contract_state_root: h.contract_state_root,
        };

        let braid = Self::compute_braid_hash(h);

        let flux_present = !h.fluxc_artifact_proof.artifact_blake3.iter().all(|&b| b == 0)
            || !h.fluxc_artifact_proof.sqisign_sig.is_empty();

        let producer = h.producer;

        let vault = CathedralVault {
            start_height: h.height,
            end_height: h.height,
            roots,
            tip_proof: None, // filled by certify step
            braid_hash: braid,
            producers: vec![producer],
            flux_proof_present: flux_present,
            certified: false,
            divergence: 0,
        };

        self.vaults.push(vault);
        self.current_open_start = Some(h.height);
        if self.vaults.len() > 64 {
            // keep a sliding window for the TUI
            self.vaults.remove(0);
        }
    }

    /// Close and certify the current open wave/vault.
    /// In a fuller impl this would call the real DagKnight orderer over the
    /// collected headers and obtain a collective tip proof.
    pub fn close_and_certify(&mut self, tip: &TipProof) -> bool {
        if let Some(v) = self.vaults.last_mut() {
            v.tip_proof = Some(tip.clone());
            // For now: certify if we have non-zero roots on at least one and a proof object.
            // Real version will also run linearize and check divergence == 0.
            let has_roots = v.roots.wallet_state_root.iter().any(|&b| b != 0)
                || v.roots.dex_state_root.iter().any(|&b| b != 0);
            v.certified = has_roots && v.tip_proof.is_some();
            v.divergence = 0; // DagKnight promise
            self.last_certified_height = v.end_height;
            self.last_divergence = 0;
            self.total_vaults = self.vaults.len();
            v.certified
        } else {
            false
        }
    }

    /// Stub for the real DagKnight linearizer (Narwhal waves → deterministic order).
    /// Later: feed a slice of headers into flux-narwhal-core + flux-consensus,
    /// get back a total order + braid commitment, assert two independent runs
    /// produce identical order (divergence = 0).
    fn compute_braid_hash(h: &SigilBlockHeaderV0) -> [u8; 32] {
        // Cheap stand-in: BLAKE3 over (height || parent || first root)
        let mut hasher = blake3::Hasher::new();
        hasher.update(&h.height.to_le_bytes());
        hasher.update(&h.parent_hash);
        hasher.update(&h.wallet_state_root);
        *hasher.finalize().as_bytes()
    }

    /// Quick health summary for heroes / tab.
    pub fn health_summary(&self) -> String {
        if self.vaults.is_empty() {
            return "Cathedral: empty (no blocks ingested)".into();
        }
        let last = self.vaults.last().unwrap();
        format!(
            "Cathedral: {} vaults | last={}..={} | div={} | certified={} | flux_proof={}",
            self.total_vaults,
            last.start_height,
            last.end_height,
            last.divergence,
            last.certified,
            last.flux_proof_present
        )
    }
}

/// Small helper so existing StateRoots (from tip feeds) can be turned into the
/// Cathedral view without a full header.
pub fn vault_from_roots(height: u64, roots: &StateRoots, has_flux_proof: bool) -> CathedralVault {
    CathedralVault {
        start_height: height,
        end_height: height,
        roots: roots.clone(),
        tip_proof: None,
        braid_hash: [0u8; 32],
        producers: vec![],
        flux_proof_present: has_flux_proof,
        certified: false,
        divergence: 0,
    }
}
