//! sigil-sigverify — parallel signature verification (SIGIL Prototype 7-E).
//!
//! The STARGATE 500M handoff found: **state is solved, crypto is the wall.**
//! Signature verification caps end-to-end TPS. The cheapest way to widen that
//! ceiling is to verify in parallel — a block's N transaction signatures are
//! mutually independent, so they fan across N cores with near-linear speedup.
//!
//! This crate is the reusable primitive:
//!   - [`Verifier`] — a scheme-generic trait (`verify(msg, sig, pk) -> bool`)
//!   - [`Ed25519Verifier`] — the hot-path scheme (fast, ~113k/s/48c measured)
//!   - [`SqiSignVerifier`] — the finality scheme (post-quantum, `--features sqisign`)
//!   - [`verify_batch_parallel`] — rayon fan-out: `&[Item] -> Vec<bool>`
//!   - [`all_valid_parallel`] — short-circuit "are they ALL valid?" for block gates
//!
//! Composes with:
//!   - P7-A verify-once mempool: call `verify_batch_parallel` on tx ingest, then
//!     consensus orders tx-hashes and NEVER re-verifies.
//!   - P7-C hot/settlement split: ed25519 for the hot path, SQIsign for finality —
//!     same parallel path, different `Verifier` impl, chosen per tx scheme.

#![forbid(unsafe_code)]

use rayon::prelude::*;

/// One verification job: message, signature, public key — all borrowed.
/// Cheap to construct per tx; the heavy work is the verify itself.
#[derive(Debug, Clone, Copy)]
pub struct VerifyItem<'a> {
    pub msg: &'a [u8],
    pub sig: &'a [u8],
    pub pubkey: &'a [u8],
}

/// A signature scheme that can verify (msg, sig, pubkey) → valid?
///
/// Implementations MUST be pure + thread-safe (`Sync`) — `verify_batch_parallel`
/// calls `verify` from many rayon worker threads concurrently. Return `false`
/// for any malformed input rather than panicking; a bad sig is a normal,
/// expected outcome, not an error condition.
pub trait Verifier: Sync {
    fn verify(&self, item: &VerifyItem<'_>) -> bool;
    /// Human label for logs/metrics.
    fn scheme(&self) -> &'static str;
}

/// Verify every item in parallel across all available cores. Returns a
/// `Vec<bool>` aligned 1:1 with the input — `out[i]` is whether `items[i]`
/// verified. Order-preserving (rayon's `par_iter().map().collect()` keeps
/// index order), so callers can map results straight back to their txs.
///
/// This is the core P7-E primitive: O(N/cores) wall-time instead of O(N).
pub fn verify_batch_parallel<V: Verifier>(verifier: &V, items: &[VerifyItem<'_>]) -> Vec<bool> {
    items.par_iter().map(|it| verifier.verify(it)).collect()
}

/// Short-circuiting "are ALL valid?" — for a block-acceptance gate where one
/// bad sig means reject the whole batch. `par_iter().all()` stops scheduling
/// new work as soon as a `false` is found, so an early-failing block costs
/// far less than verifying every sig.
pub fn all_valid_parallel<V: Verifier>(verifier: &V, items: &[VerifyItem<'_>]) -> bool {
    items.par_iter().all(|it| verifier.verify(it))
}

/// Count how many of the batch are valid (parallel). Useful for partial-accept
/// policies + metrics ("847/850 sigs valid this block").
pub fn count_valid_parallel<V: Verifier>(verifier: &V, items: &[VerifyItem<'_>]) -> usize {
    items.par_iter().filter(|it| verifier.verify(it)).count()
}

// ── ed25519 (hot path) ────────────────────────────────────────────────────────

/// Ed25519 verifier — the high-frequency hot-path scheme. 32-byte pubkey,
/// 64-byte sig. Matches the X-Wallet-Auth derivation Quillon/SIGIL wallets use
/// (priv = SHA3-256(seed), pub = Ed25519(priv)).
#[derive(Debug, Default, Clone, Copy)]
pub struct Ed25519Verifier;

impl Verifier for Ed25519Verifier {
    fn verify(&self, item: &VerifyItem<'_>) -> bool {
        use ed25519_dalek::{Signature, VerifyingKey};
        // pubkey must be exactly 32 bytes
        let pk_bytes: [u8; 32] = match item.pubkey.try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };
        let vk = match VerifyingKey::from_bytes(&pk_bytes) {
            Ok(v) => v,
            Err(_) => return false,
        };
        // sig must be exactly 64 bytes
        let sig_bytes: [u8; 64] = match item.sig.try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };
        let sig = Signature::from_bytes(&sig_bytes);
        // verify_strict rejects small-order/torsion pubkeys (malleability hardening)
        vk.verify_strict(item.msg, &sig).is_ok()
    }
    fn scheme(&self) -> &'static str { "ed25519" }
}

// ── SQIsign (finality) ────────────────────────────────────────────────────────

/// SQIsign L5 verifier — the post-quantum finality scheme. Only built with
/// `--features sqisign` (pulls flux-sqisign + its arkworks/isogeny deps).
/// 129-byte pubkey, 292-byte sig. Slow per-verify (the wall) — which is
/// exactly why parallelizing it matters most.
#[cfg(feature = "sqisign")]
#[derive(Debug, Default, Clone, Copy)]
pub struct SqiSignVerifier;

#[cfg(feature = "sqisign")]
impl Verifier for SqiSignVerifier {
    fn verify(&self, item: &VerifyItem<'_>) -> bool {
        flux_sqisign::verify(item.msg, item.sig, item.pubkey).unwrap_or(false)
    }
    fn scheme(&self) -> &'static str { "sqisign-l5" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Deterministic signing key from a 32-byte seed (any 32 bytes is a valid
    /// ed25519 secret) — no rand dependency, fully reproducible test vectors.
    /// Avoids `SigningKey::generate` (gated behind the rand_core feature).
    fn ed_keypair_seeded(n: u64) -> SigningKey {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        // salt the rest so distinct n give well-separated secrets
        for (i, b) in seed.iter_mut().enumerate().skip(8) {
            *b = (n as u8).wrapping_add(i as u8).wrapping_mul(31);
        }
        SigningKey::from_bytes(&seed)
    }

    fn ed_keypair() -> SigningKey {
        ed_keypair_seeded(0xA17E)
    }

    /// Build (sig, pk) owned bytes for an ed25519 signer over `msg`.
    fn ed_signed(kp: &SigningKey, msg: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let sig = kp.sign(msg).to_bytes().to_vec();
        let pk = kp.verifying_key().to_bytes().to_vec();
        (sig, pk)
    }

    #[test]
    fn ed25519_single_valid() {
        let sk = ed_keypair();
        let msg = b"sigil tx canonical bytes";
        let (sig, pk) = ed_signed(&sk, msg);
        let v = Ed25519Verifier;
        assert!(v.verify(&VerifyItem { msg, sig: &sig, pubkey: &pk }));
    }

    #[test]
    fn ed25519_rejects_tampered_msg() {
        let sk = ed_keypair();
        let (sig, pk) = ed_signed(&sk, b"original");
        let v = Ed25519Verifier;
        assert!(!v.verify(&VerifyItem { msg: b"tampered", sig: &sig, pubkey: &pk }));
    }

    #[test]
    fn ed25519_rejects_wrong_pubkey() {
        let sk = ed_keypair_seeded(1);
        let other = ed_keypair_seeded(2);
        let msg = b"msg";
        let (sig, _) = ed_signed(&sk, msg);
        let wrong_pk = other.verifying_key().to_bytes().to_vec();
        let v = Ed25519Verifier;
        assert!(!v.verify(&VerifyItem { msg, sig: &sig, pubkey: &wrong_pk }));
    }

    #[test]
    fn ed25519_rejects_malformed_lengths() {
        let v = Ed25519Verifier;
        // short pubkey, short sig — must return false, never panic
        assert!(!v.verify(&VerifyItem { msg: b"m", sig: &[0u8; 10], pubkey: &[0u8; 5] }));
        assert!(!v.verify(&VerifyItem { msg: b"m", sig: &[0u8; 64], pubkey: &[0u8; 31] }));
        assert!(!v.verify(&VerifyItem { msg: b"m", sig: &[0u8; 63], pubkey: &[0u8; 32] }));
    }

    #[test]
    fn batch_parallel_all_valid() {
        let v = Ed25519Verifier;
        // build 200 valid signed items (distinct deterministic keys)
        let keys: Vec<_> = (0..200u64).map(ed_keypair_seeded).collect();
        let msgs: Vec<Vec<u8>> = (0..200).map(|i| format!("tx-{i}").into_bytes()).collect();
        let sigpks: Vec<(Vec<u8>, Vec<u8>)> =
            keys.iter().zip(&msgs).map(|(k, m)| ed_signed(k, m)).collect();
        let items: Vec<VerifyItem> = msgs.iter().zip(&sigpks)
            .map(|(m, (s, p))| VerifyItem { msg: m, sig: s, pubkey: p })
            .collect();

        let results = verify_batch_parallel(&v, &items);
        assert_eq!(results.len(), 200);
        assert!(results.iter().all(|&r| r), "all 200 must verify");
        assert!(all_valid_parallel(&v, &items));
        assert_eq!(count_valid_parallel(&v, &items), 200);
    }

    #[test]
    fn batch_parallel_detects_one_bad_apple() {
        let v = Ed25519Verifier;
        let keys: Vec<_> = (0..50u64).map(ed_keypair_seeded).collect();
        let msgs: Vec<Vec<u8>> = (0..50).map(|i| format!("tx-{i}").into_bytes()).collect();
        let mut sigpks: Vec<(Vec<u8>, Vec<u8>)> =
            keys.iter().zip(&msgs).map(|(k, m)| ed_signed(k, m)).collect();
        // corrupt sig #37
        sigpks[37].0[0] ^= 0xFF;
        let items: Vec<VerifyItem> = msgs.iter().zip(&sigpks)
            .map(|(m, (s, p))| VerifyItem { msg: m, sig: s, pubkey: p })
            .collect();

        let results = verify_batch_parallel(&v, &items);
        assert!(!results[37], "corrupted sig #37 must fail");
        assert_eq!(count_valid_parallel(&v, &items), 49, "exactly one invalid");
        assert!(!all_valid_parallel(&v, &items), "block gate must reject the batch");
    }

    #[test]
    fn batch_preserves_order() {
        // results[i] must correspond to items[i] even under parallel execution.
        let v = Ed25519Verifier;
        let sk = ed_keypair();
        let mut items_owned: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
        for i in 0..20 {
            let msg = format!("m{i}").into_bytes();
            let (sig, pk) = ed_signed(&sk, &msg);
            // deliberately break even indices so the valid/invalid pattern is positional
            let sig = if i % 2 == 0 { sig } else { let mut s = sig; s[0] ^= 1; s };
            items_owned.push((msg, sig, pk));
        }
        let items: Vec<VerifyItem> = items_owned.iter()
            .map(|(m, s, p)| VerifyItem { msg: m, sig: s, pubkey: p }).collect();
        let results = verify_batch_parallel(&v, &items);
        for (i, &r) in results.iter().enumerate() {
            assert_eq!(r, i % 2 == 0, "index {i} valid-state must match position");
        }
    }

    #[test]
    fn empty_batch_is_all_valid_vacuously() {
        let v = Ed25519Verifier;
        assert_eq!(verify_batch_parallel(&v, &[]).len(), 0);
        assert!(all_valid_parallel(&v, &[]), "empty batch is vacuously all-valid");
        assert_eq!(count_valid_parallel(&v, &[]), 0);
    }
}
