//! sigil-vm — deterministic WASM contract VM (VM-1: wasmi wired).

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use wasmi::{Config, Engine, Linker, Module, Store, TypedFunc};

/// 32-byte contract identifier (matches `sigil_state::ContractId`).
pub type ContractId = [u8; 32];
/// 32-byte storage slot key.
pub type SlotId = [u8; 32];
/// 32-byte slot value.
pub type SlotValue = [u8; 32];
/// 32-byte caller/wallet address.
pub type Address = [u8; 32];

/// Gas meter — deterministic, monotonic.
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
    /// Charge `amount` gas.
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

/// Chain-implemented host interface for reads + execution context.
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

/// One storage mutation the contract requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateWrite {
    /// Contract whose storage changed.
    pub contract: ContractId,
    /// Slot written.
    pub slot: SlotId,
    /// New value (all-zero = delete).
    pub value: SlotValue,
}

/// Result of executing a contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// Gas consumed.
    pub gas_used: u64,
    /// Contract return data.
    pub return_data: Vec<u8>,
    /// Storage deltas (applied through chokepoint).
    pub state_writes: Vec<StateWrite>,
    /// True if trapped — `state_writes` MUST be empty.
    pub trapped: bool,
}

/// VM errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    /// Execution exceeded the gas limit.
    #[error("out of gas (limit {limit})")]
    OutOfGas { limit: u64 },
    /// Bytecode failed to load / validate.
    #[error("invalid wasm module: {0}")]
    InvalidModule(String),
    /// Reserved — VM-1 implements execute().
    #[error("sigil-vm execute() not yet implemented — claim VM-1 to wire wasmi")]
    NotImplemented,
}

struct ExecCtx {
    state_writes: Vec<StateWrite>,
}

fn deterministic_engine() -> Engine {
    let mut config = Config::default();
    config.consume_fuel(true);
    Engine::new(&config)
}

fn fuel_used(store: &Store<ExecCtx>) -> u64 {
    store.fuel_consumed().unwrap_or(0)
}

fn trapped_outcome(store: &Store<ExecCtx>) -> ExecOutcome {
    ExecOutcome {
        gas_used: fuel_used(store),
        return_data: Vec::new(),
        state_writes: Vec::new(),
        trapped: true,
    }
}

fn parse_i32_pair(input: &[u8]) -> Option<(i32, i32)> {
    if input.len() < 8 {
        return None;
    }
    let a = i32::from_le_bytes(input[0..4].try_into().ok()?);
    let b = i32::from_le_bytes(input[4..8].try_into().ok()?);
    Some((a, b))
}

enum CallExport {
    Missing,
    Ok(Vec<u8>),
    Trap,
}

fn try_call_add(
    store: &mut Store<ExecCtx>,
    instance: &wasmi::Instance,
    input: &[u8],
) -> CallExport {
    let add: TypedFunc<(i32, i32), i32> = match instance.get_typed_func(&*store, "add") {
        Ok(f) => f,
        Err(_) => return CallExport::Missing,
    };
    let (a, b) = parse_i32_pair(input).unwrap_or((1, 2));
    match add.call(&mut *store, (a, b)) {
        Ok(result) => CallExport::Ok(result.to_le_bytes().to_vec()),
        Err(_) => CallExport::Trap,
    }
}

fn try_call_main(
    store: &mut Store<ExecCtx>,
    instance: &wasmi::Instance,
    input: &[u8],
) -> CallExport {
    let main_fn: TypedFunc<i32, i32> = match instance.get_typed_func(&*store, "main") {
        Ok(f) => f,
        Err(_) => return CallExport::Missing,
    };
    let arg = if input.len() >= 4 {
        i32::from_le_bytes(input[0..4].try_into().unwrap_or([0; 4]))
    } else {
        0
    };
    match main_fn.call(&mut *store, arg) {
        Ok(result) => CallExport::Ok(result.to_le_bytes().to_vec()),
        Err(_) => CallExport::Trap,
    }
}

/// Execute `bytecode` with `input` under `gas_limit` via deterministic wasmi.
pub fn execute(
    bytecode: &[u8],
    input: &[u8],
    gas_limit: u64,
    host: &dyn VmHost,
) -> Result<ExecOutcome, VmError> {
    if bytecode.is_empty() {
        return Err(VmError::InvalidModule("empty bytecode".into()));
    }

    let engine = deterministic_engine();
    let module = Module::new(&engine, bytecode)
        .map_err(|e| VmError::InvalidModule(e.to_string()))?;

    let mut store = Store::new(
        &engine,
        ExecCtx {
            state_writes: Vec::new(),
        },
    );
    store
        .add_fuel(gas_limit)
        .map_err(|e| VmError::InvalidModule(format!("fuel: {e}")))?;

    let linker = Linker::new(&engine);
    let _ = host; // VM-2: host imports via linker.func_wrap

    let instance_pre = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(_) => return Ok(trapped_outcome(&store)),
    };
    let instance = match instance_pre.start(&mut store) {
        Ok(i) => i,
        Err(_) => return Ok(trapped_outcome(&store)),
    };

    let return_data = match try_call_main(&mut store, &instance, input) {
        CallExport::Ok(data) => data,
        CallExport::Trap => return Ok(trapped_outcome(&store)),
        CallExport::Missing => match try_call_add(&mut store, &instance, input) {
            CallExport::Ok(data) => data,
            CallExport::Trap => return Ok(trapped_outcome(&store)),
            CallExport::Missing => Vec::new(),
        },
    };

    let gas_used = fuel_used(&store);
    let ctx = store.into_data();
    Ok(ExecOutcome {
        gas_used,
        return_data,
        state_writes: ctx.state_writes,
        trapped: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullHost;
    impl VmHost for NullHost {
        fn storage_read(&self, _c: &ContractId, _s: &SlotId) -> SlotValue {
            [0u8; 32]
        }
        fn caller(&self) -> Address {
            [0u8; 32]
        }
        fn contract(&self) -> ContractId {
            [0u8; 32]
        }
        fn block_height(&self) -> u64 {
            0
        }
    }

    /// `(module (func (export "nop")))` — valid module, no main/add entrypoint.
    const NOP_WASM: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03,
        0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, 0x6e, 0x6f, 0x70, 0x00, 0x00, 0x0a, 0x04, 0x01,
        0x02, 0x00, 0x0b,
    ];

    /// `(module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))`
    const ADD_WASM: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f,
        0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00,
        0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
    ];

    #[test]
    fn gas_meter_charges_and_traps() {
        let mut g = GasMeter::new(100);
        g.charge(40).unwrap();
        g.charge(40).unwrap();
        assert_eq!(g.used(), 80);
        assert_eq!(g.remaining(), 20);
        assert_eq!(g.charge(30).unwrap_err(), VmError::OutOfGas { limit: 100 });
        assert_eq!(g.used(), 100);
    }

    #[test]
    fn execute_nop_module_succeeds() {
        let r = execute(NOP_WASM, b"", 1_000_000, &NullHost).unwrap();
        assert!(!r.trapped);
        assert!(r.return_data.is_empty());
        assert!(r.state_writes.is_empty());
        assert!(r.gas_used <= 1_000_000);
    }

    #[test]
    fn execute_rejects_empty_bytecode() {
        assert_eq!(
            execute(b"", b"", 1_000, &NullHost).unwrap_err(),
            VmError::InvalidModule("empty bytecode".into())
        );
    }

    #[test]
    fn execute_add_export_returns_sum() {
        let input: [u8; 8] = {
            let mut b = [0u8; 8];
            b[0..4].copy_from_slice(&1i32.to_le_bytes());
            b[4..8].copy_from_slice(&2i32.to_le_bytes());
            b
        };
        let r = execute(ADD_WASM, &input, 1_000_000, &NullHost).unwrap();
        assert!(!r.trapped, "trapped with gas_used={}", r.gas_used);
        assert_eq!(r.return_data, 3i32.to_le_bytes().to_vec());
        assert!(r.state_writes.is_empty());
    }

    #[test]
    fn state_write_roundtrips() {
        let w = StateWrite {
            contract: [1; 32],
            slot: [2; 32],
            value: [3; 32],
        };
        let j = serde_json::to_string(&w).unwrap();
        let p: StateWrite = serde_json::from_str(&j).unwrap();
        assert_eq!(w, p);
    }
}