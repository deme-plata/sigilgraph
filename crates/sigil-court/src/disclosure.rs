//! DISCLOSURE — the one lawful way records leave the nation.
//!
//! A foreign authority (a tax office, a regulator, another government) asks for records. It gets
//! them only as a court-sealed [`DisclosurePacket`]: the least that answers the purpose (Art. IV),
//! every record carrying an inclusion proof to its block's `event_log_root` (Art. V), redactions
//! committed by the very leaf they hide behind, a deterministic summary the recipient can recompute,
//! and a SQIsign seal over the packet root, the constitution hash and the court root at issue.
//! Optionally a VIEWING key grant (sight, no authority). A spend key is not a thing this module
//! can carry (Art. III) — there is no variant for it.
//!
//! What is PROVEN vs what is ATTESTED, stated plainly: inclusion of each `leaf` under the block
//! root is proven by the Merkle proof; that the disclosed cleartext `fields` correspond to that
//! leaf is proven only when `full_event` is carried (Purpose::CourtOrder), otherwise it is
//! attested by the court's seal. [`verify_packet`] reports both counts separately.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sigil_events::{u128_str, MerkleProof, SigilEvent};
use sigil_state::{TokenId, WalletId, NATIVE};

use crate::constitution::{constitution_hash, Article};
use crate::docket::{CaseId, OrderId};
use crate::merkle;

pub const PACKET_SCHEMA: &str = "sigil-court/disclosure/v3";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Purpose {
    /// Tax reporting: amounts, fees, tokens, direction; counterparties pseudonymized.
    Tax,
    /// Anti-money-laundering: counterparties in clear.
    Aml,
    /// A court order in a decided case: everything in scope, full events carried.
    CourtOrder,
    /// Audit (e.g. the bank's own books): amounts in clear, counterparties pseudonymized.
    Audit,
}

/// What kind of key an authority asks for. The court can grant sight (Viewing). The court cannot
/// grant authority (Spend) — asking for it is refused under Art. III before anything else happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyKind {
    Viewing,
    Spend,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jurisdiction {
    /// ISO 3166-1 alpha-2 country code ("DK").
    pub country: String,
    /// The authority within it ("SKAT").
    pub authority: String,
}

impl Jurisdiction {
    pub fn new(country: impl Into<String>, authority: impl Into<String>) -> Self {
        Self { country: country.into(), authority: authority.into() }
    }
    pub fn label(&self) -> String { format!("{}/{}", self.country, self.authority) }
}

/// Which records. Empty `subjects` = every wallet (used for institution-level exports such as the
/// bank's books); empty `kinds` = every kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub subjects: BTreeSet<WalletId>,
    pub kinds: BTreeSet<u8>,
    pub from_height: u64,
    pub to_height: u64,
    pub tokens: Option<BTreeSet<TokenId>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosureRequest {
    pub jurisdiction: Jurisdiction,
    pub purpose: Purpose,
    pub scope: Scope,
    pub key_kind: Option<KeyKind>,
    pub reason: String,
    pub requested_ts: u64,
    pub ttl_secs: u64,
}

/// How much is shown. Derived from the purpose (Art. IV), recorded on the order so it is auditable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Minimization {
    pub reveal_counterparties: bool,
    pub reveal_amounts: bool,
    pub carry_full_event: bool,
}

impl Minimization {
    pub fn for_purpose(p: Purpose) -> Self {
        match p {
            Purpose::Tax => Self { reveal_counterparties: false, reveal_amounts: true, carry_full_event: false },
            Purpose::Audit => Self { reveal_counterparties: false, reveal_amounts: true, carry_full_event: false },
            Purpose::Aml => Self { reveal_counterparties: true, reveal_amounts: true, carry_full_event: false },
            Purpose::CourtOrder => Self { reveal_counterparties: true, reveal_amounts: true, carry_full_event: true },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosureOrder {
    pub id: OrderId,
    pub request: DisclosureRequest,
    pub case: Option<CaseId>,
    pub issued_height: u64,
    pub issued_ts: u64,
    pub expires_ts: u64,
    pub panel: Vec<WalletId>,
    pub votes_for: u32,
    pub minimization: Minimization,
}

impl DisclosureOrder {
    pub fn is_live(&self, now_ts: u64) -> bool { now_ts < self.expires_ts }
}

pub fn order_id(request: &DisclosureRequest, issued_height: u64, issued_ts: u64) -> OrderId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-court/order");
    h.update(&serde_json::to_vec(request).unwrap_or_default());
    h.update(&issued_height.to_le_bytes());
    h.update(&issued_ts.to_le_bytes());
    *h.finalize().as_bytes()
}

/// BLAKE3 over the WHOLE issued order — its terms, not just its identity.
///
/// 🪤 [`order_id`] covers only the *request* (who asked, for what, over which range). It does not
/// cover the terms the court attached when it granted it: the [`Minimization`] level, the panel,
/// the vote, the case, the issue height. Sealing the id alone therefore left those terms
/// unauthenticated — an intermediary could flip `reveal_counterparties` to `true` and a reader
/// would believe the court had permitted counterparties in clear. Found by
/// [`crate::science::tamper_sweep`], which is exactly what that sweep is for.
pub fn order_digest(o: &DisclosureOrder) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-court/order-digest/v3");
    // Every field of DisclosureOrder is either a scalar, a string, a sequence or a struct — no
    // byte-array map keys — so a canonical JSON encoding is well-defined here. It must never be
    // allowed to degrade to empty: an unencodable order is a bug, not a runtime condition.
    let bytes = serde_json::to_vec(o).expect("DisclosureOrder is JSON-encodable");
    assert!(!bytes.is_empty());
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(&bytes);
    *h.finalize().as_bytes()
}

/// One block's events as the exporter receives them from the chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEvents {
    pub height: u64,
    pub event_log_root: [u8; 32],
    pub events: Vec<SigilEvent>,
}

impl BlockEvents {
    /// Build with the root computed by the chain's own rule (for tests and for producers).
    pub fn seal(height: u64, events: Vec<SigilEvent>) -> Self {
        let leaves: Vec<[u8; 32]> = events.iter().map(|e| e.leaf_hash()).collect();
        Self { height, event_log_root: merkle::root(&leaves), events }
    }
}

/// One disclosed record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosedRecord {
    pub height: u64,
    pub index: u32,
    pub kind: String,
    pub tag: u8,
    /// The subject's role in the event ("sender", "recipient", "miner", …; "any" when the scope
    /// has no subjects).
    pub role: String,
    /// Cleartext fields after minimization. Wallets are 64-hex; amounts are decimal strings in base units.
    pub fields: BTreeMap<String, String>,
    /// Names of fields that were pseudonymized or withheld. Their cleartext is committed by `leaf`.
    pub redacted: Vec<String>,
    pub leaf: [u8; 32],
    pub proof: MerkleProof,
    pub full_event: Option<SigilEvent>,
}

impl DisclosedRecord {
    pub fn record_leaf(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/record");
        h.update(&serde_json::to_vec(self).unwrap_or_default());
        *h.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    #[serde(with = "u128_str")] pub received: u128,
    #[serde(with = "u128_str")] pub sent: u128,
    #[serde(with = "u128_str")] pub fees: u128,
    #[serde(with = "u128_str")] pub mined: u128,
    #[serde(with = "u128_str")] pub welfare: u128,
    #[serde(with = "u128_str")] pub bank_in: u128,
    #[serde(with = "u128_str")] pub bank_out: u128,
}

/// The clean output: totals a tax office actually wants, recomputable from the records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub records: u32,
    pub redacted_fields: u32,
    pub subjects: u32,
    pub from_height: u64,
    pub to_height: u64,
    pub first_height: Option<u64>,
    pub last_height: Option<u64>,
    /// token hex → totals
    pub per_token: BTreeMap<String, TokenTotals>,
    pub per_kind: BTreeMap<String, u32>,
}

/// Sight without authority (see `sigil_shield::viewing`). Only ever a viewing key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewingGrant {
    pub subject: String,
    pub viewing_key_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealBody {
    pub schema: String,
    pub order_id: [u8; 32],
    /// Commitment to the order's full TERMS (see [`order_digest`]) — the minimization level, the
    /// panel, the vote. Without this the seal authenticates who asked but not what was granted.
    pub order_digest: [u8; 32],
    pub packet_root: [u8; 32],
    pub court_root: [u8; 32],
    pub constitution_hash: [u8; 32],
    pub issued_ts: u64,
    pub expires_ts: u64,
    pub jurisdiction: String,
    pub purpose: Purpose,
    pub records: u32,
    pub redacted_fields: u32,
    pub viewing_grants: u32,
}

impl SealBody {
    pub fn signing_bytes(&self) -> Vec<u8> { serde_json::to_vec(self).unwrap_or_default() }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seal {
    pub body: SealBody,
    /// SQIsign public key of the court (129 bytes), hex.
    pub court_pubkey_hex: String,
    /// SQIsign signature over `body.signing_bytes()` (292 bytes), hex.
    pub sig_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosurePacket {
    pub schema: String,
    pub order: DisclosureOrder,
    pub records: Vec<DisclosedRecord>,
    pub summary: Summary,
    pub viewing_grants: Vec<ViewingGrant>,
    pub seal: Seal,
}

impl DisclosurePacket {
    pub fn packet_root(records: &[DisclosedRecord]) -> [u8; 32] {
        let leaves: Vec<[u8; 32]> = records.iter().map(|r| r.record_leaf()).collect();
        merkle::root(&leaves)
    }
    pub fn to_json(&self) -> String { serde_json::to_string_pretty(self).unwrap_or_default() }

    /// The tax office's flat file: one row per record.
    pub fn tax_csv(&self) -> String {
        let mut s = String::from("height,index,kind,role,token,amount,fee,counterparty,leaf\n");
        for r in &self.records {
            let g = |k: &str| r.fields.get(k).cloned().unwrap_or_default();
            let counterparty = match r.role.as_str() {
                "sender" | "treasury" => g("to"),
                "recipient" => g("from"),
                _ => String::new(),
            };
            s.push_str(&format!("{},{},{},{},{},{},{},{},{}\n", r.height, r.index, r.kind, r.role, g("token"), g("amount"), g("fee"), counterparty, hex::encode(r.leaf)));
        }
        s
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DisclosureError {
    #[error("{article}: {reason}")]
    Unconstitutional { article: Article, reason: String },
    #[error("Art. XI: order expired at {expires_ts}, now {now}")]
    Expired { expires_ts: u64, now: u64 },
    #[error("order was revoked")]
    Revoked,
    #[error("block {height}: supplied event_log_root does not match its events")]
    RootMismatch { height: u64 },
    #[error("block {height}: no event_log_root known to the verifier")]
    RootUnknown { height: u64 },
    #[error("seal invalid: {0}")]
    BadSeal(String),
    #[error("the order's terms were altered after it was sealed")]
    OrderTermsAltered,
    #[error("packet schema is not {PACKET_SCHEMA}")]
    SchemaMismatch,
    #[error("packet root does not match its records")]
    PacketRootMismatch,
    #[error("record at height {height} index {index} is not proven under the block root")]
    RecordNotProven { height: u64, index: u32 },
    #[error("record at height {height} index {index}: full_event leaf != record leaf")]
    LeafMismatch { height: u64, index: u32 },
    #[error("summary does not recompute from the records")]
    SummaryMismatch,
    #[error("scope is empty: to_height < from_height")]
    EmptyScope,
    #[error("viewing key offered for {0}, which is not a subject of the order")]
    GrantForNonSubject(String),
}

/// Which token an event moves (None for kinds without one).
pub fn event_token(e: &SigilEvent) -> Option<TokenId> {
    Some(match e {
        SigilEvent::Send { token, .. } | SigilEvent::Receive { token, .. } => *token,
        SigilEvent::BankProposed { token, .. } | SigilEvent::BankExecuted { token, .. } => *token,
        SigilEvent::SwapExecuted { in_token, .. } => *in_token,
        SigilEvent::MintReward { .. } | SigilEvent::WelfareClaimed { .. } | SigilEvent::UsdsMinted { .. } | SigilEvent::UsdsRedeemed { .. } | SigilEvent::MandateCreated { .. } => NATIVE,
        _ => return None,
    })
}

/// The subject's role in `e`, if the subject is a party to it.
pub fn involvement(e: &SigilEvent, s: &WalletId) -> Option<&'static str> {
    let eq = |w: &WalletId| w == s;
    match e {
        SigilEvent::Send { from, to, .. } | SigilEvent::Receive { from, to, .. } => {
            if eq(from) { Some("sender") } else if eq(to) { Some("recipient") } else { None }
        }
        SigilEvent::MintReward { miner, .. } => eq(miner).then_some("miner"),
        SigilEvent::WelfareClaimed { citizen, .. } => eq(citizen).then_some("citizen"),
        SigilEvent::BankExecuted { from, to, .. } => {
            if eq(from) { Some("treasury") } else if eq(to) { Some("recipient") } else { None }
        }
        SigilEvent::BankProposed { from, to, proposer, .. } => {
            if eq(from) { Some("treasury") } else if eq(to) { Some("recipient") } else if eq(proposer) { Some("proposer") } else { None }
        }
        SigilEvent::BankApproved { approver, .. } => eq(approver).then_some("approver"),
        SigilEvent::MandateCreated { agent, .. } | SigilEvent::MandateClosed { agent, .. } => eq(agent).then_some("agent"),
        SigilEvent::UsdsMinted { wallet, .. } | SigilEvent::UsdsRedeemed { wallet, .. } => eq(wallet).then_some("vault-user"),
        SigilEvent::TokenDeployed { creator, .. } | SigilEvent::ContractDeploy { creator, .. } => eq(creator).then_some("creator"),
        SigilEvent::ValidatorJoined { validator, .. } | SigilEvent::ValidatorLeft { validator, .. } => eq(validator).then_some("validator"),
        SigilEvent::HonorConferred { recipient, conferred_by, .. } => {
            if eq(recipient) { Some("honoree") } else if eq(conferred_by) { Some("conferrer") } else { None }
        }
        SigilEvent::CitizenAttested { citizen, authority } => {
            if eq(citizen) { Some("citizen") } else if eq(authority) { Some("authority") } else { None }
        }
        SigilEvent::SwapExecuted { .. } | SigilEvent::LpDeposited { .. } | SigilEvent::LpWithdrawn { .. } | SigilEvent::ContractCall { .. } | SigilEvent::ShieldedSend { .. } => None,
    }
}

fn kind_name(e: &SigilEvent) -> &'static str {
    match e {
        SigilEvent::Send { .. } => "Send",
        SigilEvent::Receive { .. } => "Receive",
        SigilEvent::SwapExecuted { .. } => "SwapExecuted",
        SigilEvent::LpDeposited { .. } => "LpDeposited",
        SigilEvent::LpWithdrawn { .. } => "LpWithdrawn",
        SigilEvent::ContractCall { .. } => "ContractCall",
        SigilEvent::ContractDeploy { .. } => "ContractDeploy",
        SigilEvent::MintReward { .. } => "MintReward",
        SigilEvent::TokenDeployed { .. } => "TokenDeployed",
        SigilEvent::ValidatorJoined { .. } => "ValidatorJoined",
        SigilEvent::ValidatorLeft { .. } => "ValidatorLeft",
        SigilEvent::ShieldedSend { .. } => "ShieldedSend",
        SigilEvent::HonorConferred { .. } => "HonorConferred",
        SigilEvent::UsdsMinted { .. } => "UsdsMinted",
        SigilEvent::UsdsRedeemed { .. } => "UsdsRedeemed",
        SigilEvent::MandateCreated { .. } => "MandateCreated",
        SigilEvent::MandateClosed { .. } => "MandateClosed",
        SigilEvent::BankProposed { .. } => "BankProposed",
        SigilEvent::BankApproved { .. } => "BankApproved",
        SigilEvent::BankExecuted { .. } => "BankExecuted",
        SigilEvent::CitizenAttested { .. } => "CitizenAttested",
        SigilEvent::WelfareClaimed { .. } => "WelfareClaimed",
    }
}

/// Full cleartext field map. Wallet fields are named so minimization can find them.
fn fields_of(e: &SigilEvent) -> (BTreeMap<String, String>, Vec<&'static str>, Vec<&'static str>) {
    let mut f = BTreeMap::new();
    let hx = |b: &[u8]| hex::encode(b);
    // (fields, wallet-field-names, amount-field-names)
    let (wallets, amounts): (Vec<&'static str>, Vec<&'static str>) = match e {
        SigilEvent::Send { from, to, amount, token, fee } => {
            f.insert("from".into(), hx(from)); f.insert("to".into(), hx(to)); f.insert("amount".into(), amount.to_string()); f.insert("token".into(), hx(token)); f.insert("fee".into(), fee.to_string());
            (vec!["from", "to"], vec!["amount", "fee"])
        }
        SigilEvent::Receive { from, to, amount, token } => {
            f.insert("from".into(), hx(from)); f.insert("to".into(), hx(to)); f.insert("amount".into(), amount.to_string()); f.insert("token".into(), hx(token));
            (vec!["from", "to"], vec!["amount"])
        }
        SigilEvent::MintReward { miner, height, amount } => {
            f.insert("miner".into(), hx(miner)); f.insert("mined_at".into(), height.to_string()); f.insert("amount".into(), amount.to_string()); f.insert("token".into(), hx(&NATIVE));
            (vec!["miner"], vec!["amount"])
        }
        SigilEvent::WelfareClaimed { citizen, amount } => {
            f.insert("citizen".into(), hx(citizen)); f.insert("amount".into(), amount.to_string()); f.insert("token".into(), hx(&NATIVE));
            (vec!["citizen"], vec!["amount"])
        }
        SigilEvent::BankExecuted { id, from, to, token, amount } => {
            f.insert("proposal".into(), id.clone()); f.insert("from".into(), hx(from)); f.insert("to".into(), hx(to)); f.insert("token".into(), hx(token)); f.insert("amount".into(), amount.to_string());
            (vec!["from", "to"], vec!["amount"])
        }
        SigilEvent::BankProposed { id, from, to, token, amount, proposer, created_ts } => {
            f.insert("proposal".into(), id.clone()); f.insert("from".into(), hx(from)); f.insert("to".into(), hx(to)); f.insert("token".into(), hx(token)); f.insert("amount".into(), amount.to_string()); f.insert("proposer".into(), hx(proposer)); f.insert("created_ts".into(), created_ts.to_string());
            (vec!["from", "to", "proposer"], vec!["amount"])
        }
        SigilEvent::BankApproved { id, approver } => {
            f.insert("proposal".into(), id.clone()); f.insert("approver".into(), hx(approver));
            (vec!["approver"], vec![])
        }
        SigilEvent::MandateCreated { id, agent, max_amount, purpose, created_ts, expires_ts } => {
            f.insert("mandate".into(), id.clone()); f.insert("agent".into(), hx(agent)); f.insert("amount".into(), max_amount.to_string()); f.insert("purpose".into(), purpose.clone()); f.insert("created_ts".into(), created_ts.to_string()); f.insert("expires_ts".into(), expires_ts.to_string()); f.insert("token".into(), hx(&NATIVE));
            (vec!["agent"], vec!["amount"])
        }
        SigilEvent::MandateClosed { id, agent } => {
            f.insert("mandate".into(), id.clone()); f.insert("agent".into(), hx(agent));
            (vec!["agent"], vec![])
        }
        SigilEvent::SwapExecuted { pool, in_token, in_amt, out_token, out_amt, slippage_bps, fee_paid } => {
            f.insert("pool".into(), hx(pool)); f.insert("in_token".into(), hx(in_token)); f.insert("in_amount".into(), in_amt.to_string()); f.insert("out_token".into(), hx(out_token)); f.insert("out_amount".into(), out_amt.to_string()); f.insert("slippage_bps".into(), slippage_bps.to_string()); f.insert("fee".into(), fee_paid.to_string()); f.insert("token".into(), hx(in_token));
            (vec![], vec!["in_amount", "out_amount", "fee"])
        }
        SigilEvent::UsdsMinted { wallet, sigil_locked, usds_minted } => {
            f.insert("wallet".into(), hx(wallet)); f.insert("sigil_locked".into(), sigil_locked.to_string()); f.insert("usds_minted".into(), usds_minted.to_string()); f.insert("token".into(), hx(&NATIVE));
            (vec!["wallet"], vec!["sigil_locked", "usds_minted"])
        }
        SigilEvent::UsdsRedeemed { wallet, usds_burned, sigil_released } => {
            f.insert("wallet".into(), hx(wallet)); f.insert("usds_burned".into(), usds_burned.to_string()); f.insert("sigil_released".into(), sigil_released.to_string()); f.insert("token".into(), hx(&NATIVE));
            (vec!["wallet"], vec!["usds_burned", "sigil_released"])
        }
        SigilEvent::HonorConferred { order, rank, recipient, citation, conferred_by } => {
            f.insert("order".into(), format!("{order:?}")); f.insert("rank".into(), rank.clone()); f.insert("recipient".into(), hx(recipient)); f.insert("citation".into(), citation.clone()); f.insert("conferred_by".into(), hx(conferred_by));
            (vec!["recipient", "conferred_by"], vec![])
        }
        SigilEvent::CitizenAttested { authority, citizen } => {
            f.insert("authority".into(), hx(authority)); f.insert("citizen".into(), hx(citizen));
            (vec!["authority", "citizen"], vec![])
        }
        SigilEvent::TokenDeployed { creator, ticker, decimals, initial_supply } => {
            f.insert("creator".into(), hx(creator)); f.insert("ticker".into(), ticker.clone()); f.insert("decimals".into(), decimals.to_string()); f.insert("initial_supply".into(), initial_supply.to_string());
            (vec!["creator"], vec!["initial_supply"])
        }
        SigilEvent::ValidatorJoined { validator, stake } => {
            f.insert("validator".into(), hx(validator)); f.insert("stake".into(), stake.to_string());
            (vec!["validator"], vec!["stake"])
        }
        SigilEvent::ValidatorLeft { validator, refunded_stake } => {
            f.insert("validator".into(), hx(validator)); f.insert("refunded_stake".into(), refunded_stake.to_string());
            (vec!["validator"], vec!["refunded_stake"])
        }
        SigilEvent::ContractDeploy { creator, contract_id, bytecode_hash, gas_used } => {
            f.insert("creator".into(), hx(creator)); f.insert("contract".into(), hx(contract_id)); f.insert("bytecode_hash".into(), hx(bytecode_hash)); f.insert("gas_used".into(), gas_used.to_string());
            (vec!["creator"], vec![])
        }
        SigilEvent::ContractCall { contract, method, gas_used, result_hash } => {
            f.insert("contract".into(), hx(contract)); f.insert("method".into(), hx(method)); f.insert("gas_used".into(), gas_used.to_string()); f.insert("result_hash".into(), hx(result_hash));
            (vec![], vec![])
        }
        SigilEvent::LpDeposited { pool, amt_a, amt_b, shares_received } => {
            f.insert("pool".into(), hx(pool)); f.insert("amount_a".into(), amt_a.to_string()); f.insert("amount_b".into(), amt_b.to_string()); f.insert("shares".into(), shares_received.to_string());
            (vec![], vec!["amount_a", "amount_b", "shares"])
        }
        SigilEvent::LpWithdrawn { pool, shares_burned, amt_a, amt_b, fees_realized } => {
            f.insert("pool".into(), hx(pool)); f.insert("shares".into(), shares_burned.to_string()); f.insert("amount_a".into(), amt_a.to_string()); f.insert("amount_b".into(), amt_b.to_string()); f.insert("fee".into(), fees_realized.to_string());
            (vec![], vec!["amount_a", "amount_b", "shares", "fee"])
        }
        SigilEvent::ShieldedSend { token_hint, fee, n_inputs, n_outputs, proof_digest, pool_root_at_proof } => {
            f.insert("token".into(), hx(token_hint)); f.insert("fee".into(), fee.to_string()); f.insert("n_inputs".into(), n_inputs.to_string()); f.insert("n_outputs".into(), n_outputs.to_string()); f.insert("proof_digest".into(), hx(proof_digest)); f.insert("pool_root".into(), hx(pool_root_at_proof));
            (vec![], vec!["fee"])
        }
    };
    (f, wallets, amounts)
}

/// Pseudonym for a counterparty: stable within one order, unlinkable across orders.
pub fn pseudonym(order: &OrderId, wallet_hex: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-court/pseudonym");
    h.update(order);
    h.update(wallet_hex.as_bytes());
    format!("pseud:{}", hex::encode(&h.finalize().as_bytes()[..12]))
}

fn scope_ok(scope: &Scope, e: &SigilEvent, height: u64) -> Option<&'static str> {
    if height < scope.from_height || height > scope.to_height { return None; }
    if !scope.kinds.is_empty() && !scope.kinds.contains(&e.tag()) { return None; }
    if let Some(tokens) = &scope.tokens {
        match event_token(e) {
            Some(t) if tokens.contains(&t) => {}
            _ => return None,
        }
    }
    if scope.subjects.is_empty() { return Some("any"); }
    scope.subjects.iter().find_map(|s| involvement(e, s))
}

/// Build a record from an event under the order's minimization.
fn minimize(order: &DisclosureOrder, e: &SigilEvent, height: u64, index: u32, proof: MerkleProof, role: &'static str) -> DisclosedRecord {
    let (mut fields, wallet_fields, amount_fields) = fields_of(e);
    let mut redacted = Vec::new();
    let subjects_hex: BTreeSet<String> = order.request.scope.subjects.iter().map(hex::encode).collect();
    if !order.minimization.reveal_counterparties {
        for wf in wallet_fields {
            if let Some(v) = fields.get(wf).cloned() {
                if !subjects_hex.contains(&v) {
                    fields.insert(wf.to_string(), pseudonym(&order.id, &v));
                    redacted.push(wf.to_string());
                }
            }
        }
    }
    if !order.minimization.reveal_amounts {
        for af in amount_fields {
            if fields.remove(af).is_some() { redacted.push(af.to_string()); }
        }
    }
    DisclosedRecord {
        height,
        index,
        kind: kind_name(e).into(),
        tag: e.tag(),
        role: role.into(),
        fields,
        redacted,
        leaf: e.leaf_hash(),
        proof,
        full_event: order.minimization.carry_full_event.then(|| e.clone()),
    }
}

/// Recompute the summary from records — the recipient runs the same function.
pub fn summarize(order: &DisclosureOrder, records: &[DisclosedRecord]) -> Summary {
    let mut s = Summary {
        records: records.len() as u32,
        redacted_fields: records.iter().map(|r| r.redacted.len() as u32).sum(),
        subjects: order.request.scope.subjects.len() as u32,
        from_height: order.request.scope.from_height,
        to_height: order.request.scope.to_height,
        first_height: records.first().map(|r| r.height),
        last_height: records.last().map(|r| r.height),
        ..Default::default()
    };
    let amt = |r: &DisclosedRecord, k: &str| r.fields.get(k).and_then(|v| v.parse::<u128>().ok()).unwrap_or(0);
    for r in records {
        *s.per_kind.entry(r.kind.clone()).or_insert(0) += 1;
        let token = r.fields.get("token").cloned().unwrap_or_else(|| hex::encode(NATIVE));
        let t = s.per_token.entry(token).or_default();
        match (r.kind.as_str(), r.role.as_str()) {
            ("Send", "sender") | ("Send", "any") => { t.sent = t.sent.saturating_add(amt(r, "amount")); t.fees = t.fees.saturating_add(amt(r, "fee")); }
            ("Send", "recipient") | ("Receive", "recipient") | ("Receive", "any") => { t.received = t.received.saturating_add(amt(r, "amount")); }
            ("Receive", "sender") => { t.sent = t.sent.saturating_add(amt(r, "amount")); }
            ("MintReward", _) => { t.mined = t.mined.saturating_add(amt(r, "amount")); }
            ("WelfareClaimed", _) => { t.welfare = t.welfare.saturating_add(amt(r, "amount")); }
            ("BankExecuted", "recipient") => { t.bank_in = t.bank_in.saturating_add(amt(r, "amount")); }
            ("BankExecuted", _) => { t.bank_out = t.bank_out.saturating_add(amt(r, "amount")); }
            ("SwapExecuted", _) => { t.sent = t.sent.saturating_add(amt(r, "in_amount")); t.fees = t.fees.saturating_add(amt(r, "fee")); }
            _ => {}
        }
    }
    s
}

/// Court signing material (SQIsign L5). The court holds one; verifiers hold the public half.
#[derive(Clone)]
pub struct CourtKeys {
    pub sk: Vec<u8>,
    pub pk: Vec<u8>,
}

impl CourtKeys {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let (sk, pk) = flux_sqisign::keygen_from_seed(seed);
        Self { sk, pk }
    }
}

/// Produce a sealed packet from an order and the chain's events. Refuses under Art. III / XI
/// before touching a single event.
pub fn export(order: &DisclosureOrder, blocks: &[BlockEvents], viewing_keys: &[(WalletId, [u8; 32])], court_root: [u8; 32], keys: &CourtKeys, now_ts: u64) -> Result<DisclosurePacket, DisclosureError> {
    if order.request.key_kind == Some(KeyKind::Spend) {
        return Err(DisclosureError::Unconstitutional { article: Article::SpendKeysNeverLeave, reason: "a spend key is not disclosable by any Order".into() });
    }
    if !order.is_live(now_ts) { return Err(DisclosureError::Expired { expires_ts: order.expires_ts, now: now_ts }); }
    let scope = &order.request.scope;
    if scope.to_height < scope.from_height { return Err(DisclosureError::EmptyScope); }
    for (w, _) in viewing_keys {
        if !scope.subjects.contains(w) { return Err(DisclosureError::GrantForNonSubject(hex::encode(w))); }
    }
    let mut records = Vec::new();
    let mut sorted: Vec<&BlockEvents> = blocks.iter().collect();
    sorted.sort_by_key(|b| b.height);
    for b in sorted {
        if b.height < scope.from_height || b.height > scope.to_height { continue; }
        let leaves: Vec<[u8; 32]> = b.events.iter().map(|e| e.leaf_hash()).collect();
        if merkle::root(&leaves) != b.event_log_root { return Err(DisclosureError::RootMismatch { height: b.height }); }
        for (i, e) in b.events.iter().enumerate() {
            if let Some(role) = scope_ok(scope, e, b.height) {
                let proof = merkle::prove(&leaves, i).expect("index in range");
                records.push(minimize(order, e, b.height, i as u32, proof, role));
            }
        }
    }
    let summary = summarize(order, &records);
    let viewing_grants: Vec<ViewingGrant> = match order.request.key_kind {
        Some(KeyKind::Viewing) => viewing_keys.iter().map(|(w, k)| ViewingGrant { subject: hex::encode(w), viewing_key_hex: hex::encode(k) }).collect(),
        _ => Vec::new(),
    };
    let body = SealBody {
        schema: PACKET_SCHEMA.into(),
        order_id: order.id,
        order_digest: order_digest(order),
        packet_root: DisclosurePacket::packet_root(&records),
        court_root,
        constitution_hash: constitution_hash(),
        issued_ts: now_ts,
        expires_ts: order.expires_ts,
        jurisdiction: order.request.jurisdiction.label(),
        purpose: order.request.purpose,
        records: records.len() as u32,
        redacted_fields: summary.redacted_fields,
        viewing_grants: viewing_grants.len() as u32,
    };
    let sig = flux_sqisign::sign(&body.signing_bytes(), &keys.sk, &keys.pk).map_err(DisclosureError::BadSeal)?;
    Ok(DisclosurePacket {
        schema: PACKET_SCHEMA.into(),
        order: order.clone(),
        records,
        summary,
        viewing_grants,
        seal: Seal { body, court_pubkey_hex: hex::encode(&keys.pk), sig_hex: hex::encode(sig) },
    })
}

/// What a verifier learns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Records whose leaf is proven under the block root the verifier supplied.
    pub records_proven: u32,
    /// Records whose cleartext fields are re-derivable from a carried full event.
    pub fields_recomputed: u32,
    /// Records whose cleartext fields rest on the court's seal (minimized records).
    pub fields_attested: u32,
}

/// The recipient's check. `roots` = block height → `event_log_root` from the block headers they
/// trust; `court_pubkey` = the pinned court key.
pub fn verify_packet(p: &DisclosurePacket, roots: &BTreeMap<u64, [u8; 32]>, court_pubkey: &[u8], now_ts: u64) -> Result<Verified, DisclosureError> {
    if p.schema != PACKET_SCHEMA || p.seal.body.schema != PACKET_SCHEMA { return Err(DisclosureError::SchemaMismatch); }
    if p.seal.court_pubkey_hex != hex::encode(court_pubkey) { return Err(DisclosureError::BadSeal("court key does not match the pinned key".into())); }
    let sig = hex::decode(&p.seal.sig_hex).map_err(|e| DisclosureError::BadSeal(e.to_string()))?;
    match flux_sqisign::verify(&p.seal.body.signing_bytes(), &sig, court_pubkey) {
        Ok(true) => {}
        Ok(false) => return Err(DisclosureError::BadSeal("signature does not verify".into())),
        Err(e) => return Err(DisclosureError::BadSeal(e)),
    }
    let b = &p.seal.body;
    if b.order_id != p.order.id { return Err(DisclosureError::BadSeal("seal is for another order".into())); }
    if b.order_digest != order_digest(&p.order) { return Err(DisclosureError::OrderTermsAltered); }
    if b.constitution_hash != constitution_hash() { return Err(DisclosureError::BadSeal("sealed under a different constitution".into())); }
    if now_ts >= b.expires_ts { return Err(DisclosureError::Expired { expires_ts: b.expires_ts, now: now_ts }); }
    if b.records != p.records.len() as u32 || b.viewing_grants != p.viewing_grants.len() as u32 { return Err(DisclosureError::BadSeal("record/grant counts differ from seal".into())); }
    if DisclosurePacket::packet_root(&p.records) != b.packet_root { return Err(DisclosureError::PacketRootMismatch); }
    let mut v = Verified { records_proven: 0, fields_recomputed: 0, fields_attested: 0 };
    for r in &p.records {
        let root = roots.get(&r.height).ok_or(DisclosureError::RootUnknown { height: r.height })?;
        if !merkle::verify(r.leaf, &r.proof, *root) { return Err(DisclosureError::RecordNotProven { height: r.height, index: r.index }); }
        v.records_proven += 1;
        match &r.full_event {
            Some(e) => {
                if e.leaf_hash() != r.leaf { return Err(DisclosureError::LeafMismatch { height: r.height, index: r.index }); }
                v.fields_recomputed += 1;
            }
            None => v.fields_attested += 1,
        }
    }
    if summarize(&p.order, &p.records) != p.summary { return Err(DisclosureError::SummaryMismatch); }
    Ok(v)
}

/// The bank's books: every money movement the bank layer emits, institution-wide.
pub fn bank_scope(from_height: u64, to_height: u64) -> Scope {
    Scope {
        subjects: BTreeSet::new(),
        kinds: [7u8, 15, 16, 17, 18, 19, 21].into_iter().collect(), // MintReward, Mandate*, Bank*, WelfareClaimed
        from_height,
        to_height,
        tokens: None,
    }
}

/// The tax office's request for one citizen: every money event they are a party to.
pub fn tax_scope(subject: WalletId, from_height: u64, to_height: u64) -> Scope {
    Scope {
        subjects: [subject].into_iter().collect(),
        kinds: [0u8, 1, 2, 7, 13, 14, 19, 21].into_iter().collect(), // Send, Receive, Swap, Mint, Usds*, BankExecuted, Welfare
        from_height,
        to_height,
        tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(b: u8) -> WalletId { [b; 32] }

    fn chain() -> Vec<BlockEvents> {
        vec![
            BlockEvents::seal(10, vec![
                SigilEvent::MintReward { miner: w(7), height: 10, amount: 500 },
                SigilEvent::Send { from: w(7), to: w(8), amount: 120, token: NATIVE, fee: 3 },
                SigilEvent::Send { from: w(9), to: w(7), amount: 40, token: NATIVE, fee: 1 },
            ]),
            BlockEvents::seal(11, vec![
                SigilEvent::WelfareClaimed { citizen: w(7), amount: 25 },
                SigilEvent::BankExecuted { id: "p1".into(), from: [0x57; 32], to: w(7), token: NATIVE, amount: 1000 },
                SigilEvent::Send { from: w(1), to: w(2), amount: 999, token: NATIVE, fee: 9 },
            ]),
            BlockEvents::seal(12, vec![
                SigilEvent::SwapExecuted { pool: [3; 32], in_token: NATIVE, in_amt: 10, out_token: [4; 32], out_amt: 20, slippage_bps: 5, fee_paid: 1 },
            ]),
        ]
    }

    fn order(purpose: Purpose, scope: Scope, key: Option<KeyKind>) -> DisclosureOrder {
        let req = DisclosureRequest { jurisdiction: Jurisdiction::new("DK", "SKAT"), purpose, scope, key_kind: key, reason: "årsopgørelse".into(), requested_ts: 1_000, ttl_secs: 3_600 };
        DisclosureOrder { id: order_id(&req, 12, 1_000), request: req, case: None, issued_height: 12, issued_ts: 1_000, expires_ts: 4_600, panel: vec![w(20), w(21), w(22)], votes_for: 3, minimization: Minimization::for_purpose(purpose) }
    }

    fn roots(c: &[BlockEvents]) -> BTreeMap<u64, [u8; 32]> { c.iter().map(|b| (b.height, b.event_log_root)).collect() }

    #[test]
    fn tax_packet_is_minimized_proven_sealed_and_summed() {
        let keys = CourtKeys::from_seed(&[42; 32]);
        let c = chain();
        let o = order(Purpose::Tax, tax_scope(w(7), 0, 100), Some(KeyKind::Viewing));
        let p = export(&o, &c, &[(w(7), [0xAB; 32])], [1; 32], &keys, 2_000).unwrap();
        // 5 records: mint, send(sender), send(recipient), welfare, bank-in. Not the stranger send, not the swap.
        assert_eq!(p.records.len(), 5);
        let roles: Vec<&str> = p.records.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, vec!["miner", "sender", "recipient", "citizen", "recipient"]);
        // counterparties pseudonymized, subject in clear, amounts in clear
        let send = &p.records[1];
        assert_eq!(send.fields["from"], hex::encode(w(7)));
        assert!(send.fields["to"].starts_with("pseud:"));
        assert_eq!(send.redacted, vec!["to"]);
        assert_eq!(send.fields["amount"], "120");
        assert!(send.full_event.is_none());
        // summary
        let t = &p.summary.per_token[&hex::encode(NATIVE)];
        assert_eq!((t.mined, t.sent, t.received, t.fees, t.welfare, t.bank_in), (500, 120, 40, 3, 25, 1000));
        // 3 redactions: the Send counterparty each way, and the welfare treasury on the bank transfer.
        // The subject itself is never redacted — a tax office that cannot see whose return it is
        // has been handed noise (Art. IV is minimization, not obfuscation).
        assert_eq!(p.summary.redacted_fields, 3);
        assert_eq!(p.viewing_grants.len(), 1);
        let v = verify_packet(&p, &roots(&c), &keys.pk, 3_000).unwrap();
        assert_eq!(v, Verified { records_proven: 5, fields_recomputed: 0, fields_attested: 5 });
        assert!(p.tax_csv().lines().count() == 6);
    }

    #[test]
    fn spend_key_request_is_refused_under_article_three() {
        let keys = CourtKeys::from_seed(&[1; 32]);
        let o = order(Purpose::Tax, tax_scope(w(7), 0, 100), Some(KeyKind::Spend));
        assert_eq!(export(&o, &chain(), &[], [0; 32], &keys, 2_000), Err(DisclosureError::Unconstitutional { article: Article::SpendKeysNeverLeave, reason: "a spend key is not disclosable by any Order".into() }));
    }

    #[test]
    fn expiry_and_bad_roots_refuse() {
        let keys = CourtKeys::from_seed(&[1; 32]);
        let o = order(Purpose::Aml, tax_scope(w(7), 0, 100), None);
        assert_eq!(export(&o, &chain(), &[], [0; 32], &keys, 4_600), Err(DisclosureError::Expired { expires_ts: 4_600, now: 4_600 }));
        let mut c = chain();
        c[0].event_log_root = [9; 32];
        assert_eq!(export(&o, &c, &[], [0; 32], &keys, 2_000), Err(DisclosureError::RootMismatch { height: 10 }));
        assert_eq!(export(&o, &chain(), &[(w(8), [0; 32])], [0; 32], &keys, 2_000), Err(DisclosureError::GrantForNonSubject(hex::encode(w(8)))));
    }

    #[test]
    fn court_order_carries_full_events_and_tamper_is_caught() {
        let keys = CourtKeys::from_seed(&[5; 32]);
        let c = chain();
        let o = order(Purpose::CourtOrder, tax_scope(w(7), 11, 11), None);
        let mut p = export(&o, &c, &[], [0; 32], &keys, 2_000).unwrap();
        assert_eq!(p.records.len(), 2);
        assert!(p.records.iter().all(|r| r.full_event.is_some() && r.redacted.is_empty()));
        let v = verify_packet(&p, &roots(&c), &keys.pk, 3_000).unwrap();
        assert_eq!(v, Verified { records_proven: 2, fields_recomputed: 2, fields_attested: 0 });
        // tamper an amount → packet root moves → seal no longer covers it
        p.records[0].fields.insert("amount".into(), "1".into());
        assert_eq!(verify_packet(&p, &roots(&c), &keys.pk, 3_000), Err(DisclosureError::PacketRootMismatch));
        // wrong court key → refused
        let other = CourtKeys::from_seed(&[6; 32]);
        let p2 = export(&o, &c, &[], [0; 32], &keys, 2_000).unwrap();
        assert!(matches!(verify_packet(&p2, &roots(&c), &other.pk, 3_000), Err(DisclosureError::BadSeal(_))));
        // unknown block root → the verifier cannot prove it
        let mut r = roots(&c);
        r.remove(&11);
        assert_eq!(verify_packet(&p2, &r, &keys.pk, 3_000), Err(DisclosureError::RootUnknown { height: 11 }));
        // expired at verify time
        assert!(matches!(verify_packet(&p2, &roots(&c), &keys.pk, 9_999), Err(DisclosureError::Expired { .. })));
    }

    #[test]
    fn bank_books_export_is_institution_wide() {
        let keys = CourtKeys::from_seed(&[7; 32]);
        let c = chain();
        let o = order(Purpose::Audit, bank_scope(0, 100), None);
        let p = export(&o, &c, &[], [0; 32], &keys, 2_000).unwrap();
        let kinds: Vec<&str> = p.records.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["MintReward", "WelfareClaimed", "BankExecuted"]);
        assert!(p.records.iter().all(|r| r.role == "any"));
        let t = &p.summary.per_token[&hex::encode(NATIVE)];
        assert_eq!((t.mined, t.welfare, t.bank_out), (500, 25, 1000));
        verify_packet(&p, &roots(&c), &keys.pk, 3_000).unwrap();
    }

    /// The digest must move for a change to ANY term, not only to the request. The
    /// `reveal_counterparties` case is the one that was actually exploitable.
    #[test]
    fn order_digest_covers_the_terms_not_just_the_request() {
        let base = order(Purpose::Tax, tax_scope(w(7), 0, 100), None);
        let d = order_digest(&base);
        assert_ne!(d, [0u8; 32]);

        let mut o = base.clone();
        o.minimization.reveal_counterparties = true;
        assert_ne!(d, order_digest(&o), "minimization level must be sealed");

        let mut o = base.clone();
        o.minimization.carry_full_event = true;
        assert_ne!(d, order_digest(&o));

        let mut o = base.clone();
        o.votes_for += 1;
        assert_ne!(d, order_digest(&o), "the vote must be sealed");

        let mut o = base.clone();
        o.panel.push(w(99));
        assert_ne!(d, order_digest(&o), "the panel must be sealed");

        let mut o = base.clone();
        o.case = Some([1; 32]);
        assert_ne!(d, order_digest(&o));

        let mut o = base.clone();
        o.request.scope.to_height = 9_999;
        assert_ne!(d, order_digest(&o));

        assert_eq!(d, order_digest(&base.clone()), "and it is stable");
    }

    #[test]
    fn pseudonyms_are_stable_within_and_unlinkable_across_orders() {
        let a = pseudonym(&[1; 32], "aa");
        assert_eq!(a, pseudonym(&[1; 32], "aa"));
        assert_ne!(a, pseudonym(&[2; 32], "aa"));
        assert_ne!(a, pseudonym(&[1; 32], "ab"));
    }
}
