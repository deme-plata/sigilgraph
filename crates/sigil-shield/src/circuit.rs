//! Shielded-transfer circuits over LatticeGuard's R1CS `ArithmeticCircuit`.
//!
//! Wire convention (verified against `flux-lattice-guard/src/prover.rs::evaluate_linear_combination`):
//!   * index `i < num_public_inputs`  → `public_inputs[i]`
//!   * index `i >= num_public_inputs` → `witness[i - num_public_inputs]`
//! A constraint is `a · b = c` where each of a,b,c is a linear combination `Vec<(wire, coeff)>`.
//!
//! P1 ships only [`squaring_demo`] — a minimal REAL constraint that proves the LatticeGuard
//! plumbing end-to-end. [LANE-A] replaces it with [`shielded_transfer`] (see the stub below):
//! value conservation (Σin = Σout + fee) + Merkle membership of the input note + a fresh
//! nullifier. Keep this module the single owner of the circuit ↔ wire mapping so LANE-A and
//! the proof code never disagree on indices.

use flux_lattice_guard::{ArithmeticCircuit, LatticeGuardProof, Scalar};

/// Minimal real circuit: prove knowledge of `w` such that `w * w == public[0]`.
/// Returns `(circuit, witness, public_inputs)`. 1 public input (index 0), 1 witness (index 1).
pub fn squaring_demo(w: Scalar) -> (ArithmeticCircuit, Vec<Scalar>, Vec<Scalar>) {
    let num_public = 1;
    let num_witness = 1;
    let mut c = ArithmeticCircuit::new(num_public, num_witness);
    let w_wire = num_public; // witness w lives at index num_public (== 1)
    let pub_wire = 0; // public output at index 0
    // a = w, b = w, c = pub  →  w * w = pub
    c.add_multiplication_gate(
        vec![(w_wire, 1)],
        vec![(w_wire, 1)],
        vec![(pub_wire, 1)],
    );
    let witness = vec![w];
    let public_inputs = vec![w.wrapping_mul(w)];
    (c, witness, public_inputs)
}

/// Corrupt a proof so a correct verifier MUST reject it. Perturbs the first evaluation the
/// verifier checks against its error bound (LatticeGuard verifier Phase 3). If this ever
/// fails to be rejected, the LatticeGuard *verifier* has a soundness gap — a P1 finding to
/// fix in `flux-lattice-guard`, not something to paper over here.
pub fn tamper_first_byte(proof: &mut LatticeGuardProof) {
    // Shift the first evaluation well past any legitimate error bound, and disturb the
    // Fiat-Shamir transcript state so challenge reconstruction can't silently absorb it.
    proof.evaluations.0 = proof.evaluations.0.wrapping_add(1_000_003);
    proof.transcript_state[0] ^= 0xFF;
}

// ── LANE-A target (not yet implemented — do NOT ship a placeholder proof for it) ──
//
// pub fn shielded_transfer(...) -> (ArithmeticCircuit, Vec<Scalar>, Vec<Scalar>)
//   Public inputs:  merkle_root, nullifier, fee, output_commitment
//   Witness:        input_note (value, r, position), merkle_path, spend_key
//   Constraints:
//     1. output_commitment == Commit(value_out, pk_out, r_out)   (lattice commitment)
//     2. Σ input.value == Σ output.value + fee                    (conservation)
//     3. MerklePath(input_note_cm, path) == merkle_root           (membership)
//     4. nullifier == PRF(spend_key, position)                    (double-spend guard)
//   The u64 `Scalar` field here is a P1 stand-in; LANE-A pins the field/modulus to
//   RlweParams and the real commitment/PRF gadgets (docs/SIGIL_PRIVACY_ARCHITECTURE_v0.md P1).
