//! sigil-vm — deterministic WASM contract VM (skeleton + design lock).
//!
//! The **work engine** of the SIGIL agent economy: a contract execution here
//! is the unit of work that `SettleWork` pays for, and whose correctness the
//! transition-STARK attests. See `SIGIL_AGENT_ECONOMY_v0.md`.
//!
//! ## Why this exists as a clean build (not a q-vm port)
//!
//! q-vm runs WASM on wasmer/wasmtime JIT. JIT is fast but **not deterministic
//! across platforms** — and verifiable execution REQUIRES that two nodes (and
//! a prover) compute the byte-identical `contract_state_root` from the same
//! `(bytecode, input, pre-state)`. So sigil-vm is built on a deterministic
//! interpreter (`wasmi`, added in VM-1). Determinism is the prerequisite for
//! the entire proof story; everything else is downstream of it.
//!
//! ## The execution contract (locked design)
//!
//! ```text
//! execute(bytecode, input, gas_limit, host) -> ExecOutcome {
//!     gas_used,
//!     return_data,
//!     state_writes,   // (slot -> value) deltas, applied through the
//!                     // commit_state_transition chokepoint, never directly
//!     trapped,        // out-of-gas / panic / invalid memory
//! }
//! ```
//!
//! The VM NEVER writes state directly. It collects `state_writes` and hands
//! them back; the chain folds them into a `StateTransition` so the
//! `contract_state_root` advances through the single chokepoint (rule #6).
//! That keeps the VM a pure function of `(bytecode, input, pre-state)` —
//! which is exactly what makes its execution provable.
//!
//! ## Build sequence (swarm-claimable — see SIGIL_AGENT_ECONOMY_v0.md)
//!
//! - **VM-1** wire `wasmi`: real `execute()` over the deterministic interpreter
//! - **VM-2** state host functions (`storage_read`/`storage_write`/`get_caller`/…)
//! - **VM-3** `ContractDeploy` / `ContractCall` tx → sigil-tx → sigil-state
//!   (the tx variants already exist as event-only stubs in sigil-tx)
//! - **VM-4** chronos determinism test: two nodes, same contract → same root
//! - then **prove_execution** (transition STARK + bytecode `.proof` binding)

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// 32-byte contract identifier (matches `sigil_state::ContractId`).
pub type ContractId = [u8; 32];
/// 32-byte storage slot key.
pub type SlotId = [u8; 32];
/// 32-byte slot value.
pub type SlotValue = [u8; 32];
/// 32-byte caller/wallet address.
pub type Address = [u8; 32];

/// Gas meter — deterministic, monotonic. Every WASM instruction costs gas;
/// running out traps the execution (no state changes commit). Gas cost is
/// part of consensus, so the schedule must be fixed + identical on every node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasMeter {
    limit: u64,
    used: u64,
}

impl GasMeter {
    /// New meter with `limit` gas.
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }
    /// Charge `amount` gas. Returns `Err(OutOfGas)` if it would exceed the
    /// limit — the caller must trap the execution.
    pub fn charge(&mut self, amount: u64) -> Result<(), VmError> {
        let next = self.used.saturating_add(amount);
        if next > self.limit {
            self.used = self.limit;
            return Err(VmError::OutOfGas { limit: self.limit });
        }
        self.used = next;
        Ok(())
    }
    /// Gas consumed so far.
    pub fn used(&self) -> u64 {
        self.used
    }
    /// Gas remaining.
    pub fn remaining(&self) -> u64 {
        self.limit - self.used
    }
}

/// The host interface the VM calls into for state + context. The CHAIN
/// implements this; the VM only *reads* via it and *collects* writes (it
/// never mutates chain state directly — writes flow back in [`ExecOutcome`]
/// and through the chokepoint).
pub trait VmHost {
    /// Read a contract storage slot (committed pre-execution state).
    fn storage_read(&self, contract: &ContractId, slot: &SlotId) -> SlotValue;
    /// The caller (the wallet that signed the ContractCall).
    fn caller(&self) -> Address;
    /// The contract being executed.
    fn contract(&self) -> ContractId;
    /// Current block height (deterministic context — never wall-clock).
    fn block_height(&self) -> u64;
}

/// One storage mutation the contract requested. Applied by the chain through
/// `commit_state_transition` → advances `contract_state_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateWrite {
    /// Contract whose storage changed.
    pub contract: ContractId,
    /// Slot written.
    pub slot: SlotId,
    /// New value (all-zero = delete, matching sigil-state semantics).
    pub value: SlotValue,
}

/// Result of executing a contract. Pure function of `(bytecode, input,
/// pre-state)` — which is what makes it provable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// Gas consumed.
    pub gas_used: u64,
    /// Contract return data.
    pub return_data: Vec<u8>,
    /// Storage deltas to fold into the block's StateTransition (ordered,
    /// deterministic).
    pub state_writes: Vec<StateWrite>,
    /// True if the execution trapped (out-of-gas / panic / bad memory). On a
    /// trap, `state_writes` MUST be empty — a trapped call commits nothing.
    pub trapped: bool,
}

/// VM errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    /// Execution exceeded the gas limit.
    #[error("out of gas (limit {limit})")]
    OutOfGas {
        /// The gas limit that was hit.
        limit: u64,
    },
    /// Bytecode failed to load / validate.
    #[error("invalid wasm module: {0}")]
    InvalidModule(String),
    /// The interpreter isn't wired yet (skeleton state — VM-1 removes this).
    #[error("sigil-vm execute() not yet implemented — claim VM-1 to wire wasmi")]
    NotImplemented,
}

/// Execute `bytecode` with `input` under `gas_limit`, reading state via
/// `host`. **Skeleton:** returns [`VmError::NotImplemented`] until VM-1 wires
/// `wasmi`. The signature + the [`ExecOutcome`] shape are the locked contract
/// the rest of the agent-economy stack (prove_execution, SettleWork, AGORA)
/// builds against — so they can be designed in parallel with the engine.
pub fn execute(
    _bytecode: &[u8],
    _input: &[u8],
    gas_limit: u64,
    _host: &dyn VmHost,
) -> Result<ExecOutcome, VmError> {
    let _meter = GasMeter::new(gas_limit);
    // VM-1: load module via wasmi, instantiate with metered host fns, run the
    // exported entrypoint, collect state_writes, return ExecOutcome.
    Err(VmError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullHost;
    impl VmHost for NullHost {
        fn storage_read(&self, _c: &ContractId, _s: &SlotId) -> SlotValue { [0u8; 32] }
        fn caller(&self) -> Address { [0u8; 32] }
        fn contract(&self) -> ContractId { [0u8; 32] }
        fn block_height(&self) -> u64 { 0 }
    }

    #[test]
    fn gas_meter_charges_and_traps() {
        let mut g = GasMeter::new(100);
        g.charge(40).unwrap();
        g.charge(40).unwrap();
        assert_eq!(g.used(), 80);
        assert_eq!(g.remaining(), 20);
        assert_eq!(g.charge(30).unwrap_err(), VmError::OutOfGas { limit: 100 });
        // On trap the meter pins at the limit (deterministic).
        assert_eq!(g.used(), 100);
    }

    #[test]
    fn execute_is_stubbed_until_vm1() {
        let r = execute(b"", b"", 1_000, &NullHost);
        assert_eq!(r.unwrap_err(), VmError::NotImplemented);
    }

    #[test]
    fn state_write_roundtrips() {
        let w = StateWrite { contract: [1; 32], slot: [2; 32], value: [3; 32] };
        let j = serde_json::to_string(&w).unwrap();
        let p: StateWrite = serde_json::from_str(&j).unwrap();
        assert_eq!(w, p);
    }
}
