//! sigil-migrate — import the whole Quillon economy into SIGIL genesis.
//!
//! **The smart way is a signed genesis snapshot, not a live bridge.** Take one
//! authenticated snapshot of Quillon state at height `H`, map it deterministically
//! to a `Vec<StateMutation>` for SIGIL genesis, and commit the snapshot's blake3
//! manifest root + the Quillon block hash into SIGIL block 0. Every SIGIL node
//! recomputes the root at boot (the mandated 4-state-root pre-flight) → consensus
//! on the imported economy is automatic, and the import is a permanent, verifiable
//! historical fact rather than a trust-me allocation.
//!
//! Why not a runtime bridge: Quillon block production isn't finalizing right now
//! (libp2p UnsupportedProtocols stall), and a live two-chain bridge inherits all
//! of that plus replay/double-mint/oracle-lag risk. A static snapshot can't
//! double-spend, survives Quillon being down (HTTP reads work even with P2P
//! broken), and anyone can re-derive it from Quillon block `H` and diff.
//!
//! ## The six Quillon asset classes (surveyed live via the Quillon MCP)
//! | class | examples | → SIGIL |
//! |---|---|---|
//! | native | QUG | `wQUG` token (native SIGIL stays fresh-emission) OR native (open decision) |
//! | stablecoin | QUGUSD | token + peg params |
//! | wrapped | wBTC/wETH/wZEC/wIRON | token, balances seeded, `redeemable=false` until SIGIL bridge |
//! | equity/NAV | QSHARE | token + NAV/premium params |
//! | credit | QCREDIT | token + per-wallet credit positions |
//! | custom/meme/index | PACI/SCALPEL/CLAI/FORGE/… + defi indexes | token each (admin preserved) |
//!
//! Plus the 26 DEX pools (`SetPool`), with LP holder ledgers preserved so fee
//! attribution survives (Rocky/PACI, Codex/SCALPEL, …).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sigil_state::{PoolState, StateMutation, TokenId, WalletId, NATIVE};

/// Domain-separation prefix for deriving a SIGIL `TokenId` from a Quillon token's
/// canonical identifier (symbol for first-class assets, `qnk…` address for custom
/// tokens). blake3 over `PREFIX || canonical_id` → 32 bytes. Deterministic +
/// collision-resistant, so the same Quillon token always maps to the same SIGIL id.
const TOKEN_ID_PREFIX: &[u8] = b"sigil-migrate:token:v1:";

/// Derive a stable SIGIL `TokenId` for a Quillon token from its canonical id.
/// The native-QUG decision is handled by the caller (see [`QuillonToken::sigil_token_id`]);
/// this is the generic derivation for every non-native token.
pub fn derive_token_id(canonical_id: &str) -> TokenId {
    let mut h = blake3::Hasher::new();
    h.update(TOKEN_ID_PREFIX);
    h.update(canonical_id.as_bytes());
    *h.finalize().as_bytes()
}

/// How QUG maps into SIGIL.
///
/// **DECIDED (Viktor, 2026-05-30): `WrappedToken`.** QUG imports as `wQUG`, a
/// first-class SIGIL token, while SIGIL keeps its OWN fresh native emission +
/// oracle + USDS monetary policy. Every Quillon holder is whole on day one
/// (their QUG value lands as wQUG), but SIGIL is a new chain that *imported*
/// Quillon's economy — not a rebrand of it. This is [`QugMapping::default`].
///
/// `Native` (QUG == SIGIL native coin 1:1, "Quillon v2") is retained only so the
/// mapping stays expressible/testable; it is NOT the chosen path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QugMapping {
    /// QUG becomes SIGIL's native coin 1:1 (SIGIL = "Quillon v2"). QUG balances
    /// seed the `NATIVE` token. NOT the chosen path — kept for completeness.
    Native,
    /// QUG imports as a `wQUG` token alongside a fresh native SIGIL emission
    /// (keeps SIGIL's own emission/oracle/USDS monetary policy clean while every
    /// Quillon holder is whole on day one). **The chosen day-one mapping.**
    #[default]
    WrappedToken,
}

/// The canonical `wQUG` token id — the SIGIL home for imported Quillon QUG under
/// the decided [`QugMapping::WrappedToken`] policy. Stable across runs.
pub fn wqug_token_id() -> TokenId {
    derive_token_id("wQUG")
}

/// One Quillon token's definition (not its balances — see [`QuillonSnapshot::balances`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillonToken {
    /// Canonical id: the symbol for first-class assets ("QUG", "wBTC", "QSHARE"),
    /// or the `qnk…` address for custom/memecoins.
    pub canonical_id: String,
    pub symbol: String,
    pub decimals: u8,
    pub class: AssetClass,
    /// Original deployer/admin (`qnk…`); preserved so PACI stays Rocky's etc.
    pub admin: Option<String>,
    /// For wrapped assets: whether the underlying is redeemable on SIGIL day one.
    /// Always `false` at migration — we do NOT claim real-BTC redemption before
    /// the SIGIL bridge exists.
    pub redeemable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetClass {
    Native,
    Stablecoin,
    Wrapped,
    Equity,
    Credit,
    Custom,
}

impl QuillonToken {
    /// The SIGIL `TokenId` this token maps to, honoring the QUG decision.
    pub fn sigil_token_id(&self, qug: QugMapping) -> TokenId {
        if self.canonical_id == "QUG" {
            match qug {
                QugMapping::Native => NATIVE,
                QugMapping::WrappedToken => derive_token_id("wQUG"),
            }
        } else {
            derive_token_id(&self.canonical_id)
        }
    }
}

/// A wallet's balance of one token, in that token's base units (raw integer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillonBalance {
    /// `qnk…` owner address.
    pub wallet: String,
    /// Canonical token id (matches a [`QuillonToken::canonical_id`]).
    pub token: String,
    /// Raw amount in base units (decimals applied). Stored as string in JSON to
    /// survive >2^53 without f64 precision loss.
    pub amount: u128,
}

/// One Quillon DEX pool at snapshot time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillonPool {
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub lp_shares: u128,
    pub fee_bps: u16,
    /// LP holder ledger: `qnk…` → shares. Sum must equal `lp_shares`.
    pub holders: BTreeMap<String, u128>,
}

/// Tier-1 contract kinds migrated day one — the data-heavy templates whose
/// economic meaning lives almost entirely in their *storage*, so seeding the
/// raw slots + re-implementing the (simple, well-understood) logic on SIGIL is
/// enough. Logic-heavy templates (DAO, staking, lending, perps, options) +
/// externally-dependent ones (oracle, bridge) are DEFERRED — re-deployed/re-init
/// on SIGIL, not slot-migrated (Viktor's Tier-1 scope, 2026-05-30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractKind {
    /// timelock_vault — funds released after a delay / vesting cliffs.
    TimelockVault,
    /// multisig_wallet — N-of-M owner signature treasury.
    MultisigWallet,
    /// escrow — funds held pending a release condition.
    Escrow,
    /// vesting schedule (beneficiary, cliff, total, released).
    Vesting,
    /// NFT ownership ledger (token-id → owner).
    Nft,
}

/// One deployed Quillon contract migrated as opaque storage slots. SIGIL
/// re-implements the logic (Tier 1); we carry the STATE verbatim so balances,
/// owners, cliffs, and pending releases survive into genesis. Each
/// `(slot_hex → value)` becomes a `SetContractSlot` mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillonContract {
    /// Quillon contract address (`qnk…` or bare hex) — becomes the SIGIL ContractId.
    pub address: String,
    pub kind: ContractKind,
    /// Original deployer/admin (`qnk…`) — preserved so control survives.
    pub admin: Option<String>,
    /// Raw storage: 32-byte slot key (hex) → 32-byte value (hex). Opaque to the
    /// migration; meaning is the SIGIL re-implementation's job. Sorted by key in
    /// `canonical_json` so the manifest root is reproducible.
    pub slots: BTreeMap<String, String>,
}

/// Where the snapshot came from — committed into SIGIL block 0 so the import is
/// provenance-anchored and re-derivable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRef {
    pub quillon_height: u64,
    /// Quillon block hash at `quillon_height` (hex). The anchor: this exact block
    /// is what the economy was read from.
    pub quillon_block_hash: String,
    pub source: SnapshotSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotSource {
    /// Read via the Quillon Wallet MCP HTTP tools (works while Quillon P2P is
    /// broken — this is the day-one path).
    Mcp,
    /// Read directly from the Quillon `data-mainnet-genesis` RocksDB column
    /// families (the final authoritative pass).
    RocksDb,
}

/// The full Quillon economy at height `H`, ready to map into SIGIL genesis.
/// Deterministic: tokens/balances/pools are sorted before hashing so the blake3
/// manifest root is reproducible by anyone re-running the snapshot against `H`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillonSnapshot {
    pub provenance: ProvenanceRef,
    pub tokens: Vec<QuillonToken>,
    pub balances: Vec<QuillonBalance>,
    pub pools: Vec<QuillonPool>,
    /// Tier-1 data contracts (timelock/multisig/escrow/vesting/NFT) migrated as
    /// opaque storage slots. Default-empty so older snapshots deserialize.
    #[serde(default)]
    pub contracts: Vec<QuillonContract>,
}

/// Errors a snapshot can fail integrity checks with — caught BEFORE genesis,
/// per the balance-integrity discipline (verify before trusting a migration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// A balance references a token not in `tokens`.
    UnknownToken(String),
    /// A pool references a token not in `tokens`.
    PoolUnknownToken(String),
    /// A pool's holder shares don't sum to `lp_shares`.
    LpShareMismatch { pool: String, sum: u128, declared: u128 },
    /// A `qnk…` address didn't decode to 32 bytes.
    BadAddress(String),
    /// A contract slot key or value wasn't 32 bytes of hex.
    BadSlot { contract: String, detail: String },
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::UnknownToken(t) => write!(f, "balance references unknown token {t}"),
            MigrateError::PoolUnknownToken(t) => write!(f, "pool references unknown token {t}"),
            MigrateError::LpShareMismatch { pool, sum, declared } => write!(
                f,
                "pool {pool}: holder shares sum to {sum} but lp_shares={declared}"
            ),
            MigrateError::BadAddress(a) => write!(f, "address {a} did not decode to 32 bytes"),
            MigrateError::BadSlot { contract, detail } => {
                write!(f, "contract {contract}: bad slot ({detail})")
            }
        }
    }
}
impl std::error::Error for MigrateError {}

/// Decode a 32-byte value from hex (slot key or slot value). Accepts optional
/// `0x` prefix; must be exactly 64 hex chars.
fn decode_b32(s: &str) -> Option<[u8; 32]> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(h).ok()?;
    bytes.try_into().ok()
}

/// Decode a `qnk…` address (or bare hex) to a 32-byte `WalletId`.
/// Accepts an optional `qnk` prefix; the rest must be 64 hex chars.
pub fn decode_address(addr: &str) -> Result<WalletId, MigrateError> {
    let hex_part = addr.strip_prefix("qnk").unwrap_or(addr);
    let bytes = hex::decode(hex_part).map_err(|_| MigrateError::BadAddress(addr.to_string()))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| MigrateError::BadAddress(addr.to_string()))?;
    Ok(arr)
}

impl QuillonSnapshot {
    /// Validate internal consistency BEFORE producing genesis mutations. Returns
    /// every problem found (not just the first) so a migration operator can fix
    /// the snapshot in one pass. An empty result = safe to map.
    pub fn validate(&self) -> Vec<MigrateError> {
        let mut errs = Vec::new();
        let known: std::collections::BTreeSet<&str> =
            self.tokens.iter().map(|t| t.canonical_id.as_str()).collect();

        for b in &self.balances {
            if !known.contains(b.token.as_str()) {
                errs.push(MigrateError::UnknownToken(b.token.clone()));
            }
            if let Err(e) = decode_address(&b.wallet) {
                errs.push(e);
            }
        }
        for p in &self.pools {
            if !known.contains(p.token_a.as_str()) {
                errs.push(MigrateError::PoolUnknownToken(p.token_a.clone()));
            }
            if !known.contains(p.token_b.as_str()) {
                errs.push(MigrateError::PoolUnknownToken(p.token_b.clone()));
            }
            let sum: u128 = p.holders.values().copied().sum();
            if sum != p.lp_shares {
                errs.push(MigrateError::LpShareMismatch {
                    pool: format!("{}/{}", p.token_a, p.token_b),
                    sum,
                    declared: p.lp_shares,
                });
            }
            for h in p.holders.keys() {
                if let Err(e) = decode_address(h) {
                    errs.push(e);
                }
            }
        }
        for c in &self.contracts {
            if decode_address(&c.address).is_err() {
                errs.push(MigrateError::BadAddress(c.address.clone()));
            }
            if let Some(a) = &c.admin {
                if decode_address(a).is_err() {
                    errs.push(MigrateError::BadAddress(a.clone()));
                }
            }
            for (slot, value) in &c.slots {
                if decode_b32(slot).is_none() {
                    errs.push(MigrateError::BadSlot {
                        contract: c.address.clone(),
                        detail: format!("slot key {slot} not 32 bytes"),
                    });
                }
                if decode_b32(value).is_none() {
                    errs.push(MigrateError::BadSlot {
                        contract: c.address.clone(),
                        detail: format!("slot value for {slot} not 32 bytes"),
                    });
                }
            }
        }
        errs
    }

    /// Map the snapshot to the genesis `StateMutation` list. Deterministic order:
    /// balances sorted by (wallet, token), then pools sorted by id. Must
    /// `validate()` clean first (this assumes consistency but still returns
    /// errors on bad addresses).
    pub fn to_mutations(&self, qug: QugMapping) -> Result<Vec<StateMutation>, MigrateError> {
        // canonical_id → SIGIL TokenId, honoring the QUG decision
        let token_id: BTreeMap<&str, TokenId> = self
            .tokens
            .iter()
            .map(|t| (t.canonical_id.as_str(), t.sigil_token_id(qug)))
            .collect();
        let tid = |canon: &str| -> Result<TokenId, MigrateError> {
            token_id
                .get(canon)
                .copied()
                .ok_or_else(|| MigrateError::UnknownToken(canon.to_string()))
        };

        let mut muts: Vec<StateMutation> = Vec::new();

        // 1. balances → SetBalance (sorted for determinism)
        let mut bals = self.balances.clone();
        bals.sort_by(|x, y| (x.wallet.as_str(), x.token.as_str()).cmp(&(&y.wallet, &y.token)));
        for b in &bals {
            muts.push(StateMutation::SetBalance {
                wallet: decode_address(&b.wallet)?,
                token: tid(&b.token)?,
                amount: b.amount,
            });
        }

        // 2. pools → SetPool (sorted by canonical pool key)
        let mut pools = self.pools.clone();
        pools.sort_by(|x, y| {
            (x.token_a.as_str(), x.token_b.as_str()).cmp(&(&y.token_a, &y.token_b))
        });
        for p in &pools {
            let ta = tid(&p.token_a)?;
            let tb = tid(&p.token_b)?;
            // honor sigil-state's canonical ordering: token_a < token_b by bytes
            let (token_a, token_b, reserve_a, reserve_b) = if ta <= tb {
                (ta, tb, p.reserve_a, p.reserve_b)
            } else {
                (tb, ta, p.reserve_b, p.reserve_a)
            };
            // NOTE: the current sigil-state `PoolState` does NOT carry an LP
            // holder ledger (only aggregate lp_shares). So per-holder LP
            // attribution (Rocky/PACI, Codex/SCALPEL) cannot be seeded into
            // genesis via SetPool today — see `lp_holder_ledger` for the
            // out-of-band table we still emit, pending a state-model field.
            let pool_id = derive_pool_id(&token_a, &token_b);
            muts.push(StateMutation::SetPool {
                pool: pool_id,
                state: PoolState {
                    token_a,
                    token_b,
                    reserve_a,
                    reserve_b,
                    lp_shares: p.lp_shares,
                    fee_bps: p.fee_bps,
                    accrued_fees: 0,
                },
            });
        }

        // 3. Tier-1 contracts → SetContractSlot per storage slot (sorted for
        //    determinism: by contract address, then slot key).
        let mut contracts = self.contracts.clone();
        contracts.sort_by(|x, y| x.address.cmp(&y.address));
        for c in &contracts {
            let contract = decode_address(&c.address)?;
            // BTreeMap already iterates slots in sorted key order → deterministic.
            for (slot_hex, value_hex) in &c.slots {
                let slot = decode_b32(slot_hex).ok_or_else(|| MigrateError::BadSlot {
                    contract: c.address.clone(),
                    detail: format!("slot key {slot_hex}"),
                })?;
                let value = decode_b32(value_hex).ok_or_else(|| MigrateError::BadSlot {
                    contract: c.address.clone(),
                    detail: format!("slot value for {slot_hex}"),
                })?;
                muts.push(StateMutation::SetContractSlot { contract, slot, value });
            }
        }

        Ok(muts)
    }

    /// The LP holder ledger that `SetPool` can't carry (the state model has only
    /// aggregate `lp_shares`). Returns `(pool_id, holder_wallet, shares)` triples
    /// so fee attribution survives the migration out-of-band — to be consumed by
    /// a future LP-position state field, or re-issued as LP deposits on SIGIL.
    /// Included in the manifest hash via `canonical_json`, so it IS provenance-anchored.
    pub fn lp_holder_ledger(&self, qug: QugMapping) -> Result<Vec<([u8; 32], WalletId, u128)>, MigrateError> {
        let token_id: BTreeMap<&str, TokenId> = self
            .tokens
            .iter()
            .map(|t| (t.canonical_id.as_str(), t.sigil_token_id(qug)))
            .collect();
        let tid = |c: &str| token_id.get(c).copied().ok_or_else(|| MigrateError::UnknownToken(c.to_string()));
        let mut out = Vec::new();
        for p in &self.pools {
            let ta = tid(&p.token_a)?;
            let tb = tid(&p.token_b)?;
            let (a, b) = if ta <= tb { (ta, tb) } else { (tb, ta) };
            let pool_id = derive_pool_id(&a, &b);
            for (addr, shares) in &p.holders {
                out.push((pool_id, decode_address(addr)?, *shares));
            }
        }
        out.sort();
        Ok(out)
    }

    /// blake3 manifest root over the canonical JSON of the snapshot. This is what
    /// goes into SIGIL block 0 alongside the Quillon block hash. Reproducible:
    /// anyone re-runs the snapshot against height `H` and gets the same root.
    pub fn manifest_root(&self) -> [u8; 32] {
        // serde_json with sorted maps + our pre-sorted vecs → canonical bytes.
        let canonical = self.canonical_json();
        *blake3::hash(canonical.as_bytes()).as_bytes()
    }

    /// Canonical JSON: vecs sorted, so two snapshots of the same economy at the
    /// same height serialize identically regardless of read order.
    pub fn canonical_json(&self) -> String {
        let mut c = self.clone();
        c.tokens.sort_by(|a, b| a.canonical_id.cmp(&b.canonical_id));
        c.balances
            .sort_by(|a, b| (a.wallet.as_str(), a.token.as_str()).cmp(&(&b.wallet, &b.token)));
        c.pools
            .sort_by(|a, b| (a.token_a.as_str(), a.token_b.as_str()).cmp(&(&b.token_a, &b.token_b)));
        c.contracts.sort_by(|a, b| a.address.cmp(&b.address));
        serde_json::to_string(&c).expect("snapshot serializes")
    }

    /// Hex of the manifest root, for logging + the genesis header field.
    pub fn manifest_root_hex(&self) -> String {
        hex::encode(self.manifest_root())
    }
}

/// Derive a SIGIL `PoolId` from its two (already canonically-ordered) token ids.
pub fn derive_pool_id(token_a: &TokenId, token_b: &TokenId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-migrate:pool:v1:");
    h.update(token_a);
    h.update(token_b);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qnk(byte: u8) -> String {
        format!("qnk{}", hex::encode([byte; 32]))
    }

    fn sample() -> QuillonSnapshot {
        let tokens = vec![
            QuillonToken {
                canonical_id: "QUG".into(),
                symbol: "QUG".into(),
                decimals: 8,
                class: AssetClass::Native,
                admin: None,
                redeemable: false,
            },
            QuillonToken {
                canonical_id: "PACI".into(),
                symbol: "PACI".into(),
                decimals: 24,
                class: AssetClass::Custom,
                admin: Some(qnk(0x71)),
                redeemable: false,
            },
            QuillonToken {
                canonical_id: "wBTC".into(),
                symbol: "wBTC".into(),
                decimals: 8,
                class: AssetClass::Wrapped,
                admin: None,
                redeemable: false,
            },
        ];
        let balances = vec![
            QuillonBalance { wallet: qnk(0x71), token: "QUG".into(), amount: 12_629_00000000 },
            QuillonBalance { wallet: qnk(0xAB), token: "PACI".into(), amount: 100_000 },
        ];
        let mut holders = BTreeMap::new();
        holders.insert(qnk(0x71), 1_000u128);
        let pools = vec![QuillonPool {
            token_a: "PACI".into(),
            token_b: "QUG".into(),
            reserve_a: 100_000,
            reserve_b: 1,
            lp_shares: 1_000,
            fee_bps: 30,
            holders,
        }];
        let mut slots = BTreeMap::new();
        // a vesting contract: slot 0 = total, slot 1 = released (toy values)
        slots.insert(hex::encode([0u8; 32]), hex::encode({ let mut v = [0u8; 32]; v[31] = 100; v }));
        slots.insert(hex::encode({ let mut k = [0u8; 32]; k[31] = 1; k }), hex::encode([0u8; 32]));
        let contracts = vec![QuillonContract {
            address: qnk(0xCC),
            kind: ContractKind::Vesting,
            admin: Some(qnk(0x71)),
            slots,
        }];
        QuillonSnapshot {
            provenance: ProvenanceRef {
                quillon_height: 18_300_000,
                quillon_block_hash: "deadbeef".into(),
                source: SnapshotSource::Mcp,
            },
            tokens,
            balances,
            pools,
            contracts,
        }
    }

    #[test]
    fn valid_sample_passes() {
        assert!(sample().validate().is_empty(), "clean snapshot must validate");
    }

    #[test]
    fn detects_unknown_token_in_balance() {
        let mut s = sample();
        s.balances.push(QuillonBalance { wallet: qnk(1), token: "GHOST".into(), amount: 1 });
        assert!(s.validate().iter().any(|e| matches!(e, MigrateError::UnknownToken(t) if t == "GHOST")));
    }

    #[test]
    fn detects_lp_share_mismatch() {
        let mut s = sample();
        s.pools[0].lp_shares = 999; // holders sum to 1000
        assert!(s
            .validate()
            .iter()
            .any(|e| matches!(e, MigrateError::LpShareMismatch { .. })));
    }

    #[test]
    fn qug_native_vs_wrapped_differ() {
        let qug = &sample().tokens[0];
        assert_eq!(qug.sigil_token_id(QugMapping::Native), NATIVE);
        assert_ne!(qug.sigil_token_id(QugMapping::WrappedToken), NATIVE);
    }

    #[test]
    fn default_mapping_is_wqug_per_viktor_decision() {
        // Viktor decided 2026-05-30: QUG imports as wQUG, not native.
        assert_eq!(QugMapping::default(), QugMapping::WrappedToken);
        let qug = &sample().tokens[0];
        assert_eq!(
            qug.sigil_token_id(QugMapping::default()),
            wqug_token_id(),
            "QUG under the default mapping must land on the canonical wQUG id"
        );
        assert_ne!(wqug_token_id(), NATIVE, "wQUG must NOT be the native token");
    }

    #[test]
    fn token_ids_are_stable_and_distinct() {
        assert_eq!(derive_token_id("PACI"), derive_token_id("PACI"));
        assert_ne!(derive_token_id("PACI"), derive_token_id("SCALPEL"));
    }

    #[test]
    fn to_mutations_emits_balance_and_pool() {
        let muts = sample().to_mutations(QugMapping::WrappedToken).unwrap();
        let n_bal = muts.iter().filter(|m| matches!(m, StateMutation::SetBalance { .. })).count();
        let n_pool = muts.iter().filter(|m| matches!(m, StateMutation::SetPool { .. })).count();
        assert_eq!(n_bal, 2, "two balances → two SetBalance");
        assert_eq!(n_pool, 1, "one pool → one SetPool");
    }

    #[test]
    fn pool_respects_canonical_token_ordering() {
        let muts = sample().to_mutations(QugMapping::WrappedToken).unwrap();
        let pool = muts.iter().find_map(|m| match m {
            StateMutation::SetPool { state, .. } => Some(state),
            _ => None,
        }).unwrap();
        assert!(pool.token_a <= pool.token_b, "token_a must be <= token_b by bytes");
        // reserves must travel with their token after any swap-ordering
        assert_eq!(pool.reserve_a + pool.reserve_b, 100_001);
    }

    #[test]
    fn contracts_migrate_as_setcontractslot() {
        let muts = sample().to_mutations(QugMapping::WrappedToken).unwrap();
        let n_slots = muts
            .iter()
            .filter(|m| matches!(m, StateMutation::SetContractSlot { .. }))
            .count();
        assert_eq!(n_slots, 2, "sample vesting contract has 2 slots → 2 SetContractSlot");
    }

    #[test]
    fn contract_bad_slot_is_caught() {
        let mut s = sample();
        s.contracts[0].slots.insert("nothex".into(), hex::encode([0u8; 32]));
        assert!(s.validate().iter().any(|e| matches!(e, MigrateError::BadSlot { .. })));
    }

    #[test]
    fn contract_state_folds_into_manifest_root() {
        let s1 = sample();
        let mut s2 = sample();
        // flip one byte of one slot value → root must move
        let key = s2.contracts[0].slots.keys().next().unwrap().clone();
        s2.contracts[0].slots.insert(key, hex::encode({ let mut v = [0u8; 32]; v[0] = 9; v }));
        assert_ne!(s1.manifest_root(), s2.manifest_root(), "contract slot change must move root");
    }

    #[test]
    fn lp_holder_ledger_preserved_out_of_band() {
        // PoolState can't hold the holder ledger, so it's exported separately
        // but still folded into the manifest hash (provenance-anchored).
        let led = sample().lp_holder_ledger(QugMapping::WrappedToken).unwrap();
        assert_eq!(led.len(), 1, "one holder in the sample pool");
        assert_eq!(led[0].2, 1_000, "holder's shares preserved");
    }

    #[test]
    fn manifest_root_is_deterministic_and_order_independent() {
        let s1 = sample();
        let mut s2 = sample();
        // shuffle input order — canonical_json must normalize it away
        s2.balances.reverse();
        s2.tokens.reverse();
        assert_eq!(s1.manifest_root(), s2.manifest_root(), "root must be order-independent");
        assert_eq!(s1.manifest_root_hex().len(), 64);
    }

    #[test]
    fn manifest_root_changes_when_economy_changes() {
        let s1 = sample();
        let mut s2 = sample();
        s2.balances[0].amount += 1; // one satoshi different
        assert_ne!(s1.manifest_root(), s2.manifest_root(), "any change must move the root");
    }

    #[test]
    fn bad_address_is_caught() {
        let mut s = sample();
        s.balances[0].wallet = "qnkNOThex".into();
        assert!(s.validate().iter().any(|e| matches!(e, MigrateError::BadAddress(_))));
    }
}
