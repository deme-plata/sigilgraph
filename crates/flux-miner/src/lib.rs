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
/// Automated ARX differential trail search (Lipmaa-Moriai xdp+ + Matsui-style bound) — the
/// rigorous gate behind pow's sampled differential screen. Research tooling; bench/test only.
#[cfg(any(test, feature = "bench"))]
pub mod diff_search;
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

/// The mining ENGINE orchestration (MinerStats + supervisor + CPU/GPU workers),
/// shared by the standalone  binary AND sigil-top in-node Mining
/// tab so both run byte-identical mining code. Needs the HTTP client.
#[cfg(feature = "client")]
pub mod engine;

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

/// POOL-SHARES: a bounded, RESUMABLE Lane-A search. Grinds at most `budget`
/// nonces starting at `base`; on a hit returns the assembled block plus the next
/// base to resume from, on a miss returns the advanced base. Pool mode needs
/// this because one height yields MANY shares: each found share is submitted and
/// the search continues from where it stopped (restarting at 0 would re-find the
/// same nonce forever), and the budget keeps the loop responsive to tip changes.
pub fn mine_dual_from<G: VdfGroup>(
    header: &[u8],
    target: u64,
    vdf_t: u64,
    g: &G,
    base: u64,
    budget: u64,
) -> (Option<DualLaneBlock>, u64) {
    let mut b = base;
    while b.wrapping_sub(base) < budget {
        let words = pow::blake4_words_x8(header, b);
        if let Some(i) = words.iter().position(|&w| w <= target) {
            let nonce = b.wrapping_add(i as u64);
            return (Some(block_for_nonce(header, nonce, g, vdf_t)), nonce.wrapping_add(1));
        }
        b = b.wrapping_add(8);
    }
    (None, b)
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

/// Is the STRICT Lane-A binding on? See [`verify_dual`].
///
/// Staged behind `SIGIL_STRICT_LANE_A=1` (read once) so the LIVE pool's accept
/// behaviour is byte-identical until the operator/T1 opens the gate. Read once
/// into a `LazyLock` because `verify_dual` is on the per-submission hot path and
/// a `var()` syscall per share would be a measurable tax.
static STRICT_LANE_A: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    std::env::var("SIGIL_STRICT_LANE_A").map(|v| v == "1").unwrap_or(false)
});

/// Verify a dual-lane block: BOTH the BLAKE4 PoW (`<= target`) AND the VDF proof
/// must check out. This is the consensus rule a node enforces.
///
/// ## Lane-A forgery (the gap this closes)
///
/// Historically this bounded the *recomputed* hash by the target and, separately,
/// bounded the *claimed* `blake4_hash` by the target — but never tied the two
/// together. So a genuinely-mined nonce paired with ANY under-target word (`0` is
/// the trivial choice) passed the gate, and that **forged word is what
/// `fold_tip` writes permanently into the header** as the block's proof-of-work.
/// The claimed word is consensus-visible state, so a forged one corrupts the tip
/// chain even though the nonce behind it was honest work.
///
/// The fix is the equality below. It was applied on the braid path at the caller
/// (`sigil-api::mining::lane_a_word_is_honest`, commit 44ae98e) but NOT in this
/// shared gate, which is what `sigil-rpcd` (the live pool) and `chain_verify`
/// both route through.
///
/// **Honest rigs are unaffected by construction:** [`block_for_nonce`] — and
/// therefore [`mine_dual`] and [`mine_dual_from`], i.e. every path a stock rig
/// can produce a share through — sets `blake4_hash = blake4(header, nonce)`. The
/// equality already holds for them, so enabling the flag cannot change their
/// accept rate. See the `strict_lane_a_*` tests.
pub fn verify_dual<G: VdfGroup>(g: &G, block: &DualLaneBlock, target: u64) -> bool {
    verify_dual_with(g, block, target, *STRICT_LANE_A)
}

/// [`verify_dual`] with the Lane-A strictness passed explicitly instead of read
/// from the environment. Exists so both policies are testable deterministically
/// in ONE process (the env-backed `LazyLock` resolves once and cannot be toggled
/// between tests) and so a caller that has already decided the policy — e.g. a
/// staged rollout, or a replay tool re-verifying historical blocks under the
/// rules that were live at the time — can state it explicitly.
pub fn verify_dual_with<G: VdfGroup>(
    g: &G,
    block: &DualLaneBlock,
    target: u64,
    strict_lane_a: bool,
) -> bool {
    // Lane A: re-hash the claimed nonce.
    let recomputed = blake4(&block.header, block.nonce);
    if recomputed > target {
        return false;
    }
    if block.blake4_hash > target {
        return false;
    }
    // Lane A (strict): the CLAIMED word must BE the recomputed word. Without this
    // the two checks above are independent and a forged-but-under-target word
    // rides into the header alongside an honest nonce.
    if strict_lane_a && block.blake4_hash != recomputed {
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

    /// SLICE 2 — the forgery the weak gate lets through, and the fix.
    ///
    /// An HONEST nonce (real Lane-A work, real VDF) paired with a FORGED Lane-A
    /// word of `0`: under-target, so the old independent-bounds gate accepts it,
    /// and `fold_tip` then writes that `0` into the header as the block's PoW.
    #[test]
    fn strict_lane_a_blocks_forged_word_but_legacy_accepts_it() {
        let g = ModSquaring::bench_2048();
        let header = b"sigil-g0-lane-a-forgery";
        let target = u64::MAX >> 12;
        let honest = mine_dual(header, target, 2_000, &g);

        // Forge ONLY the claimed word; nonce + VDF stay genuine.
        let mut forged = honest.clone();
        forged.blake4_hash = 0;
        assert_ne!(
            forged.blake4_hash,
            blake4(&forged.header, forged.nonce),
            "precondition: the claimed word is not the real one"
        );

        assert!(
            verify_dual_with(&g, &forged, target, false),
            "LEGACY (strict=false): the forged word is accepted — this is the live-pool weakness"
        );
        assert!(
            !verify_dual_with(&g, &forged, target, true),
            "STRICT (strict=true): the forged word must be rejected"
        );
    }

    /// The go/no-go safety property: every share a stock rig can produce already
    /// satisfies the equality, so turning the flag ON cannot change its accept
    /// rate. Covers BOTH miner entry points (`mine_dual` and the pool-mode
    /// `mine_dual_from` / `block_for_nonce` GPU path).
    #[test]
    fn strict_lane_a_is_a_noop_for_honest_miners() {
        let g = ModSquaring::bench_2048();
        let target = u64::MAX >> 10;

        for i in 0..8u64 {
            let header = format!("sigil-g0-honest-{i}").into_bytes();

            // Path 1: the plain solve loop.
            let b = mine_dual(&header, target, 1_000, &g);
            assert_eq!(
                b.blake4_hash,
                blake4(&b.header, b.nonce),
                "mine_dual must report the recomputed word"
            );
            assert!(verify_dual_with(&g, &b, target, false));
            assert!(
                verify_dual_with(&g, &b, target, true),
                "honest mine_dual share must still verify under STRICT"
            );

            // Path 2: pool mode — resumable search, then the GPU-style assembler.
            if let (Some(pb), _) = mine_dual_from(&header, target, 1_000, &g, 0, 100_000) {
                assert_eq!(pb.blake4_hash, blake4(&pb.header, pb.nonce));
                assert!(
                    verify_dual_with(&g, &pb, target, true),
                    "honest mine_dual_from share must still verify under STRICT"
                );
                let rebuilt = block_for_nonce(&header, pb.nonce, &g, 1_000);
                assert!(
                    verify_dual_with(&g, &rebuilt, target, true),
                    "block_for_nonce (GPU half) must still verify under STRICT"
                );
            }
        }
    }

    /// STRICT must not weaken anything: the pre-existing tamper cases still fail.
    #[test]
    fn strict_lane_a_still_rejects_tampered_lanes() {
        let g = ModSquaring::bench_2048();
        let header = b"sigil-g0-strict-tamper";
        let target = u64::MAX >> 12;
        let block = mine_dual(header, target, 2_000, &g);

        let mut bad_a = block.clone();
        bad_a.nonce ^= 1;
        assert!(!verify_dual_with(&g, &bad_a, target, true), "tampered nonce");

        let mut bad_b = block;
        bad_b.vdf.y[0] ^= 1;
        assert!(!verify_dual_with(&g, &bad_b, target, true), "tampered VDF");
    }

    #[test]
    fn unit_formatting() {
        assert_eq!(format_flux(1e18), "1.000 Φ");
        assert_eq!(format_flux(3e9), "3.000 nΦ"); // 3 GH/s = 3 nanoflux
        assert_eq!(format_omega(1e6), "1.000 Ω"); // 1 Mturn/s = 1 omega
    }
}
