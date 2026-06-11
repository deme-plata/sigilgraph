//! SIGIL bank council — N-of-M (default 2-of-2) approval for treasury transfers.
//!
//! Mirrors the `flux_bank_*` propose → approve → execute shape: a council member FILES a
//! transfer proposal (and auto-approves it), other members APPROVE, and only when the proposal
//! reaches `threshold` distinct approvals does it become executable. Same contract as the rest of
//! sigil-bank: **no I/O, no state borrow** — sigil-rpcd moves the SIGIL through
//! `sigil_state::commit_state_transition` ONLY when [`Council::approve`] returns `Ok(true)`, then
//! calls [`Council::mark_executed`]. So a single key can never drain the treasury: two of two
//! council members must independently sign.

use serde::{Deserialize, Serialize};

use crate::WalletId;

/// 32-byte token id — mirrors `sigil_state::TokenId` without importing sigil-state (same
/// one-way-dep rule as [`crate::WalletId`]).
pub type TokenId = [u8; 32];

/// A pending (or executed) treasury transfer awaiting council approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub from: WalletId,
    pub to: WalletId,
    pub token: TokenId,
    pub amount: u128,
    /// Distinct council members who have approved (the proposer is the first).
    pub approvals: Vec<WalletId>,
    /// "pending" | "executed"
    pub status: String,
    pub created_ts: u64,
}

/// The bank council roster + threshold + the proposal book (persisted in its own flux-db key).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Council {
    pub members: Vec<WalletId>,
    pub threshold: usize,
    pub proposals: Vec<Proposal>,
}

impl Default for Council {
    fn default() -> Self {
        Council { members: Vec::new(), threshold: 2, proposals: Vec::new() }
    }
}

impl Council {
    /// Seed the roster + threshold (rpcd seeds [master, operator] / 2-of-2 on first boot). Idempotent.
    pub fn seed(&mut self, members: Vec<WalletId>, threshold: usize) {
        if self.members.is_empty() {
            self.members = members;
            self.threshold = threshold.max(1);
        }
    }

    pub fn is_member(&self, w: &WalletId) -> bool {
        self.members.contains(w)
    }

    pub fn get(&self, id: &str) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == id)
    }

    /// File a transfer proposal. The proposer must be a council member and auto-approves it.
    pub fn propose(
        &mut self,
        id: String,
        from: WalletId,
        to: WalletId,
        token: TokenId,
        amount: u128,
        proposer: WalletId,
        now: u64,
    ) -> Result<Proposal, String> {
        if !self.is_member(&proposer) {
            return Err("proposer is not a council member".into());
        }
        if amount == 0 {
            return Err("amount must be > 0".into());
        }
        let p = Proposal {
            id,
            from,
            to,
            token,
            amount,
            approvals: vec![proposer],
            status: "pending".into(),
            created_ts: now,
        };
        self.proposals.push(p.clone());
        Ok(p)
    }

    /// Add `approver`'s approval. Returns `Ok(true)` the moment the proposal reaches `threshold`
    /// (the caller then executes the transfer + calls [`Self::mark_executed`]); `Ok(false)` if more
    /// approvals are still needed. Idempotent per member; rejects non-members + non-pending proposals.
    pub fn approve(&mut self, id: &str, approver: WalletId) -> Result<bool, String> {
        if !self.is_member(&approver) {
            return Err("approver is not a council member".into());
        }
        let threshold = self.threshold;
        let p = self
            .proposals
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("proposal not found")?;
        if p.status != "pending" {
            return Err(format!("proposal {id} is {}", p.status));
        }
        if !p.approvals.contains(&approver) {
            p.approvals.push(approver);
        }
        Ok(p.approvals.len() >= threshold)
    }

    pub fn mark_executed(&mut self, id: &str) {
        if let Some(p) = self.proposals.iter_mut().find(|p| p.id == id) {
            p.status = "executed".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const M1: WalletId = [1u8; 32];
    const M2: WalletId = [2u8; 32];
    const OUTSIDER: WalletId = [9u8; 32];
    const TOK: TokenId = [0xAA; 32];

    #[test]
    fn two_of_two_requires_both_distinct_members() {
        let mut c = Council::default();
        c.seed(vec![M1, M2], 2);
        // outsider can't propose
        assert!(c.propose("p1".into(), M1, M2, TOK, 100, OUTSIDER, 0).is_err());
        // M1 proposes (auto-approves) → not yet at threshold
        let p = c.propose("p1".into(), M1, M2, TOK, 100, M1, 0).unwrap();
        assert_eq!(p.approvals.len(), 1);
        // M1 re-approving does NOT reach 2-of-2 (idempotent)
        assert_eq!(c.approve("p1", M1).unwrap(), false);
        // outsider can't approve
        assert!(c.approve("p1", OUTSIDER).is_err());
        // M2 approves → threshold reached
        assert_eq!(c.approve("p1", M2).unwrap(), true);
        c.mark_executed("p1");
        // re-approving an executed proposal is rejected
        assert!(c.approve("p1", M1).is_err());
    }
}
