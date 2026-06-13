//! Token + pool registry — the SIGIL port of Quillon's
//! `q-storage/src/token_registry.rs` (LANE-W item 4: sigil-dex was missing a
//! registry entirely).
//!
//! ## Deliberate divergences from the Quillon original
//!
//! - **No I/O.** The original was an async RocksDB + tokio::RwLock cache
//!   stack. sigil-dex is a pure crate; the registry is a plain serde
//!   struct over `BTreeMap`s. The caller (sigil-rpcd) persists it inside
//!   its flux-db snapshot exactly like the rest of node state — one
//!   persistence path, not two.
//! - **No market-data fields.** `price_usd`/`market_cap`/`volume_24h`/
//!   `BigDecimal` were API-server display state, not consensus state. The
//!   oracle (`sigil-oracle`) owns price; the registry owns identity.
//! - **Heights, not wall-clock.** `created_at` is a block height —
//!   deterministic, replayable.
//! - **Binary ids.** Tokens/pools are keyed by their 32-byte chain ids
//!   (`sigil_state::TokenId`-shaped), not display strings; symbols are a
//!   secondary index.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Decimal-string codec for `u128` — same wire convention as
/// `sigil_state::u128_str` (duplicated here because sigil-dex deliberately
/// has no sigil-state dependency). Keeps JSON clients (JS wallets) from
/// silently truncating 128-bit supplies.
pub mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    /// Serialize a `u128` as its decimal string.
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    /// Deserialize a `u128` from its decimal string.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 32-byte token identifier (mirrors `sigil_state::TokenId` without the dep —
/// sigil-dex stays dependency-free).
pub type TokenId = [u8; 32];
/// 32-byte pool identifier.
pub type PoolId = [u8; 32];
/// 32-byte wallet address.
pub type WalletId = [u8; 32];

/// The all-zero native-SIGIL token sentinel (same value as
/// `sigil_state::NATIVE`).
pub const NATIVE_TOKEN: TokenId = [0u8; 32];

/// Token identity + lifecycle metadata. The registry's unit of record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMetadata {
    /// Chain token id (the wallet-SMT key).
    pub token_id: TokenId,
    /// Display symbol, e.g. `"SSHARE"`. Unique within a registry.
    pub symbol: String,
    /// Human name.
    pub name: String,
    /// Base-10 decimals (SIGIL convention: 8).
    pub decimals: u8,
    /// Total supply in base units at registration / last update.
    #[serde(with = "u128_str")]
    pub total_supply: u128,
    /// Creator wallet.
    pub creator: WalletId,
    /// Block height the token was registered at.
    pub created_at_height: u64,
    /// Active (tradeable/listed) flag.
    pub is_active: bool,
    /// Whether at least one pool includes this token (maintained by
    /// [`TokenRegistry::register_pool`]).
    pub has_liquidity_pool: bool,
    /// Pools this token is part of.
    pub liquidity_pools: Vec<PoolId>,
    /// Optional freeform description.
    pub description: Option<String>,
    /// Optional project website.
    pub website: Option<String>,
}

/// Pool identity metadata — the registry-side mirror of the committed
/// `sigil_state::PoolState` (which owns the live reserves; this struct
/// deliberately does NOT duplicate them — one source of truth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMetadata {
    /// Chain pool id.
    pub pool_id: PoolId,
    /// Display pair, e.g. `"SSHARE/SIGIL"`.
    pub pair: String,
    /// Token-A id (the side hashed first into the pool id).
    pub token_a: TokenId,
    /// Token-B id.
    pub token_b: TokenId,
    /// Per-swap fee in basis points.
    pub fee_bps: u16,
    /// Block height the pool was registered at.
    pub created_at_height: u64,
    /// Creator wallet.
    pub creator: WalletId,
    /// Active flag.
    pub is_active: bool,
}

/// Registry errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A token with this id is already registered.
    #[error("token id already registered")]
    DuplicateToken,
    /// Another token already owns this symbol.
    #[error("symbol already registered to a different token")]
    DuplicateSymbol,
    /// A pool with this id is already registered.
    #[error("pool id already registered")]
    DuplicatePool,
    /// A referenced token is not in the registry.
    #[error("unknown token")]
    UnknownToken,
}

/// Serde codec for `BTreeMap<[u8;32], V>` as a sequence of pairs —
/// serde_json refuses non-string map keys ("key must be a string"), so the
/// byte-keyed maps go over the wire as `[(key, value), …]` instead.
mod map32 {
    use super::*;
    pub fn serialize<S: serde::Serializer, V: Serialize>(
        map: &BTreeMap<[u8; 32], V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(map.iter())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>, V: Deserialize<'de>>(
        d: D,
    ) -> Result<BTreeMap<[u8; 32], V>, D::Error> {
        let v: Vec<([u8; 32], V)> = Vec::deserialize(d)?;
        Ok(v.into_iter().collect())
    }
}

/// The registry: tokens, pools, and a symbol index. Plain data — serialize
/// it into the node's state snapshot for persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenRegistry {
    #[serde(with = "map32")]
    tokens: BTreeMap<TokenId, TokenMetadata>,
    #[serde(with = "map32")]
    pools: BTreeMap<PoolId, PoolMetadata>,
    symbols: BTreeMap<String, TokenId>,
}

impl TokenRegistry {
    /// Empty registry with the native SIGIL token pre-registered (height 0,
    /// genesis-creator zero wallet) — every chain has it, so every registry
    /// starts with it.
    pub fn new() -> Self {
        let mut r = Self::default();
        r.register_token(TokenMetadata {
            token_id: NATIVE_TOKEN,
            symbol: "SIGIL".to_string(),
            name: "SIGIL".to_string(),
            decimals: 8,
            total_supply: 0, // live native supply is chain state, not registry state
            creator: [0u8; 32],
            created_at_height: 0,
            is_active: true,
            has_liquidity_pool: false,
            liquidity_pools: Vec::new(),
            description: Some("Native SIGIL token".to_string()),
            website: None,
        })
        .expect("registering NATIVE into an empty registry cannot fail");
        r
    }

    // ── token operations ───────────────────────────────────────────────────

    /// Register a new token. Rejects duplicate ids and duplicate symbols.
    pub fn register_token(&mut self, token: TokenMetadata) -> Result<(), RegistryError> {
        if self.tokens.contains_key(&token.token_id) {
            return Err(RegistryError::DuplicateToken);
        }
        if let Some(existing) = self.symbols.get(&token.symbol) {
            if *existing != token.token_id {
                return Err(RegistryError::DuplicateSymbol);
            }
        }
        self.symbols.insert(token.symbol.clone(), token.token_id);
        self.tokens.insert(token.token_id, token);
        Ok(())
    }

    /// Token by chain id.
    pub fn token(&self, id: &TokenId) -> Option<&TokenMetadata> {
        self.tokens.get(id)
    }

    /// Token by display symbol.
    pub fn token_by_symbol(&self, symbol: &str) -> Option<&TokenMetadata> {
        self.symbols.get(symbol).and_then(|id| self.tokens.get(id))
    }

    /// All registered tokens, newest registration first.
    pub fn all_tokens(&self) -> Vec<&TokenMetadata> {
        let mut v: Vec<&TokenMetadata> = self.tokens.values().collect();
        v.sort_by(|a, b| b.created_at_height.cmp(&a.created_at_height));
        v
    }

    /// Active tokens only.
    pub fn active_tokens(&self) -> Vec<&TokenMetadata> {
        self.all_tokens().into_iter().filter(|t| t.is_active).collect()
    }

    /// Apply an in-place metadata update (supply refresh, deactivation, …).
    pub fn update_token(
        &mut self,
        id: &TokenId,
        update_fn: impl FnOnce(&mut TokenMetadata),
    ) -> Result<(), RegistryError> {
        let token = self.tokens.get_mut(id).ok_or(RegistryError::UnknownToken)?;
        let old_symbol = token.symbol.clone();
        update_fn(token);
        // Keep the symbol index coherent if the update renamed the token.
        if token.symbol != old_symbol {
            let new_symbol = token.symbol.clone();
            self.symbols.remove(&old_symbol);
            self.symbols.insert(new_symbol, *id);
        }
        Ok(())
    }

    // ── pool operations ────────────────────────────────────────────────────

    /// Register a pool. Both tokens must already be registered; their
    /// `liquidity_pools` associations are maintained here (the q-storage
    /// `associate_pool_with_tokens` behavior).
    pub fn register_pool(&mut self, pool: PoolMetadata) -> Result<(), RegistryError> {
        if self.pools.contains_key(&pool.pool_id) {
            return Err(RegistryError::DuplicatePool);
        }
        if !self.tokens.contains_key(&pool.token_a) || !self.tokens.contains_key(&pool.token_b) {
            return Err(RegistryError::UnknownToken);
        }
        for side in [pool.token_a, pool.token_b] {
            let t = self.tokens.get_mut(&side).expect("checked above");
            t.has_liquidity_pool = true;
            if !t.liquidity_pools.contains(&pool.pool_id) {
                t.liquidity_pools.push(pool.pool_id);
            }
        }
        self.pools.insert(pool.pool_id, pool);
        Ok(())
    }

    /// Pool by chain id.
    pub fn pool(&self, id: &PoolId) -> Option<&PoolMetadata> {
        self.pools.get(id)
    }

    /// All pools, newest first.
    pub fn all_pools(&self) -> Vec<&PoolMetadata> {
        let mut v: Vec<&PoolMetadata> = self.pools.values().collect();
        v.sort_by(|a, b| b.created_at_height.cmp(&a.created_at_height));
        v
    }

    /// Every pool that includes `token`.
    pub fn pools_for_token(&self, token: &TokenId) -> Vec<&PoolMetadata> {
        match self.tokens.get(token) {
            Some(t) => t
                .liquidity_pools
                .iter()
                .filter_map(|pid| self.pools.get(pid))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Counts: (tokens, pools).
    pub fn len(&self) -> (usize, usize) {
        (self.tokens.len(), self.pools.len())
    }

    /// True if no tokens are registered (a `new()` registry is never empty —
    /// it holds NATIVE).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSHARE_ID: TokenId = [0x55; 32];
    const CREATOR: WalletId = [0x11; 32];
    const POOL_ID: PoolId = [0x77; 32];

    fn sshare() -> TokenMetadata {
        TokenMetadata {
            token_id: SSHARE_ID,
            symbol: "SSHARE".to_string(),
            name: "SIGIL Treasury Share".to_string(),
            decimals: 8,
            total_supply: 1_000 * 100_000_000,
            creator: CREATOR,
            created_at_height: 42,
            is_active: true,
            has_liquidity_pool: false,
            liquidity_pools: vec![],
            description: None,
            website: None,
        }
    }

    #[test]
    fn native_is_preregistered() {
        let r = TokenRegistry::new();
        let native = r.token_by_symbol("SIGIL").expect("NATIVE pre-registered");
        assert_eq!(native.token_id, NATIVE_TOKEN);
        assert_eq!(native.decimals, 8);
        assert_eq!(r.len(), (1, 0));
    }

    #[test]
    fn token_registration_and_lookup() {
        // Port of q-storage test_token_registration.
        let mut r = TokenRegistry::new();
        r.register_token(sshare()).unwrap();

        let by_id = r.token(&SSHARE_ID).expect("by id");
        assert_eq!(by_id.symbol, "SSHARE");
        let by_symbol = r.token_by_symbol("SSHARE").expect("by symbol");
        assert_eq!(by_symbol.token_id, SSHARE_ID);
        assert_eq!(r.all_tokens()[0].symbol, "SSHARE", "newest first");
    }

    #[test]
    fn duplicate_token_and_symbol_rejected() {
        let mut r = TokenRegistry::new();
        r.register_token(sshare()).unwrap();
        assert_eq!(r.register_token(sshare()), Err(RegistryError::DuplicateToken));
        let mut imposter = sshare();
        imposter.token_id = [0x99; 32];
        assert_eq!(r.register_token(imposter), Err(RegistryError::DuplicateSymbol));
    }

    #[test]
    fn pool_registration_associates_tokens() {
        // Port of q-storage test_pool_registration.
        let mut r = TokenRegistry::new();
        r.register_token(sshare()).unwrap();
        r.register_pool(PoolMetadata {
            pool_id: POOL_ID,
            pair: "SSHARE/SIGIL".to_string(),
            token_a: NATIVE_TOKEN,
            token_b: SSHARE_ID,
            fee_bps: 30,
            created_at_height: 43,
            creator: CREATOR,
            is_active: true,
        })
        .unwrap();

        assert_eq!(r.pool(&POOL_ID).unwrap().pair, "SSHARE/SIGIL");
        let t = r.token(&SSHARE_ID).unwrap();
        assert!(t.has_liquidity_pool);
        assert_eq!(t.liquidity_pools, vec![POOL_ID]);
        assert_eq!(r.pools_for_token(&SSHARE_ID).len(), 1);
        assert_eq!(r.pools_for_token(&NATIVE_TOKEN).len(), 1);
    }

    #[test]
    fn pool_against_unregistered_token_rejected() {
        let mut r = TokenRegistry::new();
        let err = r.register_pool(PoolMetadata {
            pool_id: POOL_ID,
            pair: "GHOST/SIGIL".to_string(),
            token_a: NATIVE_TOKEN,
            token_b: [0xEE; 32],
            fee_bps: 30,
            created_at_height: 1,
            creator: CREATOR,
            is_active: true,
        });
        assert_eq!(err, Err(RegistryError::UnknownToken));
    }

    #[test]
    fn update_token_keeps_symbol_index_coherent() {
        let mut r = TokenRegistry::new();
        r.register_token(sshare()).unwrap();
        r.update_token(&SSHARE_ID, |t| t.total_supply = 7).unwrap();
        assert_eq!(r.token(&SSHARE_ID).unwrap().total_supply, 7);
        assert!(matches!(
            r.update_token(&[0xAB; 32], |_| {}),
            Err(RegistryError::UnknownToken)
        ));
    }

    #[test]
    fn registry_snapshot_roundtrips() {
        // Persistence contract: the registry serializes losslessly so the
        // node can embed it in its flux-db state snapshot.
        let mut r = TokenRegistry::new();
        r.register_token(sshare()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: TokenRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token(&SSHARE_ID), r.token(&SSHARE_ID));
        assert_eq!(back.len(), r.len());
    }
}
