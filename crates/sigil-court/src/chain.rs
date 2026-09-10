//! Commit `court_root` into the chain — the same way the Nation does: a reserved slot under the
//! governance contract, through the real `commit_state_transition` path, so the court's whole state
//! rides `contract_state_root` into the block header without any schema change.

use sigil_state::{commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation, StateRoots, StateTransition};

use crate::SupremeCourt;

/// Same governance contract as `sigil-nation-chain::gov_contract` (one contract, many slots).
pub fn gov_contract() -> ContractId {
    let mut id = [0u8; 32];
    let tag = b"sigil:nation:gov";
    id[..tag.len()].copy_from_slice(tag);
    id
}

/// The slot that holds the current `court_root`.
pub fn court_root_slot() -> SlotId {
    let mut s = [0u8; 32];
    let tag = b"court_root";
    s[..tag.len()].copy_from_slice(tag);
    s
}

pub fn court_root_mutation(court: &SupremeCourt) -> StateMutation {
    StateMutation::SetContractSlot { contract: gov_contract(), slot: court_root_slot(), value: court.court_root() }
}

pub fn commit_court_root(state: &mut SigilState, court: &SupremeCourt, height: u64) -> Result<StateRoots, CommitError> {
    let t = StateTransition { at_height: height, mutations: vec![court_root_mutation(court)] };
    commit_state_transition(state, &t, height)
}

pub fn read_court_root(state: &SigilState) -> [u8; 32] {
    state.contract_slot(&gov_contract(), &court_root_slot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::Rank;

    #[test]
    fn court_root_lands_in_contract_state_root_and_moves_with_the_docket() {
        let mut s = SigilState::new();
        let mut c = SupremeCourt::from_seed(&[3; 32]);
        let before = s.roots().contract_state_root;
        let r1 = commit_court_root(&mut s, &c, 1).unwrap();
        assert_ne!(r1.contract_state_root, before);
        assert_eq!(read_court_root(&s), c.court_root());
        c.appoint([1; 32], Rank::Justice, 2).unwrap();
        let r2 = commit_court_root(&mut s, &c, 2).unwrap();
        assert_ne!(r2.contract_state_root, r1.contract_state_root);
        assert_eq!(read_court_root(&s), c.court_root());
    }
}
