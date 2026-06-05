//! sigil-nation-chain — wire NATION-IN-A-BOX into the live SIGIL chain.
//!
//! A [`Nation`] keeps its whole economy in one 32-byte [`Nation::nation_root`]. This crate commits
//! that root into the chain's **`contract_state_root`** — via a reserved governance contract slot,
//! through the real [`commit_state_transition`] consensus path. So the nation's state becomes part of
//! consensus **without any header or schema change**: it rides an existing root that already goes into
//! the block header verbatim. Update the nation → commit at the next height → `contract_state_root`
//! moves → the chain attests the new governance state.

use sigil_nation::Nation;
use sigil_state::{
    commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation,
    StateRoots, StateTransition,
};

/// The reserved, well-known contract address that holds nation/governance state on-chain.
pub fn gov_contract() -> ContractId {
    let mut id = [0u8; 32];
    let tag = b"sigil:nation:gov";
    id[..tag.len()].copy_from_slice(tag);
    id
}

/// The reserved slot under [`gov_contract`] that holds the current `nation_root`.
pub fn nation_root_slot() -> SlotId {
    let mut s = [0u8; 32];
    let tag = b"nation_root";
    s[..tag.len()].copy_from_slice(tag);
    s
}

/// The mutation that commits `nation`'s root into the governance slot. A block producer drops this
/// into the block's mutation list; it lands in `contract_state_root` like any other contract write.
pub fn nation_root_mutation(nation: &Nation) -> StateMutation {
    StateMutation::SetContractSlot {
        contract: gov_contract(),
        slot: nation_root_slot(),
        value: nation.nation_root(),
    }
}

/// Commit `nation`'s current root into chain `state` at `height` through the real consensus path.
/// Returns the new [`StateRoots`] — `contract_state_root` now carries the nation.
pub fn commit_nation_root(state: &mut SigilState, nation: &Nation, height: u64) -> Result<StateRoots, CommitError> {
    let transition = StateTransition { at_height: height, mutations: vec![nation_root_mutation(nation)] };
    commit_state_transition(state, &transition, height)
}

/// Read the `nation_root` currently committed on-chain (all-zero if none committed yet).
pub fn read_nation_root(state: &SigilState) -> [u8; 32] {
    state.contract_slot(&gov_contract(), &nation_root_slot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_nation::{AcceptNonEmpty, Tier};

    const A: &AcceptNonEmpty = &AcceptNonEmpty;

    fn sample_nation() -> Nation {
        let mut n = Nation::new();
        n.admit("qnk_a", Tier::Gold, b"att", 1, A).unwrap();
        n.admit("qnk_b", Tier::Silver, b"att", 1, A).unwrap();
        n.collect_fee(10_000);
        n
    }

    #[test]
    fn nation_root_lands_in_contract_state_root() {
        let mut state = SigilState::new();
        let empty_contract_root = state.roots().contract_state_root;
        let n = sample_nation();

        let roots = commit_nation_root(&mut state, &n, 1).unwrap();

        // the on-chain slot now holds EXACTLY the nation's root
        assert_eq!(read_nation_root(&state), n.nation_root());
        // and the chain's contract_state_root moved to carry it
        assert_ne!(roots.contract_state_root, empty_contract_root);
        assert_eq!(roots.contract_state_root, state.roots().contract_state_root);
    }

    #[test]
    fn contract_root_tracks_nation_changes() {
        let mut state = SigilState::new();
        let mut n = sample_nation();
        let r1 = commit_nation_root(&mut state, &n, 1).unwrap().contract_state_root;

        // evolve the nation (a new citizen) → its root changes → re-commit at the next height
        n.admit("qnk_c", Tier::Bronze, b"att", 2, A).unwrap();
        let r2 = commit_nation_root(&mut state, &n, 2).unwrap().contract_state_root;

        assert_ne!(r1, r2, "contract_state_root must follow the nation's evolution");
        assert_eq!(read_nation_root(&state), n.nation_root());
    }

    #[test]
    fn wrong_height_is_rejected_by_the_chokepoint() {
        let mut state = SigilState::new();
        let n = sample_nation();
        // committing at the wrong expected height must fail the consensus check
        let transition = StateTransition { at_height: 5, mutations: vec![nation_root_mutation(&n)] };
        assert!(commit_state_transition(&mut state, &transition, 6).is_err());
    }

    #[test]
    fn reserved_addresses_are_stable() {
        // the well-known addresses must never move, or old commitments become unreadable
        assert_eq!(&gov_contract()[..16], b"sigil:nation:gov");
        assert_eq!(&nation_root_slot()[..11], b"nation_root");
    }
}
