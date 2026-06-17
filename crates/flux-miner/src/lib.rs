//! # flux-miner — the dual-lane Flux miner
//!
//! Two orthogonal axes, both required for a valid block:
//!   * **Lane A — BLAKE4 (Φ, power):** parallel hashes/sec. Find a nonce whose
//!     BLAKE4 hash is below the difficulty target. Hardware-buyable, scales with
//!     cores — provides throughput + liveness.
//!   * **Lane B — VDF (Ω, time):** `t` sequential squarings (`flux-vdf`). Cannot
//!     be parallelized; one fast core ≈ one vote — provides fair, grind-proof,
//!     ASIC-resistant proof of elapsed time.
//!
//! `power can't fake time, time can't fake power` — an attacker must win both.

use flux_vdf::{eval, verify, VdfGroup, VdfProof};

pub mod client;
/// BLAKE4 — the parameterized-round PoW hash (R=7 ≡ BLAKE3, R<7 = the speed lever).
pub mod pow;
/// CPU/GPU hybrid mining — OpenCL BLAKE4 Lane-A search (ported from the QUG
/// q-miner). Gated: needs the `gpu` feature + an OpenCL runtime (a GPU box).
#[cfg(feature = "gpu")]
pub mod gpu;
/// The light ECONOMIC node (price + arb + DCA) — needs flux-market + flux-fold.
#[cfg(feature = "market")]
pub mod light;
/// The HTTP self-updater — needs reqwest, so it rides the `client` feature.
#[cfg(feature = "client")]
pub mod updater;

// ── BLAKE4: the PoW hash (BLAKE3 core, Flux-parallelized) ───────────────────

/// One BLAKE4 evaluation over `header || nonce`; the first 8 bytes are the
/// target word a miner drives below the difficulty target. BLAKE3 core =
/// preimage-hard, so the difficulty search can't be shortcut.
#[inline]
pub fn blake4(header: &[u8], nonce: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(header);
    h.update(&nonce.to_le_bytes());
    let b = h.finalize();
    u64::from_le_bytes(b.as_bytes()[0..8].try_into().unwrap())
}

/// The VDF seed for a found block: `BLAKE3(header || nonce)` (matches the SIGIL
/// header's `vdf_input = BLAKE3(parent_hash || nonce)` binding).
fn vdf_seed(header: &[u8], nonce: u64) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"flux-miner/vdf-seed/v1");
    h.update(header);
    h.update(&nonce.to_le_bytes());
    *h.finalize().as_bytes()
}

// ── the number-powers: Φ (power) and Ω (time) ───────────────────────────────

/// Format a hashrate in FLUX (Φ): `1 Φ = 1 EH/s`, so `1 nΦ = 1 GH/s`.
pub fn format_flux(hps: f64) -> String {
    let f = hps / 1e18;
    let (v, u) = if f >= 1.0 { (f, "Φ") }
        else if f >= 1e-3 { (f * 1e3, "mΦ") }
        else if f >= 1e-6 { (f * 1e6, "µΦ") }
        else if f >= 1e-9 { (f * 1e9, "nΦ") }
        else { (f * 1e12, "pΦ") };
    format!("{v:.3} {u}")
}

/// Format a VDF rate in OMEGA (Ω): `1 Ω = 1 Mega-turn/s` (sequential squarings).
pub fn format_omega(turns_per_sec: f64) -> String {
    let o = turns_per_sec / 1e6;
    if o >= 1.0 { format!("{o:.3} Ω") } else { format!("{:.1} mΩ", o * 1e3) }
}

// ── the dual-lane block ──────────────────────────────────────────────────────

/// A mined block: the BLAKE4 PoW solution (Lane A) and the VDF proof (Lane B).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DualLaneBlock {
    pub header: Vec<u8>,
    /// Lane A: the winning nonce and its BLAKE4 hash word (`<= target`).
    pub nonce: u64,
    pub blake4_hash: u64,
    /// Lane B: the Wesolowski VDF proof over `BLAKE3(header || nonce)`.
    pub vdf: VdfProof,
}

/// Mine one dual-lane block: search BLAKE4 nonces until one is below `target`
/// (Lane A), then run the VDF for `vdf_t` sequential turns over the found block
/// (Lane B). Single-threaded reference loop; production fans Lane A over cores.
pub fn mine_dual<G: VdfGroup>(header: &[u8], target: u64, vdf_t: u64, g: &G) -> DualLaneBlock {
    // Lane A search: grind 8 nonces per call via the AVX2 8-way kernel (scalar
    // fallback when AVX2 is absent). Each lane is byte-identical to `blake4`, so a
    // hit verifies under the unchanged consensus path. ~3.18x measured vs scalar.
    let nonce = {
        let mut base = 0u64;
        loop {
            let words = pow::blake4_words_x8(header, base);
            if let Some(i) = words.iter().position(|&w| w <= target) {
                break base.wrapping_add(i as u64);
            }
            base = base.wrapping_add(8);
        }
    };
    let blake4_hash = blake4(header, nonce); // exact word the node re-verifies
    let x = g.from_seed(&vdf_seed(header, nonce));
    let vdf = eval(g, &x, vdf_t);
    DualLaneBlock { header: header.to_vec(), nonce, blake4_hash, vdf }
}

/// Assemble a [`DualLaneBlock`] for an ALREADY-FOUND BLAKE4 nonce (e.g. one the
/// GPU Lane-A search returned): recompute the BLAKE4 hash + run the VDF (Lane B)
/// over it. The node's [`verify_dual`] re-checks both lanes with [`blake4`], so a
/// GPU search must use the SAME hash (full-round `blake4` == `pow` R=7) for the
/// share to be accepted. This is the CPU half of the hybrid: GPU finds the nonce,
/// the CPU does the inherently-sequential VDF.
pub fn block_for_nonce<G: VdfGroup>(header: &[u8], nonce: u64, g: &G, vdf_t: u64) -> DualLaneBlock {
    let blake4_hash = blake4(header, nonce);
    let x = g.from_seed(&vdf_seed(header, nonce));
    let vdf = eval(g, &x, vdf_t);
    DualLaneBlock { header: header.to_vec(), nonce, blake4_hash, vdf }
}

/// Verify a dual-lane block: BOTH the BLAKE4 PoW (`<= target`) AND the VDF proof
/// must check out. This is the consensus rule a node enforces.
pub fn verify_dual<G: VdfGroup>(g: &G, block: &DualLaneBlock, target: u64) -> bool {
    // Lane A: re-hash the claimed nonce.
    if blake4(&block.header, block.nonce) > target {
        return false;
    }
    if block.blake4_hash > target {
        return false;
    }
    // Lane B: re-derive the seed and verify the VDF in O(1).
    let x = g.from_seed(&vdf_seed(&block.header, block.nonce));
    verify(g, &x, &block.vdf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_vdf::ModSquaring;

    #[test]
    fn dual_lane_mine_then_verify() {
        let g = ModSquaring::bench_2048();
        let header = b"sigil-g0-block-7";
        // easy target (top ~12 bits zero) so the test finds a nonce fast.
        let target = u64::MAX >> 12;
        let block = mine_dual(header, target, 2_000, &g);

        assert!(block.blake4_hash <= target, "Lane A: hash below target");
        assert!(verify_dual(&g, &block, target), "both lanes must verify");

        // tamper Lane A (nonce) → fails
        let mut bad_a = block.clone();
        bad_a.nonce ^= 1;
        assert!(!verify_dual(&g, &bad_a, target), "tampered nonce must fail");

        // tamper Lane B (vdf) → fails
        let mut bad_b = block;
        bad_b.vdf.y[0] ^= 1;
        assert!(!verify_dual(&g, &bad_b, target), "tampered VDF must fail");
    }

    #[test]
    fn unit_formatting() {
        assert_eq!(format_flux(1e18), "1.000 Φ");
        assert_eq!(format_flux(3e9), "3.000 nΦ"); // 3 GH/s = 3 nanoflux
        assert_eq!(format_omega(1e6), "1.000 Ω"); // 1 Mturn/s = 1 omega
    }
}
