//! auth.rs — per-request wallet-signature authorization for the mutating RPC
//! routes served by `sigil-rpcd`.
//!
//! THE PROBLEM this closes: every balance-moving route used to take the actor
//! wallet (`from` / `miner` / `operator_pool`) as a plain request field with no
//! proof the caller controlled it — so anyone could `curl` another wallet's
//! funds away, mint, or drain a pool. (Audit 2026-06-10, C2–C5/H8.)
//!
//! THE FIX: a wallet IS an ed25519 public key (32 bytes). To authorize a
//! mutating request the caller signs a canonical, domain-separated, nonce'd
//! message with the matching secret key and submits the 64-byte signature
//! (`sig`, hex) and a strictly-increasing `nonce`. The server rebuilds the exact
//! message, verifies it against the ACTOR wallet, and rejects a non-increasing
//! nonce (replay protection). Funds can therefore only be moved by the holder
//! of the wallet's key.
//!
//! Canonical message format (UTF-8 — trivial to reproduce in any client):
//!
//! ```text
//! sigil-rpc/v1|<action>|<field0>|<field1>|...|nonce=<nonce>
//! ```
//!
//! Each route fixes its own `action` tag and ordered field list (see the route
//! handlers); the client must concatenate identically. Domain separation across
//! actions means a signature for one action can't authorize another.

use sigil_oauth::verify_sig;
use sigil_state::WalletId;

/// Domain tag — bump the version suffix on any breaking change to the message
/// layout so old signatures can't be replayed against new semantics.
pub const AUTH_DOMAIN: &str = "sigil-rpc/v1";

/// Build the canonical message a caller must sign for `action` with the ordered
/// `fields` (each already formatted to its on-the-wire string) and `nonce`.
pub fn auth_message(action: &str, fields: &[&str], nonce: u64) -> Vec<u8> {
    let mut s = String::with_capacity(48 + fields.iter().map(|f| f.len() + 1).sum::<usize>());
    s.push_str(AUTH_DOMAIN);
    s.push('|');
    s.push_str(action);
    for f in fields {
        s.push('|');
        s.push_str(f);
    }
    s.push_str("|nonce=");
    s.push_str(&nonce.to_string());
    s.into_bytes()
}

/// Verify that `sig` authorizes (`action`, `fields`, `nonce`) as `actor`. Does
/// NOT check nonce monotonicity — the daemon enforces that with its per-wallet
/// nonce store (so replay is caught even across reconnects).
pub fn verify_request(
    actor: &WalletId,
    action: &str,
    fields: &[&str],
    nonce: u64,
    sig: &[u8; 64],
) -> bool {
    verify_sig(actor, &auth_message(action, fields, nonce), sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oauth::Keypair;

    #[test]
    fn valid_signature_authorizes_action() {
        let kp = Keypair::from_seed(&[3u8; 32]);
        let actor = kp.pubkey();
        let nonce = 1700000000000;
        let fields = ["aa", "bb", "100"];
        let sig = kp.sign(&auth_message("swap", &fields, nonce));
        assert!(verify_request(&actor, "swap", &fields, nonce, &sig));
    }

    #[test]
    fn wrong_actor_rejected() {
        let signer = Keypair::from_seed(&[3u8; 32]);
        let other = Keypair::from_seed(&[4u8; 32]).pubkey();
        let nonce = 5;
        let fields = ["aa"];
        let sig = signer.sign(&auth_message("swap", &fields, nonce));
        // The signature is valid, but for a DIFFERENT wallet than `other`.
        assert!(!verify_request(&other, "swap", &fields, nonce, &sig));
    }

    #[test]
    fn tampered_field_rejected() {
        let kp = Keypair::from_seed(&[3u8; 32]);
        let actor = kp.pubkey();
        let nonce = 9;
        let sig = kp.sign(&auth_message("swap", &["aa", "100"], nonce));
        // Attacker bumps the amount after signing → message changes → rejected.
        assert!(!verify_request(&actor, "swap", &["aa", "999"], nonce, &sig));
    }

    #[test]
    fn cross_action_replay_rejected() {
        let kp = Keypair::from_seed(&[3u8; 32]);
        let actor = kp.pubkey();
        let nonce = 9;
        let fields = ["aa", "100"];
        let sig = kp.sign(&auth_message("swap", &fields, nonce));
        // A swap signature must not authorize a credit with the same fields.
        assert!(!verify_request(&actor, "credit", &fields, nonce, &sig));
    }

    #[test]
    fn nonce_is_bound_into_signature() {
        let kp = Keypair::from_seed(&[3u8; 32]);
        let actor = kp.pubkey();
        let fields = ["aa"];
        let sig = kp.sign(&auth_message("swap", &fields, 1));
        // Replaying the same sig under a different nonce fails (nonce is signed).
        assert!(!verify_request(&actor, "swap", &fields, 2, &sig));
    }
}
