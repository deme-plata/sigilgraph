//! onboard.rs — verified-user onboarding + the earning gate.
//!
//! The lane: **onboard a user → verify them (Discord / OAuth2 / self-signed) →
//! bind their wallet → they earn.** A VERIFIED user's mining/attestation reward is
//! credited to their own wallet; an UNVERIFIED user's reward routes to the master
//! (dev-fee) treasury. Honesty — a real, bound identity — is the price of earning.
//!
//! This composes with [`crate::credit_light_verifiers`] (the conserved pool→recipient
//! transfer): the gate only chooses the recipient. NATIVE is conserved either way,
//! and the 21M cap stays enforced by the chokepoint underneath.

use std::collections::BTreeMap;

use sigil_oauth::{verify_sig, wallet_id, Keypair};
use sigil_state::{SigilState, WalletId};

use crate::{credit_light_verifiers, LightCreditResult, RpcError};

/// How a user proved identity to be allowed to earn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyMethod {
    /// Discord OAuth — the user's Discord snowflake id, bound to their wallet.
    Discord(String),
    /// Generic OAuth2 (Google, GitHub, …) — provider + subject (the `sub` claim).
    OAuth2 { provider: String, subject: String },
    /// Pure on-chain: the wallet signed the challenge itself, no third party.
    SelfSigned,
}

impl VerifyMethod {
    fn tag(&self) -> Vec<u8> {
        match self {
            VerifyMethod::Discord(id) => [b"discord:", id.as_bytes()].concat(),
            VerifyMethod::OAuth2 { provider, subject } => {
                [b"oauth2:", provider.as_bytes(), b":", subject.as_bytes()].concat()
            }
            VerifyMethod::SelfSigned => b"self".to_vec(),
        }
    }
}

/// The canonical message a user signs to bind (wallet ⇄ identity). Domain-separated
/// and nonce'd so a signature can't be replayed across methods or sessions.
pub fn binding_message(wallet: &WalletId, method: &VerifyMethod, nonce: u64) -> Vec<u8> {
    let mut m = b"sigil-onboard:v1:".to_vec();
    m.extend_from_slice(wallet);
    m.push(b'|');
    m.extend_from_slice(&method.tag());
    m.push(b'|');
    m.extend_from_slice(&nonce.to_le_bytes());
    m
}

/// A user's verification record: the wallet (= pubkey), how they proved identity,
/// and the signature over the binding message.
#[derive(Clone, Debug)]
pub struct Verification {
    pub wallet: WalletId,
    pub method: VerifyMethod,
    pub nonce: u64,
    pub sig: [u8; 64],
}

impl Verification {
    /// Sign a fresh verification with the user's keypair (wallet = its pubkey).
    pub fn sign(kp: &Keypair, method: VerifyMethod, nonce: u64) -> Self {
        let wallet = kp.pubkey();
        let sig = kp.sign(&binding_message(&wallet, &method, nonce));
        Verification { wallet, method, nonce, sig }
    }
    /// Check the signature binds this wallet to this identity (the wallet IS the pubkey).
    pub fn is_valid(&self) -> bool {
        verify_sig(&self.wallet, &binding_message(&self.wallet, &self.method, self.nonce), &self.sig)
    }
    /// Human-readable wallet id (sgl1…/qnk… display), for greeting the user.
    pub fn wallet_display(&self) -> String {
        wallet_id(&self.wallet)
    }
}

/// The set of verified users. A wallet earns iff it has a valid verification here.
/// (In-memory here; the on-chain home is a committed registry root — same shape.)
#[derive(Default)]
pub struct VerifiedRegistry {
    by_wallet: BTreeMap<WalletId, Verification>,
}

impl VerifiedRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// Register a verification. Rejects an invalid signature (fail loud, no silent
    /// "verified" status without proof).
    pub fn register(&mut self, v: Verification) -> Result<(), RpcError> {
        if !v.is_valid() {
            return Err(RpcError::InvalidVerification);
        }
        self.by_wallet.insert(v.wallet, v);
        Ok(())
    }
    pub fn is_verified(&self, wallet: &WalletId) -> bool {
        self.by_wallet.get(wallet).map(|v| v.is_valid()).unwrap_or(false)
    }
    pub fn len(&self) -> usize {
        self.by_wallet.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_wallet.is_empty()
    }
}

/// Outcome of settling one attestation reward through the earning gate.
#[derive(Clone, Debug)]
pub struct EarnResult {
    /// The wallet that actually received the reward.
    pub recipient: WalletId,
    /// True if it went to the user (verified); false if it routed to the dev-fee.
    pub earned: bool,
    pub credit: LightCreditResult,
}

/// THE EARNING GATE. Settle a mining/attestation `reward` for `user`:
/// - verified  → credited to the user's own wallet (they earn);
/// - unverified→ routed to `master` (the dev-fee treasury).
/// Conserved: the reward is debited from `operator_pool` exactly once either way.
pub fn settle_attestation(
    state: &mut SigilState,
    height: u64,
    operator_pool: WalletId,
    reward: u128,
    user: WalletId,
    master: WalletId,
    registry: &VerifiedRegistry,
) -> Result<EarnResult, RpcError> {
    let earned = registry.is_verified(&user);
    let recipient = if earned { user } else { master };
    let credit = credit_light_verifiers(state, height, operator_pool, reward, &[recipient])?;
    Ok(EarnResult { recipient, earned, credit })
}

/// Onboard a brand-new user from a 32-byte seed (deterministic, spendable). Returns
/// the keypair (the user keeps the secret) and a self-signed verification they can
/// upgrade to Discord/OAuth2 later. NEVER returns a seedless/unspendable wallet.
pub fn onboard_user(seed: &[u8; 32], nonce: u64) -> (Keypair, Verification) {
    let kp = Keypair::from_seed(seed);
    let v = Verification::sign(&kp, VerifyMethod::SelfSigned, nonce);
    (kp, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::{commit_state_transition, StateMutation, StateTransition, NATIVE};

    const POOL: WalletId = [0x0C; 32];
    const MASTER: WalletId = [0x33; 32];

    fn seeded_pool(amount: u128) -> SigilState {
        let mut s = SigilState::new();
        commit_state_transition(
            &mut s,
            &StateTransition {
                at_height: 0,
                mutations: vec![StateMutation::SetBalance { wallet: POOL, token: NATIVE, amount }],
            },
            0,
        )
        .unwrap();
        s
    }

    #[test]
    fn discord_binding_roundtrips_and_tamper_fails() {
        let kp = Keypair::from_seed(&[7u8; 32]);
        let v = Verification::sign(&kp, VerifyMethod::Discord("123456789".into()), 1);
        assert!(v.is_valid());
        // tamper the bound identity → signature no longer verifies
        let mut bad = v.clone();
        bad.method = VerifyMethod::Discord("999999999".into());
        assert!(!bad.is_valid(), "rebinding to another discord id must break the sig");
    }

    #[test]
    fn verified_user_earns_to_own_wallet() {
        let mut s = seeded_pool(10_000);
        let kp = Keypair::from_seed(&[1u8; 32]);
        let user = kp.pubkey();
        let mut reg = VerifiedRegistry::new();
        reg.register(Verification::sign(&kp, VerifyMethod::Discord("agent-1".into()), 1)).unwrap();

        let r = settle_attestation(&mut s, 1, POOL, 500, user, MASTER, &reg).unwrap();
        assert!(r.earned, "verified user earns");
        assert_eq!(r.recipient, user);
        assert_eq!(s.balance_of(&user, &NATIVE), 500);
        assert_eq!(s.balance_of(&MASTER, &NATIVE), 0, "nothing to dev-fee");
    }

    #[test]
    fn unverified_user_routes_to_dev_fee() {
        let mut s = seeded_pool(10_000);
        let user = Keypair::from_seed(&[2u8; 32]).pubkey();
        let reg = VerifiedRegistry::new(); // user NOT registered

        let r = settle_attestation(&mut s, 1, POOL, 500, user, MASTER, &reg).unwrap();
        assert!(!r.earned, "unverified does not earn");
        assert_eq!(r.recipient, MASTER);
        assert_eq!(s.balance_of(&user, &NATIVE), 0, "user earns nothing while unverified");
        assert_eq!(s.balance_of(&MASTER, &NATIVE), 500, "reward routes to dev-fee");
    }

    #[test]
    fn native_is_conserved_either_way() {
        for verified in [true, false] {
            let mut s = seeded_pool(10_000);
            let kp = Keypair::from_seed(&[9u8; 32]);
            let user = kp.pubkey();
            let mut reg = VerifiedRegistry::new();
            if verified {
                reg.register(Verification::sign(&kp, VerifyMethod::SelfSigned, 1)).unwrap();
            }
            let before = s.balance_of(&POOL, &NATIVE) + s.balance_of(&user, &NATIVE) + s.balance_of(&MASTER, &NATIVE);
            settle_attestation(&mut s, 1, POOL, 500, user, MASTER, &reg).unwrap();
            let after = s.balance_of(&POOL, &NATIVE) + s.balance_of(&user, &NATIVE) + s.balance_of(&MASTER, &NATIVE);
            assert_eq!(before, after, "NATIVE conserved (verified={verified})");
        }
    }

    #[test]
    fn onboard_then_upgrade_to_discord() {
        // Cold start: a seed → spendable wallet + self-signed verification.
        let (kp, self_v) = onboard_user(&[42u8; 32], 0);
        assert!(self_v.is_valid());
        let mut reg = VerifiedRegistry::new();
        reg.register(self_v).unwrap();
        assert!(reg.is_verified(&kp.pubkey()), "self-signed onboard counts as verified");
        // Later the user links Discord — same wallet, stronger binding.
        reg.register(Verification::sign(&kp, VerifyMethod::Discord("link-1".into()), 2)).unwrap();
        assert_eq!(reg.len(), 1, "same wallet, upgraded binding (not a new identity)");
    }
}
