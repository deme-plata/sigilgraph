//! onramp.rs — the fiat door: a confirmed Stripe payment becomes on-chain credits.
//! Pricing: $0.02 = 1 credit ⇒ $2 = 100 credits. Idempotent — a webhook replay of the
//! same payment id is a no-op (never double-credits).

use crate::{topup_credit, MandatError};
use flux_uint::Amount;
use sigil_state::{SigilState, StateTransition, WalletId};
use std::collections::HashSet;

/// $0.02 per credit.
pub const CENTS_PER_CREDIT: u64 = 2;

/// A confirmed payment from the fiat rail (Stripe). `id` is the Stripe event id.
#[derive(Clone, Debug)]
pub struct Payment {
    pub id: String,
    pub usd_cents: u64,
}

/// Credits granted for a USD-cent amount. $2 (200¢) → 100 credits.
pub const fn credits_for(usd_cents: u64) -> Amount {
    Amount::from_ore((usd_cents / CENTS_PER_CREDIT) as u128)
}

/// Apply a confirmed payment as a top-up — IDEMPOTENT. Returns:
///   • `None`            → already applied (webhook replay), nothing to commit
///   • `Some(Ok(t))`     → commit `t` to credit the account
///   • `Some(Err(e))`    → overflow (won't happen for sane amounts)
/// `seen` is the service's persisted set of applied payment ids.
pub fn apply_payment(
    state: &SigilState,
    account: &WalletId,
    payment: &Payment,
    at_height: u64,
    seen: &mut HashSet<String>,
) -> Option<Result<StateTransition, MandatError>> {
    if !seen.insert(payment.id.clone()) {
        return None; // replay — the credits already landed once
    }
    Some(topup_credit(state, account, credits_for(payment.usd_cents), at_height))
}
