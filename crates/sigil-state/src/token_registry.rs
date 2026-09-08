//! The on-chain token registry — `TokenId -> {symbol, name, decimals, supply, deployer}`.
//!
//! ## Why this exists
//!
//! SIGIL has been a multi-token chain at the balance layer since genesis:
//! [`SigilState::wallets`](crate::SigilState) is keyed `(WalletId, TokenId)` and every
//! `SigilTx::Send`/`Swap`/`LpDeposit` names a token. What it did NOT have was any way to
//! learn what a token IS. `SigilTx::TokenDeploy` emitted an event and left nothing
//! behind, so `sigil-api`'s `display_symbol()` fell back to the first eight hex
//! characters of the id and said so in its own doc comment:
//!
//! > *"There is no persistent token-ticker registry on chain yet — `TokenDeploy` only
//! > emits an event, it doesn't leave a queryable `TokenId -> ticker` mapping behind (a
//! > real follow-on, not invented here)."*
//!
//! This is that follow-on. A wallet holding the operator's CHAD token showed `1421AA72`;
//! now it shows `CHAD`.
//!
//! ## Why it lives in CONTRACT STORAGE and not a new field
//!
//! A new `BTreeMap` on `SigilState` would need its own root, and a fifth root means a
//! header change — a hard fork, for a lookup table. Contract storage already exists,
//! already has an accumulator (`contract_acc`), and already lands in
//! `contract_state_root`. Writing the registry through the ordinary
//! [`StateMutation::SetContractSlot`](crate::StateMutation::SetContractSlot) therefore
//! gets consensus coverage and cross-node divergence detection for free, with no change
//! to the header, no change to `StateRoots`, and no new accumulator to keep in sync.
//!
//! **And it cannot fork the chain.** `SigilTx::TokenDeploy` has never been applied on
//! `sigil-g2` — no node or API path has ever constructed one — so every historical block
//! contains zero registry writes and every historical `contract_state_root` is unchanged
//! by this code existing. A handler for a transaction variant that has never been used
//! reinterprets nothing. (Verified 2026-09-07 by grepping every caller in `sigil-node`
//! and `sigil-api` before writing a line of this.)
//!
//! ## Layout
//!
//! Four 32-byte slots per token, under one reserved contract id. Slot keys are
//! domain-separated hashes of the token id, so two tokens can never collide and a token
//! cannot be made to overwrite another's metadata by choosing its id.

use crate::{ContractId, SlotId, StateMutation, TokenId, WalletId};

/// The reserved contract id the registry lives under.
///
/// A hash of a domain string rather than a low-numbered constant like `[1u8; 32]`: a
/// real contract could plausibly be deployed at a hand-picked pretty address, and the
/// registry sharing storage with it would let contract writes forge token metadata.
/// Nothing can deploy to a preimage of this string without inverting BLAKE3.
pub const TOKEN_REGISTRY_CONTRACT: ContractId = registry_contract();

/// Field indices within a token's slot group. Append only — an existing index must never
/// be reused for a different meaning, or old rows silently change interpretation.
const F_SYMBOL: u8 = 0;
const F_NAME: u8 = 1;
const F_PACKED: u8 = 2; // decimals (1 byte) ‖ initial supply (16 bytes, big-endian u128)
const F_DEPLOYER: u8 = 3;

/// Longest ticker/name the registry stores. Both are held in ONE 32-byte slot, so this
/// is a hard limit rather than a style guide: a longer string is rejected at deploy time
/// instead of being silently truncated into a different token's name.
pub const MAX_TEXT: usize = 31;

/// What the chain knows about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// Display ticker, e.g. `"CHAD"`.
    pub symbol: String,
    /// Longer display name; equals `symbol` when the deployer gave only a ticker.
    pub name: String,
    /// Decimal places.
    pub decimals: u8,
    /// Supply minted at deploy. NOT a live total — a token whose supply can change must
    /// be summed from balances, and calling this "totalSupply" would be a lie the moment
    /// anything mints.
    pub initial_supply: u128,
    /// Wallet that deployed it and received the initial supply.
    pub deployer: WalletId,
}

/// Why a `TokenDeploy` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// Ticker was empty, over [`MAX_TEXT`] bytes, or held a byte outside printable ASCII.
    BadSymbol(String),
    /// Name was over [`MAX_TEXT`] bytes or held a byte outside printable ASCII.
    BadName(String),
    /// A token is already registered at this id.
    AlreadyRegistered(TokenId),
    /// The all-zero token id is native SIGIL and can never be re-registered.
    ReservedNativeId,
}

impl core::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadSymbol(s) => write!(f, "bad ticker {s:?}: must be 1..={MAX_TEXT} printable ASCII bytes"),
            Self::BadName(s) => write!(f, "bad name {s:?}: must be at most {MAX_TEXT} printable ASCII bytes"),
            Self::AlreadyRegistered(t) => write!(f, "token {} is already registered", hex8(t)),
            Self::ReservedNativeId => write!(f, "the all-zero token id is native SIGIL and is reserved"),
        }
    }
}
impl std::error::Error for RegistryError {}

/// The slot a given field of a given token occupies.
///
/// `blake3(domain ‖ token ‖ field)`. Domain-separated so a slot in this contract can
/// never be reached by any other derivation, and token-hashed so the four fields of one
/// token are scattered rather than adjacent — adjacency would let a caller who can write
/// one slot guess its neighbours.
pub fn slot_for(token: &TokenId, field: u8) -> SlotId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-token-registry-v1/slot");
    h.update(token);
    h.update(&[field]);
    *h.finalize().as_bytes()
}

/// Pack a short string into one 32-byte slot: `len` then the bytes, zero-padded.
fn pack_text(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    out[0] = b.len() as u8;
    out[1..1 + b.len()].copy_from_slice(b);
    out
}

fn unpack_text(v: &[u8; 32]) -> String {
    let n = (v[0] as usize).min(MAX_TEXT);
    String::from_utf8_lossy(&v[1..1 + n]).into_owned()
}

fn pack_decimals_supply(decimals: u8, supply: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = decimals;
    out[1..17].copy_from_slice(&supply.to_be_bytes());
    out
}

fn unpack_decimals_supply(v: &[u8; 32]) -> (u8, u128) {
    let mut s = [0u8; 16];
    s.copy_from_slice(&v[1..17]);
    (v[0], u128::from_be_bytes(s))
}

fn valid_text(s: &str, max_empty_ok: bool) -> bool {
    if s.len() > MAX_TEXT || (!max_empty_ok && s.is_empty()) {
        return false;
    }
    // Printable ASCII only. A ticker carrying control characters, a right-to-left
    // override or zero-width joiners is a phishing primitive: two tokens that render
    // identically in a wallet list while being different assets.
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// The mutations that register `token`. Pure — it reads nothing and writes nothing;
/// the caller feeds the result through `commit_state_transition` like any other change.
///
/// `existing` is what [`lookup`] currently returns for this id, so the duplicate check
/// happens against real state rather than on trust.
#[allow(clippy::too_many_arguments)]
pub fn register_mutations(
    token: &TokenId,
    symbol: &str,
    name: &str,
    decimals: u8,
    initial_supply: u128,
    deployer: &WalletId,
    existing: Option<&TokenInfo>,
) -> Result<Vec<StateMutation>, RegistryError> {
    if token == &[0u8; 32] {
        return Err(RegistryError::ReservedNativeId);
    }
    if existing.is_some() {
        return Err(RegistryError::AlreadyRegistered(*token));
    }
    if !valid_text(symbol, false) {
        return Err(RegistryError::BadSymbol(symbol.to_string()));
    }
    let name = if name.is_empty() { symbol } else { name };
    if !valid_text(name, true) {
        return Err(RegistryError::BadName(name.to_string()));
    }
    let set = |field: u8, value: [u8; 32]| StateMutation::SetContractSlot {
        contract: TOKEN_REGISTRY_CONTRACT,
        slot: slot_for(token, field),
        value,
    };
    Ok(vec![
        set(F_SYMBOL, pack_text(symbol)),
        set(F_NAME, pack_text(name)),
        set(F_PACKED, pack_decimals_supply(decimals, initial_supply)),
        set(F_DEPLOYER, *deployer),
    ])
}

/// Read a token back out of contract storage. `read` is any slot reader — in production
/// `|c, s| state.contract_slot(c, s)`; in tests a plain map.
pub fn lookup<F>(token: &TokenId, read: F) -> Option<TokenInfo>
where
    F: Fn(&ContractId, &SlotId) -> Option<[u8; 32]>,
{
    let sym = read(&TOKEN_REGISTRY_CONTRACT, &slot_for(token, F_SYMBOL))?;
    // An all-zero symbol slot means "never written", not "a token named empty string":
    // unwritten contract storage reads as zeroes, so without this an unregistered token
    // would come back as a real token with a blank ticker.
    if sym[0] == 0 {
        return None;
    }
    let name = read(&TOKEN_REGISTRY_CONTRACT, &slot_for(token, F_NAME)).unwrap_or([0u8; 32]);
    let packed = read(&TOKEN_REGISTRY_CONTRACT, &slot_for(token, F_PACKED)).unwrap_or([0u8; 32]);
    let deployer = read(&TOKEN_REGISTRY_CONTRACT, &slot_for(token, F_DEPLOYER)).unwrap_or([0u8; 32]);
    let (decimals, initial_supply) = unpack_decimals_supply(&packed);
    let symbol = unpack_text(&sym);
    Some(TokenInfo {
        name: if name[0] == 0 { symbol.clone() } else { unpack_text(&name) },
        symbol,
        decimals,
        initial_supply,
        deployer,
    })
}

/// `"CHAD"` when the token is registered, `"SIGIL"` for the native id, and the old
/// eight-hex placeholder when it is not — so an unregistered token degrades to exactly
/// the behaviour that existed before this module, never to a blank or a panic.
pub fn display_symbol<F>(token: &TokenId, read: F) -> String
where
    F: Fn(&ContractId, &SlotId) -> Option<[u8; 32]>,
{
    if token == &[0u8; 32] {
        return "SIGIL".to_string();
    }
    match lookup(token, read) {
        Some(i) => i.symbol,
        None => hex8(token).to_uppercase(),
    }
}

/// First four bytes as hex. Local rather than via the `hex` crate, which this crate
/// only carries as a dev-dependency — pulling it into the lib for two format strings
/// would be a real dependency for a cosmetic reason.
fn hex8(t: &TokenId) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(8);
    for b in &t[..4] {
        out.push(D[(b >> 4) as usize] as char);
        out.push(D[(b & 0x0f) as usize] as char);
    }
    out
}

/// BLAKE3 of the domain string, pinned as a constant because `blake3` cannot run in a
/// `const fn`. Never edit this by hand — `registry_contract_id_matches_domain` recomputes
/// it at run time and fails if the two ever disagree.
const fn registry_contract() -> ContractId {
    [
        0xeb, 0x48, 0xef, 0xed, 0x18, 0x8a, 0xbe, 0xb2,
        0x66, 0x72, 0xa4, 0x7a, 0x6e, 0x8e, 0x09, 0xd4,
        0x29, 0x76, 0x9f, 0x66, 0x04, 0xb1, 0x23, 0x3f,
        0x15, 0xcb, 0xfd, 0x75, 0xb6, 0x54, 0xcc, 0xff,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn store() -> BTreeMap<(ContractId, SlotId), [u8; 32]> { BTreeMap::new() }
    fn apply(m: &[StateMutation], into: &mut BTreeMap<(ContractId, SlotId), [u8; 32]>) {
        for mu in m {
            if let StateMutation::SetContractSlot { contract, slot, value } = mu {
                into.insert((*contract, *slot), *value);
            }
        }
    }
    fn reader(m: &BTreeMap<(ContractId, SlotId), [u8; 32]>)
        -> impl Fn(&ContractId, &SlotId) -> Option<[u8; 32]> + '_ {
        move |c, s| m.get(&(*c, *s)).copied()
    }

    /// The pinned constant must really be BLAKE3 of the domain string. If someone edits
    /// one without the other, every slot key silently moves and the registry appears to
    /// lose every token at once.
    #[test]
    fn registry_contract_id_matches_domain() {
        let want = *blake3::hash(b"sigil-token-registry-v1/contract").as_bytes();
        assert_eq!(TOKEN_REGISTRY_CONTRACT, want, "pinned contract id no longer matches its domain string");
    }

    #[test]
    fn chad_round_trips() {
        let token = [0x14u8; 32];
        let deployer = [0xd7u8; 32];
        let mut st = store();
        let m = register_mutations(&token, "CHAD", "CHAD", 18, 1_035_923_596_588_534_750_000_000_000_000_000, &deployer, None).unwrap();
        apply(&m, &mut st);
        let got = lookup(&token, reader(&st)).expect("registered");
        assert_eq!(got.symbol, "CHAD");
        assert_eq!(got.name, "CHAD");
        assert_eq!(got.decimals, 18);
        assert_eq!(got.initial_supply, 1_035_923_596_588_534_750_000_000_000_000_000);
        assert_eq!(got.deployer, deployer);
        assert_eq!(display_symbol(&token, reader(&st)), "CHAD");
    }

    /// The whole point: before, this returned the first eight hex characters.
    #[test]
    fn unregistered_token_keeps_the_old_placeholder() {
        let st = store();
        let token = [0x14u8; 32];
        assert_eq!(display_symbol(&token, reader(&st)), "14141414");
        assert!(lookup(&token, reader(&st)).is_none());
    }

    /// Unwritten contract storage reads as zeroes. Without the `sym[0] == 0` guard an
    /// unregistered token would come back as a real token with a blank ticker.
    #[test]
    fn all_zero_slots_are_absent_not_a_blank_token() {
        let mut st = store();
        st.insert((TOKEN_REGISTRY_CONTRACT, slot_for(&[7u8; 32], 0)), [0u8; 32]);
        assert!(lookup(&[7u8; 32], reader(&st)).is_none());
    }

    #[test]
    fn native_is_always_sigil_and_can_never_be_registered() {
        let st = store();
        assert_eq!(display_symbol(&[0u8; 32], reader(&st)), "SIGIL");
        assert_eq!(
            register_mutations(&[0u8; 32], "FAKE", "", 8, 1, &[1u8; 32], None).unwrap_err(),
            RegistryError::ReservedNativeId
        );
    }

    #[test]
    fn a_token_cannot_be_registered_twice() {
        let token = [0x14u8; 32];
        let existing = TokenInfo { symbol: "CHAD".into(), name: "CHAD".into(), decimals: 18,
                                   initial_supply: 1, deployer: [0u8; 32] };
        assert_eq!(
            register_mutations(&token, "EVIL", "", 18, 1, &[9u8; 32], Some(&existing)).unwrap_err(),
            RegistryError::AlreadyRegistered(token)
        );
    }

    /// A ticker with a right-to-left override or zero-width joiner renders identically to
    /// another token in a wallet list. That is a phishing primitive, not a style problem.
    #[test]
    fn tickers_are_printable_ascii_only() {
        for bad in ["", "\u{202e}DAHC", "CH\u{200b}AD", "a".repeat(32).as_str(), "CHAD\n"] {
            assert!(
                register_mutations(&[1u8; 32], bad, "", 18, 1, &[0u8; 32], None).is_err(),
                "ticker {bad:?} should have been refused"
            );
        }
        assert!(register_mutations(&[1u8; 32], "a".repeat(31).as_str(), "", 18, 1, &[0u8; 32], None).is_ok());
    }

    /// Two tokens must never share a slot, and the four fields of one token must differ.
    #[test]
    fn slots_never_collide() {
        let mut seen = std::collections::HashSet::new();
        for t in 0u8..24 {
            for f in 0u8..4 {
                assert!(seen.insert(slot_for(&[t; 32], f)), "slot collision at token {t} field {f}");
            }
        }
    }

    /// The largest supply the chain can express must survive the pack/unpack.
    #[test]
    fn supply_packs_losslessly_at_the_extremes() {
        for v in [0u128, 1, u128::MAX, 1_035_923_596_588_534_750_000_000_000_000_000] {
            let p = pack_decimals_supply(255, v);
            assert_eq!(unpack_decimals_supply(&p), (255, v));
        }
    }
}
