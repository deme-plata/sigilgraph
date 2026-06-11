//! SIGIL agent spend-mandates — the ON-CHAIN home for LANE-Y's agent-money guard.
//!
//! Ports the quillon-wallet `agent_create_mandate` semantics: an autonomous agent may move
//! funds ONLY under an active, unexpired mandate that still has spend headroom
//! (`max_amount - spent`). The mandate carries a stated `purpose` and an expiry, so an
//! operator grants a bounded, auditable spend authority instead of an open-ended key.
//!
//! Same contract as the rest of sigil-bank: **no I/O, no state borrow**. sigil-rpcd persists
//! this book in its own flux-db key and threads the actual SIGIL movement through
//! `sigil_state::commit_state_transition`, so the CLI and the MCP share ONE source of truth
//! (the chain) instead of an MCP-local file.

use serde::{Deserialize, Serialize};

use crate::WalletId;

/// A bounded, expiring spend authority granted to an agent wallet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mandate {
    pub id: String,
    pub agent: WalletId,
    pub max_amount: u128,
    pub spent: u128,
    pub purpose: String,
    pub created_ts: u64,
    pub expires_ts: u64,
    /// "active" | "closed"
    pub status: String,
}

impl Mandate {
    /// Live = active AND not past its expiry (an expired mandate authorizes nothing).
    pub fn is_live(&self, now: u64) -> bool {
        self.status == "active" && self.expires_ts > now
    }
    pub fn headroom(&self) -> u128 {
        self.max_amount.saturating_sub(self.spent)
    }
}

/// The persisted set of mandates (own flux-db key, additive — same pattern as the credit vault).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MandateBook {
    pub mandates: Vec<Mandate>,
}

impl MandateBook {
    /// Grant a mandate. `id` is supplied by the caller (rpcd derives a unique one). Returns a
    /// clone of the new mandate for the response.
    pub fn create(
        &mut self,
        id: String,
        agent: WalletId,
        max_amount: u128,
        purpose: String,
        ttl_secs: u64,
        now: u64,
    ) -> Mandate {
        let m = Mandate {
            id,
            agent,
            max_amount,
            spent: 0,
            purpose,
            created_ts: now,
            expires_ts: now.saturating_add(ttl_secs),
            status: "active".into(),
        };
        self.mandates.push(m.clone());
        m
    }

    pub fn get(&self, id: &str) -> Option<&Mandate> {
        self.mandates.iter().find(|m| m.id == id)
    }

    /// Revoke a mandate by id. Returns false if no such mandate.
    pub fn close(&mut self, id: &str) -> bool {
        match self.mandates.iter_mut().find(|m| m.id == id) {
            Some(m) => {
                m.status = "closed".into();
                true
            }
            None => false,
        }
    }

    /// The live mandate for `agent` with the most headroom (None if the agent has no live mandate).
    pub fn active_for(&self, agent: &WalletId, now: u64) -> Option<&Mandate> {
        self.mandates
            .iter()
            .filter(|m| &m.agent == agent && m.is_live(now))
            .max_by_key(|m| m.headroom())
    }

    /// Charge `amount` against mandate `id`: it must be live AND have the headroom. Records the
    /// spend and returns the REMAINING headroom. The caller moves funds only AFTER this `Ok`, so
    /// the mandate cap is enforced before any balance changes.
    pub fn charge(&mut self, id: &str, amount: u128, now: u64) -> Result<u128, String> {
        let m = self
            .mandates
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or("mandate not found")?;
        if !m.is_live(now) {
            return Err(format!("mandate {id} is {} / expired", m.status));
        }
        let new_spent = m.spent.checked_add(amount).ok_or("mandate spend overflow")?;
        if new_spent > m.max_amount {
            return Err(format!(
                "over mandate cap: spent {} + {} > max {}",
                m.spent, amount, m.max_amount
            ));
        }
        m.spent = new_spent;
        Ok(m.max_amount - new_spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: WalletId = [7u8; 32];

    #[test]
    fn mandate_lifecycle_caps_and_expires() {
        let mut b = MandateBook::default();
        let m = b.create("m1".into(), A, 1000, "trade".into(), 100, 1_000);
        assert_eq!(m.headroom(), 1000);
        assert!(b.active_for(&A, 1_050).is_some(), "live before expiry");
        // charge within cap
        assert_eq!(b.charge("m1", 600, 1_050).unwrap(), 400);
        // over remaining headroom rejected (no spend recorded)
        assert!(b.charge("m1", 500, 1_050).is_err());
        assert_eq!(b.get("m1").unwrap().spent, 600);
        // expired authorizes nothing
        assert!(b.active_for(&A, 2_000).is_none(), "expired");
        assert!(b.charge("m1", 1, 2_000).is_err(), "expired charge rejected");
        // closed authorizes nothing
        assert!(b.close("m1"));
        assert!(b.active_for(&A, 1_050).is_none(), "closed");
    }
}
