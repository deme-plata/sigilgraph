//! sigil-treasury — SIGIL Nations v0.4: the foundation treasury.
//!
//! Three jobs, all committed in [`Treasury::root`]:
//! 1. **Collect** the 10% dev fee on settled work ([`collect_dev_fee`]).
//! 2. **Protect** the balance with a **MAX-WINS watermark** ([`commit_sync`]): a stale or partial
//!    observation can NEVER lower the balance. This is the literal fix for the Quillon
//!    `save_wallet_balances` bug — a replay wrote stale data and dropped a wallet 3200 → 1484. Here a
//!    sync that would lower the balance is refused, by construction.
//! 3. **Pay out** only through a **council 2-of-2 gate** ([`payout`]) — the single path that lowers the
//!    balance. No master key; quorum moves the money.
//!
//! [`Treasury::root`] is an O(1) BLAKE3 commit over (balance, total_in, total_out), so the treasury
//! state lives in the root, not a drift-prone side ledger.

/// Dev fee in basis points (1000 bps = 10%).
pub const DEV_FEE_BPS: u128 = 1000;
const _: () = assert!(DEV_FEE_BPS <= 10_000, "dev fee can't exceed 100%");

use std::collections::BTreeSet;

/// The 10% dev fee on a gross settlement amount.
pub fn dev_fee(gross: u128) -> u128 { gross * DEV_FEE_BPS / 10_000 }

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotApproved,  // payout lacked a council 2-of-2
    Insufficient, // payout exceeds the balance
    AlreadySpent, // this proposal already funded a payout (spend-once)
}

fn spend_hash(proposal: u64, amount: u128) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&proposal.to_le_bytes());
    h.update(&amount.to_le_bytes());
    *h.finalize().as_bytes()
}
fn xor_into(acc: &mut [u8; 32], h: &[u8; 32]) {
    for (a, b) in acc.iter_mut().zip(h.iter()) { *a ^= *b; }
}

#[derive(Default)]
pub struct Treasury {
    balance: u128,
    total_in: u128,
    total_out: u128,
    spent: BTreeSet<u64>,   // proposals that already funded a payout — spend-once
    spent_root: [u8; 32],   // XOR accumulator over (proposal, amount) spends
}

impl Treasury {
    pub fn new() -> Self { Self::default() }
    pub fn balance(&self) -> u128 { self.balance }
    pub fn total_in(&self) -> u128 { self.total_in }
    pub fn total_out(&self) -> u128 { self.total_out }

    /// Collect the 10% dev fee on `gross` settled work into the treasury. Returns the fee taken.
    pub fn collect_dev_fee(&mut self, gross: u128) -> u128 {
        let fee = dev_fee(gross);
        // saturating: an overflow here must not panic the writer (or wrap the
        // MAX-WINS watermark this crate exists to protect).
        self.balance = self.balance.saturating_add(fee);
        self.total_in = self.total_in.saturating_add(fee);
        fee
    }

    /// Direct inflow (e.g. a grant/endowment) — monotonic up.
    pub fn accrue(&mut self, amount: u128) {
        self.balance = self.balance.saturating_add(amount);
        self.total_in = self.total_in.saturating_add(amount);
    }

    /// MAX-WINS sync — the Quillon fix. A stale/partial observation can only RAISE the balance, never
    /// lower it. Returns true if it raised the balance, false if refused (`observed <= balance`).
    pub fn commit_sync(&mut self, observed: u128) -> bool {
        if observed > self.balance {
            self.total_in = self.total_in.saturating_add(observed - self.balance);
            self.balance = observed;
            true
        } else {
            false // refuse to destroy a higher balance with a stale lower one
        }
    }

    /// Council-gated payout (governance money), funded by a specific `proposal`. `council_2of2` MUST
    /// come from a sigil-council `MoneyOrConsensus` Passed outcome. This is the ONLY path that lowers
    /// the balance, and each proposal can fund **at most one** payout (spend-once).
    pub fn payout(&mut self, proposal: u64, amount: u128, council_2of2: bool) -> Result<(), Error> {
        if !council_2of2 { return Err(Error::NotApproved); }
        if self.spent.contains(&proposal) { return Err(Error::AlreadySpent); }
        if amount > self.balance { return Err(Error::Insufficient); }
        self.balance -= amount;
        self.total_out += amount;
        self.spent.insert(proposal);
        xor_into(&mut self.spent_root, &spend_hash(proposal, amount));
        Ok(())
    }

    /// Has `proposal` already funded a payout?
    pub fn has_spent(&self, proposal: u64) -> bool { self.spent.contains(&proposal) }

    /// O(1) committed root over (balance, total_in, total_out, spent set).
    pub fn root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&self.balance.to_le_bytes());
        h.update(&self.total_in.to_le_bytes());
        h.update(&self.total_out.to_le_bytes());
        h.update(&self.spent_root);
        *h.finalize().as_bytes()
    }
    pub fn root_hex(&self) -> String { hex::encode(self.root()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_fee_is_ten_percent() {
        assert_eq!(dev_fee(1000), 100);
        assert_eq!(dev_fee(650), 65);
    }

    #[test]
    fn collect_grows_balance_and_total_in() {
        let mut t = Treasury::new();
        let f = t.collect_dev_fee(1000);
        assert_eq!(f, 100);
        assert_eq!(t.balance(), 100);
        assert_eq!(t.total_in(), 100);
    }

    #[test]
    fn max_wins_refuses_a_lower_sync() {
        let mut t = Treasury::new();
        t.accrue(3200);
        // a stale replay reports 1484 — the EXACT Quillon scenario; must be refused
        let raised = t.commit_sync(1484);
        assert!(!raised, "a lower observation must NOT be applied");
        assert_eq!(t.balance(), 3200, "the higher balance must survive");
    }

    #[test]
    fn max_wins_accepts_a_higher_sync() {
        let mut t = Treasury::new();
        t.accrue(1000);
        assert!(t.commit_sync(1500));
        assert_eq!(t.balance(), 1500);
    }

    #[test]
    fn payout_requires_council_2of2() {
        let mut t = Treasury::new();
        t.accrue(500);
        assert_eq!(t.payout(1, 100, false), Err(Error::NotApproved));
        assert_eq!(t.balance(), 500); // nothing moved
    }

    #[test]
    fn payout_executes_on_approval() {
        let mut t = Treasury::new();
        t.accrue(500);
        t.payout(1, 120, true).unwrap();
        assert_eq!(t.balance(), 380);
        assert_eq!(t.total_out(), 120);
        assert!(t.has_spent(1));
    }

    #[test]
    fn payout_cannot_overdraw() {
        let mut t = Treasury::new();
        t.accrue(50);
        assert_eq!(t.payout(1, 100, true), Err(Error::Insufficient));
        assert_eq!(t.balance(), 50);
    }

    #[test]
    fn spend_once_blocks_a_second_payout_on_the_same_proposal() {
        let mut t = Treasury::new();
        t.accrue(1000);
        t.payout(7, 300, true).unwrap();              // first draw ok
        assert_eq!(t.payout(7, 300, true), Err(Error::AlreadySpent)); // re-spend blocked
        assert_eq!(t.balance(), 700);                  // only one 300 left
        // a DIFFERENT proposal can still pay
        t.payout(8, 200, true).unwrap();
        assert_eq!(t.balance(), 500);
    }

    #[test]
    fn already_spent_beats_insufficient() {
        let mut t = Treasury::new();
        t.accrue(100);
        t.payout(1, 100, true).unwrap();              // drains to 0, proposal 1 spent
        // re-spend proposal 1 for a huge amount → AlreadySpent (not Insufficient)
        assert_eq!(t.payout(1, 9999, true), Err(Error::AlreadySpent));
    }

    #[test]
    fn root_moves_on_state_change() {
        let mut t = Treasury::new();
        let r0 = t.root();
        t.collect_dev_fee(1000);
        let r1 = t.root();
        assert_ne!(r0, r1);
        t.payout(1, 50, true).unwrap();
        assert_ne!(r1, t.root());
    }
}
