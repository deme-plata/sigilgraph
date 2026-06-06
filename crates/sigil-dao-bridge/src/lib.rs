//! sigil-dao-bridge — SigilGraph DAO × VM × DEX integration.
//!
//! Wires governance ([`sigil_council`]), treasury ([`sigil_treasury`]),
//! contract execution ([`sigil_vm`]), and AMM math ([`sigil_dex`]) through
//! the single [`sigil_state::commit_state_transition`] chokepoint.
//!
//! Testnet surface: `https://sigilgraph.fluxapp.xyz` (network `sigil-g0`).

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use sigil_council::{Council, Outcome, Risk};
use sigil_dex::{swap, Pool as DexPool, SwapDirection};
use sigil_state::{
    commit_state_transition, CommitError, ContractId, NATIVE, PoolId, PoolState, SigilState,
    SlotId, StateMutation, StateTransition, TokenId, WalletId,
};
use sigil_treasury::Treasury;
use sigil_vm::{execute, ExecOutcome, VmError, VmHost};

pub const VERSION: &str = "0.1.0";
pub const NETWORK_ID: &str = "sigil-g0";
pub const TESTNET_URL: &str = "https://sigilgraph.fluxapp.xyz";
pub const REGISTRY_PATH: &str = "/dao-vm-dex-registry.json";

/// Composite committed root over council governance + treasury.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaoRoots {
    /// Council governance root (XOR accumulator over decided proposals).
    pub gov_root_hex: String,
    /// Treasury committed root.
    pub treasury_root_hex: String,
    /// BLAKE3(council.gov_root || treasury.root).
    pub dao_composite_root_hex: String,
    /// Wallet state root from sigil-state.
    pub wallet_state_root_hex: String,
    /// DEX state root from sigil-state.
    pub dex_state_root_hex: String,
    /// Contract state root from sigil-state.
    pub contract_state_root_hex: String,
}

/// Governance-gated action the bridge can execute after council approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DaoAction {
    /// Constant-product swap routed through sigil-dex math.
    DexSwap {
        proposal_id: u64,
        from: WalletId,
        pool: PoolId,
        in_token: TokenId,
        #[serde(with = "sigil_state::u128_str")]
        in_amt: u128,
        direction: SwapDirectionWire,
        #[serde(with = "sigil_state::u128_str")]
        tx_fee: u128,
    },
    /// Treasury payout — requires MoneyOrConsensus + Passed.
    TreasuryPayout {
        proposal_id: u64,
        #[serde(with = "sigil_state::u128_str")]
        amount: u128,
    },
    /// WASM contract call — scaffold until sigil-vm VM-1 wires wasmi.
    VmContractCall {
        proposal_id: u64,
        caller: WalletId,
        contract: ContractId,
        input_hex: String,
        gas_limit: u64,
    },
}

/// Wire-friendly swap direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapDirectionWire {
    /// Token A → Token B.
    AtoB,
    /// Token B → Token A.
    BtoA,
}

impl From<SwapDirectionWire> for SwapDirection {
    fn from(d: SwapDirectionWire) -> Self {
        match d {
            SwapDirectionWire::AtoB => SwapDirection::AtoB,
            SwapDirectionWire::BtoA => SwapDirection::BtoA,
        }
    }
}

/// Outcome of executing a passed DAO action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeOutcome {
    pub proposal_id: u64,
    pub council_outcome: String,
    pub committed_height: u64,
    pub dao_roots: DaoRoots,
    pub vm_outcome: Option<VmOutcomeWire>,
    pub notes: Vec<String>,
}

/// VM execution summary for MCP / testnet registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmOutcomeWire {
    pub gas_used: u64,
    pub trapped: bool,
    pub state_writes: usize,
    pub status: String,
}

/// SigilGraph integration runtime — council + treasury + chain state.
pub struct SigilGraphBridge {
    council: Council,
    treasury: Treasury,
    state: SigilState,
    height: u64,
}

/// Bridge errors.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("proposal not found: {0}")]
    ProposalNotFound(u64),
    #[error("proposal not passed: {0}")]
    NotPassed(u64),
    #[error("council error: {0:?}")]
    Council(sigil_council::Error),
    #[error("treasury error: {0:?}")]
    Treasury(sigil_treasury::Error),
    #[error("dex error: {0}")]
    Dex(String),
    #[error("vm error: {0}")]
    Vm(#[from] VmError),
    #[error("commit error: {0}")]
    Commit(#[from] CommitError),
    #[error("pool not found")]
    PoolNotFound,
    #[error("insufficient balance")]
    InsufficientBalance,
    #[error("invalid hex: {0}")]
    InvalidHex(String),
}

fn dex_pool_from_state(p: &PoolState) -> DexPool {
    DexPool {
        reserve_a: p.reserve_a,
        reserve_b: p.reserve_b,
        total_shares: p.lp_shares,
        fee_bps: p.fee_bps,
        accrued_fees_a: 0,
        accrued_fees_b: 0,
    }
}

fn pool_state_from_dex(prev: &PoolState, after: &DexPool) -> Result<PoolState, BridgeError> {
    let fees_delta = after
        .accrued_fees_a
        .checked_add(after.accrued_fees_b)
        .ok_or_else(|| BridgeError::Dex("fee overflow".into()))?;
    Ok(PoolState {
        token_a: prev.token_a,
        token_b: prev.token_b,
        reserve_a: after.reserve_a,
        reserve_b: after.reserve_b,
        lp_shares: after.total_shares,
        fee_bps: after.fee_bps,
        accrued_fees: prev
            .accrued_fees
            .checked_add(fees_delta)
            .ok_or_else(|| BridgeError::Dex("accrued fee overflow".into()))?,
    })
}

/// Composite DAO root binding council governance to treasury state.
pub fn dao_composite_root(council: &Council, treasury: &Treasury) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-dao-bridge-v0.1");
    h.update(&council.gov_root());
    h.update(&treasury.root());
    *h.finalize().as_bytes()
}

impl SigilGraphBridge {
    /// New bridge with empty state at height 0.
    pub fn new(total_franchise: u64) -> Self {
        Self {
            council: Council::new(total_franchise),
            treasury: Treasury::new(),
            state: SigilState::new(),
            height: 0,
        }
    }

    /// Genesis bridge with master wallet + native balance seeded via chokepoint.
    pub fn with_genesis_wallet(
        wallet: WalletId,
        initial_native: u128,
        total_franchise: u64,
    ) -> Self {
        let mut s = Self::new(total_franchise);
        let mut st = SigilState::new();
        let tr = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet },
                StateMutation::SetBalance {
                    wallet,
                    token: NATIVE,
                    amount: initial_native,
                },
            ],
        };
        commit_state_transition(&mut st, &tr, 0).expect("genesis");
        s.state = st;
        s.height = 1;
        s
    }

    pub fn council(&self) -> &Council {
        &self.council
    }

    pub fn treasury(&self) -> &Treasury {
        &self.treasury
    }

    pub fn state(&self) -> &SigilState {
        &self.state
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    /// Seed treasury (testnet bootstrap / tests).
    pub fn accrue_treasury(&mut self, amount: u128) {
        self.treasury.accrue(amount);
    }

    pub fn dao_roots(&self) -> DaoRoots {
        let roots = self.state.roots();
        let composite = dao_composite_root(&self.council, &self.treasury);
        DaoRoots {
            gov_root_hex: self.council.gov_root_hex(),
            treasury_root_hex: self.treasury.root_hex(),
            dao_composite_root_hex: hex::encode(composite),
            wallet_state_root_hex: hex::encode(roots.wallet_state_root),
            dex_state_root_hex: hex::encode(roots.dex_state_root),
            contract_state_root_hex: hex::encode(roots.contract_state_root),
        }
    }

    pub fn propose(&mut self, id: u64, title: impl Into<String>, risk: Risk) {
        self.council.propose(id, title, risk);
    }

    pub fn vote(&mut self, id: u64, weight: u64, support: bool) -> Result<(), BridgeError> {
        self.council
            .vote(id, weight, support)
            .map_err(BridgeError::Council)
    }

    pub fn sign(&mut self, id: u64) -> Result<(), BridgeError> {
        self.council.sign(id).map_err(BridgeError::Council)
    }

    pub fn finalize(&mut self, id: u64) -> Result<Outcome, BridgeError> {
        self.council.finalize(id).map_err(BridgeError::Council)
    }

    /// Execute a DAO action after its proposal has Passed.
    pub fn execute_passed_action(
        &mut self,
        action: DaoAction,
    ) -> Result<BridgeOutcome, BridgeError> {
        let proposal_id = match &action {
            DaoAction::DexSwap { proposal_id, .. }
            | DaoAction::TreasuryPayout { proposal_id, .. }
            | DaoAction::VmContractCall { proposal_id, .. } => *proposal_id,
        };
        let p = self
            .council
            .get(proposal_id)
            .ok_or(BridgeError::ProposalNotFound(proposal_id))?;
        if p.outcome != Outcome::Passed {
            return Err(BridgeError::NotPassed(proposal_id));
        }

        let mut notes = Vec::new();
        let mut vm_outcome = None;
        let mut mutations = Vec::new();

        match action {
            DaoAction::TreasuryPayout { proposal_id, amount } => {
                let council_2of2 = p.risk == Risk::MoneyOrConsensus;
                self.treasury
                    .payout(proposal_id, amount, council_2of2)
                    .map_err(BridgeError::Treasury)?;
                notes.push(format!(
                    "treasury payout {amount} via proposal {proposal_id}"
                ));
            }
            DaoAction::DexSwap {
                from,
                pool,
                in_token,
                in_amt,
                direction,
                tx_fee,
                ..
            } => {
                let prev = self
                    .state
                    .pool(&pool)
                    .ok_or(BridgeError::PoolNotFound)?
                    .clone();
                let bal = self.state.balance_of(&from, &in_token);
                let needed = in_amt
                    .checked_add(tx_fee)
                    .ok_or_else(|| BridgeError::Dex("amount overflow".into()))?;
                if bal < needed {
                    return Err(BridgeError::InsufficientBalance);
                }
                let dex_pool = dex_pool_from_state(&prev);
                let dir: SwapDirection = direction.into();
                let out = swap(&dex_pool, dir, in_amt, 0)
                    .map_err(|e| BridgeError::Dex(format!("{e}")))?;
                let pool_after = pool_state_from_dex(&prev, &out.pool_after)?;
                let (out_token, out_amt) = match dir {
                    SwapDirection::AtoB => (prev.token_b, out.amount_out),
                    SwapDirection::BtoA => (prev.token_a, out.amount_out),
                };
                mutations.push(StateMutation::SwapDelta {
                    from,
                    pool,
                    in_token,
                    in_amt,
                    out_token,
                    out_amt,
                    fee: tx_fee,
                    pool_after,
                });
                notes.push(format!("dex swap {in_amt} -> {out_amt} on pool"));
            }
            DaoAction::VmContractCall {
                caller,
                contract,
                input_hex,
                gas_limit,
                ..
            } => {
                let input = hex::decode(input_hex.trim_start_matches("0x"))
                    .map_err(|e| BridgeError::InvalidHex(e.to_string()))?;
                let host = BridgeVmHost {
                    state: &self.state,
                    caller,
                    contract,
                    height: self.height,
                };
                match execute(&[], &input, gas_limit, &host) {
                    Ok(out) => {
                        vm_outcome = Some(vm_outcome_wire(&out));
                        for w in &out.state_writes {
                            mutations.push(StateMutation::SetContractSlot {
                                contract: w.contract,
                                slot: w.slot,
                                value: w.value,
                            });
                        }
                        notes.push(
                            "vm execute returned (VM-1 pending for real wasmi)".into(),
                        );
                    }
                    Err(VmError::NotImplemented) => {
                        vm_outcome = Some(VmOutcomeWire {
                            gas_used: 0,
                            trapped: false,
                            state_writes: 0,
                            status: "scaffolded — sigil-vm VM-1 not wired".into(),
                        });
                        notes.push(
                            "vm scaffold: execute() returns NotImplemented until VM-1"
                                .into(),
                        );
                    }
                    Err(e) => return Err(BridgeError::Vm(e)),
                }
            }
        }

        if !mutations.is_empty() {
            let tr = StateTransition {
                at_height: self.height,
                mutations,
            };
            commit_state_transition(&mut self.state, &tr, self.height)?;
            self.height += 1;
        }

        Ok(BridgeOutcome {
            proposal_id,
            council_outcome: "passed".into(),
            committed_height: self.height,
            dao_roots: self.dao_roots(),
            vm_outcome,
            notes,
        })
    }
}

fn vm_outcome_wire(out: &ExecOutcome) -> VmOutcomeWire {
    VmOutcomeWire {
        gas_used: out.gas_used,
        trapped: out.trapped,
        state_writes: out.state_writes.len(),
        status: if out.trapped { "trapped" } else { "ok" }.into(),
    }
}

struct BridgeVmHost<'a> {
    state: &'a SigilState,
    caller: WalletId,
    contract: ContractId,
    height: u64,
}

impl VmHost for BridgeVmHost<'_> {
    fn storage_read(&self, contract: &ContractId, slot: &SlotId) -> [u8; 32] {
        self.state.contract_slot(contract, slot)
    }
    fn caller(&self) -> WalletId {
        self.caller
    }
    fn contract(&self) -> ContractId {
        self.contract
    }
    fn block_height(&self) -> u64 {
        self.height
    }
}

/// Testnet registry bundle for sigilgraph.fluxapp.xyz.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestnetDaoBundle {
    pub version: String,
    pub network_id: String,
    pub testnet_url: String,
    pub registry_path: String,
    pub honest: HonestStatus,
    pub sample_proposals: Vec<serde_json::Value>,
    pub integration: IntegrationMap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HonestStatus {
    pub live: Vec<String>,
    pub scaffolded: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationMap {
    pub dao: String,
    pub vm: String,
    pub dex: String,
    pub state_chokepoint: String,
    pub agora_stargate: String,
    pub cosmos_citizenship: String,
}

/// Build the testnet DAO/VM/DEX integration bundle.
pub fn testnet_dao_bundle(_deployer_hex: &str) -> TestnetDaoBundle {
    TestnetDaoBundle {
        version: VERSION.into(),
        network_id: NETWORK_ID.into(),
        testnet_url: TESTNET_URL.into(),
        registry_path: REGISTRY_PATH.into(),
        honest: HonestStatus {
            live: vec![
                "sigil-council gov_root commit on finalize".into(),
                "sigil-treasury MAX-WINS + council-gated payout".into(),
                "sigil-dex swap math -> SwapDelta -> commit_state_transition".into(),
                "dao_composite_root (council + treasury BLAKE3)".into(),
            ],
            scaffolded: vec![
                "sigil-vm execute() — VM-1 wasmi wiring pending".into(),
                "sigilgraph.fluxapp.xyz registry JSON publish".into(),
                "flux-agora-stargate ContractDeploy event-only until VM-1".into(),
            ],
        },
        sample_proposals: vec![
            serde_json::json!({
                "id": 1,
                "title": "Bootstrap SIGIL/AGORA pool",
                "risk": "money_or_consensus",
                "action": {"action": "dex_swap", "proposal_id": 1}
            }),
            serde_json::json!({
                "id": 2,
                "title": "Treasury grant for validator ops",
                "risk": "money_or_consensus",
                "action": {"action": "treasury_payout", "proposal_id": 2}
            }),
            serde_json::json!({
                "id": 3,
                "title": "Deploy governance hook contract",
                "risk": "low_risk",
                "action": {"action": "vm_contract_call", "proposal_id": 3}
            }),
        ],
        integration: IntegrationMap {
            dao: "sigil-council + sigil-treasury".into(),
            vm: "sigil-vm (deterministic wasmi, VM-1)".into(),
            dex: "sigil-dex -> sigil-state SwapDelta".into(),
            state_chokepoint: "sigil-state::commit_state_transition".into(),
            agora_stargate: "flux-agora-stargate (provenance + stargate ingest profile)".into(),
            cosmos_citizenship: "sigil-cosmos-core (kappa ritual + flux-nations admission)".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_council::Risk;

    #[test]
    fn dao_composite_root_changes_with_governance() {
        let mut bridge = SigilGraphBridge::new(100);
        let r0 = dao_composite_root(bridge.council(), bridge.treasury());
        bridge.propose(1, "test", Risk::LowRisk);
        bridge.sign(1).unwrap();
        bridge.vote(1, 60, true).unwrap();
        bridge.finalize(1).unwrap();
        let r1 = dao_composite_root(bridge.council(), bridge.treasury());
        assert_ne!(r0, r1);
    }

    #[test]
    fn treasury_payout_requires_passed_proposal() {
        let mut bridge = SigilGraphBridge::new(100);
        bridge.accrue_treasury(1000);
        bridge.propose(1, "grant", Risk::MoneyOrConsensus);
        let err = bridge.execute_passed_action(DaoAction::TreasuryPayout {
            proposal_id: 1,
            amount: 100,
        });
        assert!(matches!(err, Err(BridgeError::NotPassed(1))));
    }

    #[test]
    fn testnet_bundle_has_honest_flags() {
        let b = testnet_dao_bundle("aa".repeat(32).as_str());
        assert_eq!(b.network_id, "sigil-g0");
        assert!(!b.honest.live.is_empty());
        assert!(!b.honest.scaffolded.is_empty());
    }
}