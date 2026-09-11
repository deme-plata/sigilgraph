//! sigil-usds — USDS (usdSIGIL), SIGIL's native $-pegged stablecoin.
//!
//! Mint: lock `sigil_amount` of NATIVE SIGIL into the [`VAULT`] and receive
//! USDS worth SLIGHTLY LESS than the locked USD value — the gap is
//! [`MINT_BUFFER_BPS`], a collateral cushion, not a fee (the fee is separate,
//! see below). Redeem: burn USDS, release the SIGIL at the current price.
//!
//! ## Why a buffer (2026-08-18 design decision)
//!
//! The original version of this module minted USDS worth EXACTLY the locked
//! value (1:1, no cushion) — safe from any leverage/liquidation bug class,
//! but with one real gap: if SIGIL's price falls between one user's mint and
//! a LATER user's redemption, the vault can come up short (the same failure
//! mode QUGUSD defends against with a 135% collateral ratio — see
//! `MANDAT_QUILLON_BANK_KREDIT.md` era research). Rather than copy QUGUSD's
//! full CDP + liquidation machinery (more code, more risk surface, and two
//! independent mint paths is exactly the bug class that hurt QUGUSD — see
//! the 2026-08-18 design review), USDS keeps its single, simple mint/redeem
//! path and adds a modest FIXED cushion instead: every mint locks
//! [`MINT_BUFFER_BPS`] (105%) of the USD value it issues, so the vault
//! always holds a same-day margin against a price move, without any
//! per-position liquidation logic to get wrong.
//!
//! ## Why a fee (same design decision)
//!
//! Every mint and redeem also pays [`sigil_bank::MASTER_SWAP_FEE_BPS`] (the
//! SAME protocol fee already charged on DEX swaps, reused rather than
//! inventing a new constant) via [`sigil_bank::split_swap_output`] — the
//! master wallet earns a small, consistent cut of stablecoin activity, same
//! as it does on trading activity. This is deliberately the SAME function
//! swaps already use, not a parallel implementation, so a future change to
//! the protocol fee rate updates both paths at once.
//!
//! Invariants:
//! - **NATIVE is conserved.** Minting MOVES SIGIL user→vault (it is never
//!   created or destroyed), so the 21M cap is untouched and the chokepoint's
//!   supply check is a no-op delta. The redeem-side fee moves a sliver of
//!   that same NATIVE from vault→master wallet — still conserved, just
//!   reassigned.
//! - **USDS supply tracks collateral, with a margin.** USDS is a separate
//!   (uncapped) token; its balance grows on mint and shrinks on redeem,
//!   backed by AT LEAST its face value in vault SIGIL at issue time (105% of
//!   it, before any price movement).
//! - **Everything is committed in roots** via `commit_state_transition` — no
//!   side ledger (the Quillon-postmortem discipline).
//!
//! Units: SIGIL price is USD×1e8 per WHOLE SIGIL (`sigil_oracle::PRICE_SCALE`);
//! USDS has 8 decimals ($1 == 1e8 base); NATIVE amounts are g2 GLYPHS
//! (1 SIGIL == 10^10 glyphs, [`GLYPHS_PER_SIGIL`]). glyphs → USD-e8 is
//! `glyphs × price / GLYPHS_PER_SIGIL`. ⚠️ The original 2026-08-18 version
//! divided by `PRICE_SCALE` here — correct on g1's 8-decimal chain, a silent
//! 100× over-mint on g2's 10-decimal chain. Fixed 2026-08-31 before the
//! module's first live use (the oracle had never been fed, so no mis-scaled
//! mint ever landed — verified `usds_supply == 0` on the live node).

//!
//! ## Going live (2026-09-11)
//!
//! Everything above was committed on 2026-08-18/31 and had never run: the live
//! node showed `price 0 / vault 0 / supply 0` because (a) the price could only
//! be pushed by the operator's own key, (b) a USDS balance could not be MOVED
//! (`SHIELDED_ONLY_HEIGHT == 0` retires every transparent `Send`, token or
//! not), and (c) nothing tracked how much USDS existed. Three consensus
//! changes fix that, all switched on together at [`USDS_LIVE_HEIGHT`] so that
//! every node flips at the same block and the live producer is never the
//! first to emit the new shape:
//!
//! 1. `sigil-tx` accepts a transparent `Send` whose token is NOT native from
//!    that height (native stays shielded-only — privacy is about SIGIL, and
//!    USDS is a dollar IOU with no shielded representation).
//! 2. The master may delegate price pushes (`sigil_oracle::FEEDER_SLOT`);
//!    every push is height-stamped and a stale price refuses to mint/redeem
//!    ([`UsdsError::StalePrice`]) — fail closed, exactly like `NoPrice`.
//! 3. Total USDS supply is committed in [`SUPPLY_SLOT`] on every mint/redeem
//!    (the O(1) counter `native_supply` already gives NATIVE), so
//!    `/v1/usds/status` reports a number that is in the roots, not a guess.

use sigil_bank::{split_swap_output, BankError, DEV_MASTER_WALLET};
use sigil_oracle::{price_is_fresh, read_price, read_price_height, PRICE_SCALE, MAX_PRICE_AGE_BLOCKS};
use sigil_state::{
    commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation,
    StateTransition, TokenId, WalletId, NATIVE,
};

/// USDS token id.
pub const USDS: TokenId = [0xD5; 32];
/// The collateral vault that holds locked SIGIL backing the USDS supply.
pub const VAULT: WalletId = [0x0B; 32];
/// The USDS module's own contract namespace — holds the committed supply.
pub const USDS_CONTRACT: ContractId = [0x0D; 32];
/// Slot holding total circulating USDS (LE u128 in the first 16 bytes).
/// Every mint adds to it, every redeem subtracts, in the SAME transition as
/// the balance writes — so it can never drift from the balances it counts.
pub const SUPPLY_SLOT: SlotId = [0x01; 32];

/// The block from which USDS is LIVE: token transfers, oracle delegation,
/// height-stamped pushes, supply tracking and the freshness rule all switch
/// on here, on every node at once. Chosen 2026-09-11 at height 6,027,010
/// with ~15 h of margin (6.6 blk/s) to deploy the follower (happysrv) first
/// and the Epsilon producer second — a producer that reaches this height on
/// the OLD binary simply never emits the new shape (safe), while a follower
/// on the old binary would fork on the first USDS block (not safe), which is
/// why the order matters.
pub const USDS_LIVE_HEIGHT: u64 = 6_400_000;

/// Is the USDS feature set active at `height`?
pub fn usds_active(height: u64) -> bool {
    height >= USDS_LIVE_HEIGHT
}

/// Collateral buffer over 1:1 value, in the same basis-point convention
/// `sigil_bank` uses (`10_000` = 100%). **`10_500` = 105%**: locking $X of
/// SIGIL mints `$X / 1.05` of USDS, leaving the remaining ~4.76% of the
/// locked value sitting in the vault as a cushion against SIGIL's price
/// falling before a later redemption. Fixed at genesis, like every other
/// protocol-rate constant in this codebase — a future consensus upgrade can
/// change it.
pub const MINT_BUFFER_BPS: u128 = 10_500;

/// g2 native decimals: 1 SIGIL = 10^10 glyphs. Every NATIVE amount in this
/// module is glyphs; every USD value is USDS-base (1e8 = $1); the oracle
/// price is USD×1e8 per WHOLE SIGIL. This constant exists so the glyph↔USD
/// conversion is written once — dividing by `PRICE_SCALE` instead (the g1
/// 8-decimal formula) over-values SIGIL 100× and is exactly the bug class
/// the Android-wallet "8dp on a 10dp chain" note warns about.
pub const GLYPHS_PER_SIGIL: u128 = 10_000_000_000;

#[derive(Debug, thiserror::Error)]
pub enum UsdsError {
    #[error("oracle price is unset (0) — cannot mint/redeem")]
    NoPrice,
    #[error("oracle price is stale: pushed at height {pushed_at}, now {at_height}, max age {max_age} blocks — cannot mint/redeem")]
    StalePrice { pushed_at: u64, at_height: u64, max_age: u64 },
    #[error("amount is zero")]
    ZeroAmount,
    #[error("insufficient SIGIL collateral")]
    InsufficientSigil,
    #[error("insufficient USDS balance")]
    InsufficientUsds,
    #[error("vault underfunded for this redemption")]
    VaultUnderfunded,
    #[error("welfare payer holds {have} glyphs but the mint needs {need} (collateral + fee)")]
    WelfarePayerUnderfunded { have: u128, need: u128 },
    #[error("arithmetic overflow")]
    Overflow,
    #[error("protocol fee split: {0}")]
    Fee(#[from] BankError),
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// The committed total USDS supply. `0` if nothing was ever minted.
pub fn read_supply(state: &SigilState) -> u128 {
    let v = state.contract_slot(&USDS_CONTRACT, &SUPPLY_SLOT);
    let mut b = [0u8; 16];
    b.copy_from_slice(&v[..16]);
    u128::from_le_bytes(b)
}

/// The mutation that sets the committed supply to `new_supply`.
fn supply_mutation(new_supply: u128) -> StateMutation {
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&new_supply.to_le_bytes());
    StateMutation::SetContractSlot { contract: USDS_CONTRACT, slot: SUPPLY_SLOT, value }
}

/// The oracle price usable at `at_height`, or the exact reason it is not.
/// `NoPrice` when nothing was ever pushed; `StalePrice` when the last push is
/// older than `MAX_PRICE_AGE_BLOCKS` (or carries no height stamp at all —
/// the pre-activation shape, whose age is unknowable and therefore refused).
fn usable_price(state: &SigilState, at_height: u64) -> Result<u128, UsdsError> {
    let price = read_price(state);
    if price == 0 {
        return Err(UsdsError::NoPrice);
    }
    if !price_is_fresh(state, at_height) {
        return Err(UsdsError::StalePrice {
            pushed_at: read_price_height(state),
            at_height,
            max_age: MAX_PRICE_AGE_BLOCKS,
        });
    }
    Ok(price)
}

/// Pure outcome of [`plan_mint`] — the mutations it would take, and how much
/// USDS the user would actually receive, WITHOUT having committed anything.
#[derive(Debug, Clone)]
pub struct MintPlan {
    /// USDS that would be credited to the user (after buffer + fee).
    pub usds_to_user: u128,
    /// The exact mutations [`mint`] (or a caller integrating USDS into a
    /// larger batched transition, e.g. `sigil-tx::apply_tx`) must commit.
    pub mutations: Vec<StateMutation>,
}

/// Plan a mint: lock `sigil_amount` of NATIVE into the vault, work out how
/// much USDS that yields after the buffer + protocol fee — but do NOT touch
/// storage. Read-only (`&SigilState`), so this is safe to call from inside
/// `sigil-tx::apply_tx`'s immutable-state pass, exactly like `sigil_dex::swap`
/// already is for `SigilTx::Swap`. [`mint`] is a thin wrapper that plans then
/// commits, for direct/standalone callers (tests, tools) that aren't routing
/// through a `SigilTx`.
pub fn plan_mint(state: &SigilState, user: WalletId, sigil_amount: u128, at_height: u64) -> Result<MintPlan, UsdsError> {
    if sigil_amount == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    let price = usable_price(state, at_height)?;
    let user_sigil = state.balance_of(&user, &NATIVE);
    if user_sigil < sigil_amount {
        return Err(UsdsError::InsufficientSigil);
    }

    // glyphs → USD-e8: divide by GLYPHS_PER_SIGIL (g2 10-dp), NOT PRICE_SCALE.
    let locked_value =
        sigil_amount.checked_mul(price).ok_or(UsdsError::Overflow)? / GLYPHS_PER_SIGIL;
    // The buffer: mint only `value / 1.05` in USDS — see module docs.
    let usds_gross = locked_value
        .checked_mul(sigil_bank::BPS_DENOMINATOR)
        .ok_or(UsdsError::Overflow)?
        / MINT_BUFFER_BPS;
    if usds_gross == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    // The protocol fee: same split every DEX swap already pays.
    let split = split_swap_output(usds_gross, Some(DEV_MASTER_WALLET))?;

    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    let user_usds = state.balance_of(&user, &USDS);
    let master_usds = state.balance_of(&DEV_MASTER_WALLET, &USDS);
    let new_supply = read_supply(state).checked_add(usds_gross).ok_or(UsdsError::Overflow)?;
    Ok(MintPlan {
        usds_to_user: split.user_share,
        mutations: vec![
            // total USDS in existence, committed alongside the balances it counts.
            supply_mutation(new_supply),
            // lock collateral: user → vault (NATIVE conserved), FULL amount —
            // the buffer is what's NOT minted back out as USDS, not a bigger lock.
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil - sigil_amount },
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil + sigil_amount },
            // mint USDS: user's share, plus the protocol's fee share.
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds + split.user_share },
            StateMutation::SetBalance {
                wallet: DEV_MASTER_WALLET,
                token: USDS,
                amount: master_usds + split.master_share,
            },
        ],
    })
}

/// A planned welfare stipend mint — see [`plan_welfare_mint`].
pub struct WelfareMintPlan {
    /// NATIVE glyphs locked from the payer into the vault as collateral.
    pub sigil_locked: u128,
    /// USDS credited to the recipient — exactly the requested target.
    pub usds_to_recipient: u128,
    pub mutations: Vec<StateMutation>,
}

/// Plan minting EXACTLY `usds_target` USDS to `recipient`, collateralized by
/// the `payer`'s NATIVE at the oracle price with the same [`MINT_BUFFER_BPS`]
/// cushion as [`plan_mint`] — the welfare-stipend shape (payer = the welfare
/// treasury, recipient = the claiming citizen). Read-only; the caller
/// (`sigil-tx::apply_tx`'s `WelfareClaim` arm) commits the mutations.
///
/// Differences from [`plan_mint`], all deliberate:
/// - **Inverted direction**: the caller names the USDS OUT and we solve for
///   the SIGIL in, rounding UP both steps so the vault's collateral never
///   dips below the buffer.
/// - **No protocol swap fee**: the dev fee already took its welfare carve
///   when the treasury was funded (200 bps of every block reward) — taxing
///   the stipend again on the way out would be the protocol fee'ing its own
///   welfare payment.
/// - **`native_fee_burn`** folds the claim tx's fee into the payer debit in
///   the SAME `SetBalance` (two absolute writes to one key would leave
///   last-write-wins nondeterminism), and that fee burns — matching the
///   pre-USDS `WelfareClaim` semantics exactly.
pub fn plan_welfare_mint(
    state: &SigilState,
    payer: WalletId,
    recipient: WalletId,
    usds_target: u128,
    native_fee_burn: u128,
    at_height: u64,
) -> Result<WelfareMintPlan, UsdsError> {
    if usds_target == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    if payer == VAULT {
        // The vault can't collateralize itself — and payer/VAULT sharing a
        // key would make the two NATIVE SetBalance writes below race.
        return Err(UsdsError::InsufficientSigil);
    }
    let price = usable_price(state, at_height)?;
    // USD value that must be locked: target × 1.05, rounded up.
    let locked_value = div_ceil(
        usds_target.checked_mul(MINT_BUFFER_BPS).ok_or(UsdsError::Overflow)?,
        sigil_bank::BPS_DENOMINATOR,
    );
    // Glyphs carrying that value at the oracle price, rounded up.
    let sigil_locked = div_ceil(
        locked_value.checked_mul(GLYPHS_PER_SIGIL).ok_or(UsdsError::Overflow)?,
        price,
    );
    let payer_sigil = state.balance_of(&payer, &NATIVE);
    let need = sigil_locked.checked_add(native_fee_burn).ok_or(UsdsError::Overflow)?;
    if payer_sigil < need {
        return Err(UsdsError::WelfarePayerUnderfunded { have: payer_sigil, need });
    }
    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    let recipient_usds = state.balance_of(&recipient, &USDS);
    let new_supply = read_supply(state).checked_add(usds_target).ok_or(UsdsError::Overflow)?;
    Ok(WelfareMintPlan {
        sigil_locked,
        usds_to_recipient: usds_target,
        mutations: vec![
            supply_mutation(new_supply),
            // ONE absolute write per (wallet, token): collateral + fee burn
            // leave the payer together; only the collateral reaches the vault
            // (the fee portion burns, reducing NATIVE supply — same as the
            // fee burn every Send already does).
            StateMutation::SetBalance { wallet: payer, token: NATIVE, amount: payer_sigil - need },
            StateMutation::SetBalance {
                wallet: VAULT,
                token: NATIVE,
                amount: vault_sigil.checked_add(sigil_locked).ok_or(UsdsError::Overflow)?,
            },
            StateMutation::SetBalance {
                wallet: recipient,
                token: USDS,
                amount: recipient_usds.checked_add(usds_target).ok_or(UsdsError::Overflow)?,
            },
        ],
    })
}

/// Ceiling division without the `a + b - 1` overflow trap.
fn div_ceil(a: u128, b: u128) -> u128 {
    a / b + u128::from(a % b != 0)
}

/// Mint USDS by locking `sigil_amount` of NATIVE into the vault, committing
/// immediately. See [`plan_mint`] for the pure version this wraps — use that
/// instead when integrating into a larger batched transition (e.g. a
/// `SigilTx`). Returns the USDS actually credited to `user`.
pub fn mint(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    sigil_amount: u128,
) -> Result<u128, UsdsError> {
    let plan = plan_mint(state, user, sigil_amount, height)?;
    let t = StateTransition { at_height: height, mutations: plan.mutations };
    commit_state_transition(state, &t, height)?;
    Ok(plan.usds_to_user)
}

/// Pure outcome of [`plan_redeem`] — mirrors [`MintPlan`].
#[derive(Debug, Clone)]
pub struct RedeemPlan {
    /// SIGIL that would be credited to the user (after the protocol fee).
    pub sigil_to_user: u128,
    /// The exact mutations [`redeem`] (or a `SigilTx`-integrating caller)
    /// must commit.
    pub mutations: Vec<StateMutation>,
}

/// Plan a redemption: burn `usds_amount` USDS, work out the SIGIL released
/// from the vault at the current price minus the protocol fee — read-only,
/// same "plan then commit" split as [`plan_mint`]/[`mint`].
pub fn plan_redeem(state: &SigilState, user: WalletId, usds_amount: u128, at_height: u64) -> Result<RedeemPlan, UsdsError> {
    if usds_amount == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    let price = usable_price(state, at_height)?;
    let user_usds = state.balance_of(&user, &USDS);
    if user_usds < usds_amount {
        return Err(UsdsError::InsufficientUsds);
    }
    // USD-e8 → glyphs: multiply by GLYPHS_PER_SIGIL (g2 10-dp), NOT PRICE_SCALE.
    let sigil_gross =
        usds_amount.checked_mul(GLYPHS_PER_SIGIL).ok_or(UsdsError::Overflow)? / price;
    let split = split_swap_output(sigil_gross, Some(DEV_MASTER_WALLET))?;

    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    if vault_sigil < sigil_gross {
        return Err(UsdsError::VaultUnderfunded);
    }
    let user_sigil = state.balance_of(&user, &NATIVE);
    let master_sigil = state.balance_of(&DEV_MASTER_WALLET, &NATIVE);
    // Supply can never go below what is being burned: every USDS in a wallet
    // was counted in on the way in. Saturating anyway so a (never expected)
    // pre-tracking balance cannot underflow the counter into u128::MAX.
    let new_supply = read_supply(state).saturating_sub(usds_amount);
    Ok(RedeemPlan {
        sigil_to_user: split.user_share,
        mutations: vec![
            supply_mutation(new_supply),
            // burn USDS
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds - usds_amount },
            // release collateral: vault → user's share + master's fee share
            // (NATIVE conserved throughout — just reassigned).
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil - sigil_gross },
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil + split.user_share },
            StateMutation::SetBalance {
                wallet: DEV_MASTER_WALLET,
                token: NATIVE,
                amount: master_sigil + split.master_share,
            },
        ],
    })
}

/// Redeem `usds_amount` USDS for SIGIL from the vault, committing
/// immediately. See [`plan_redeem`] for the pure version. Returns the SIGIL
/// actually credited to `user`.
pub fn redeem(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    usds_amount: u128,
) -> Result<u128, UsdsError> {
    let plan = plan_redeem(state, user, usds_amount, height)?;
    let t = StateTransition {
        at_height: height,
        mutations: plan.mutations,
    };
    commit_state_transition(state, &t, height)?;
    Ok(plan.sigil_to_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oracle::{update_price, ORACLE_AUTHORITY};

    const USER: WalletId = [0x11; 32];
    /// Every test acts at this height; the oracle push below lands one block
    /// earlier, so the price is fresh (age 1 block).
    const H: u64 = 1_000;

    // genesis: fund USER with 21 SIGIL (21 × 1e10 GLYPHS — g2 decimals), set
    // price $1.00 — chosen so the $21 value × 10000/10500 lands exactly on
    // 20e8 USDS (no rounding noise in the buffer math, so fee math is the
    // only rounding in play).
    fn genesis() -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: 21 * GLYPHS_PER_SIGIL }],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        update_price(&mut s, H - 1, ORACLE_AUTHORITY, PRICE_SCALE).unwrap(); // $1 / SIGIL
        s
    }

    fn native_total(s: &SigilState) -> u128 {
        s.balance_of(&USER, &NATIVE) + s.balance_of(&VAULT, &NATIVE) + s.balance_of(&DEV_MASTER_WALLET, &NATIVE)
    }

    fn usds_total(s: &SigilState) -> u128 {
        s.balance_of(&USER, &USDS) + s.balance_of(&DEV_MASTER_WALLET, &USDS)
    }

    #[test]
    fn mint_applies_the_buffer_before_the_fee() {
        let mut s = genesis();
        // lock 21 SIGIL (21e10 glyphs) @ $1 = $21 value → buffer'd gross =
        // 21e8 × 10000/10500 = 20e8 USDS ($20 exactly)
        let usds = mint(&mut s, H, USER, 21 * GLYPHS_PER_SIGIL).unwrap();
        let expected_split = split_swap_output(20 * 100_000_000, Some(DEV_MASTER_WALLET)).unwrap();
        assert_eq!(usds, expected_split.user_share, "user gets the post-fee share of the buffer'd amount");
        assert!(usds < 21 * 100_000_000, "buffer + fee must issue LESS USDS than the raw locked value");
        assert_eq!(s.balance_of(&USER, &USDS), usds);
        assert_eq!(s.balance_of(&DEV_MASTER_WALLET, &USDS), expected_split.master_share, "protocol fee credited in USDS");
        assert_eq!(s.balance_of(&USER, &NATIVE), 0, "the FULL 21 SIGIL is locked, not just the buffer'd portion");
        assert_eq!(s.balance_of(&VAULT, &NATIVE), 21 * GLYPHS_PER_SIGIL, "vault holds the full lock");
    }

    #[test]
    fn committed_supply_equals_the_sum_of_balances_through_mint_and_redeem() {
        // The whole reason SUPPLY_SLOT exists: the number /v1/usds/status
        // reports must be the number the wallets hold, and it must be in the
        // roots. Mint, then redeem half, then check both ways.
        let mut s = genesis();
        assert_eq!(read_supply(&s), 0);
        let roots_before = s.roots();
        let usds = mint(&mut s, H, USER, 21 * GLYPHS_PER_SIGIL).unwrap();
        assert_eq!(read_supply(&s), 20 * 100_000_000, "supply counts user share + fee share (the gross)");
        assert_eq!(read_supply(&s), usds_total(&s));
        assert_ne!(s.roots().contract_state_root, roots_before.contract_state_root, "supply is committed");
        redeem(&mut s, H + 1, USER, usds / 2).unwrap();
        assert_eq!(read_supply(&s), usds_total(&s), "after a partial redeem the counter still matches the wallets");
    }

    #[test]
    fn vault_holds_more_value_than_outstanding_usds_right_after_mint() {
        // The whole point of the buffer: immediately after a mint, at the
        // SAME price, the vault's collateral value must exceed total USDS
        // supply (the safety margin QUGUSD gets from a 135% ratio; USDS gets
        // it from this fixed buffer instead).
        let mut s = genesis();
        mint(&mut s, H, USER, 21 * GLYPHS_PER_SIGIL).unwrap();
        let price = read_price(&s);
        let vault_value = s.balance_of(&VAULT, &NATIVE) * price / GLYPHS_PER_SIGIL;
        let total_usds = read_supply(&s);
        assert!(vault_value > total_usds, "vault_value={vault_value} must exceed total_usds={total_usds}");
    }

    #[test]
    fn redeem_pays_the_fee_from_the_released_sigil() {
        let mut s = genesis();
        let usds = mint(&mut s, H, USER, 21 * GLYPHS_PER_SIGIL).unwrap();
        let before = native_total(&s);

        let sigil_back = redeem(&mut s, H + 1, USER, usds).unwrap();
        let expected_gross = usds.checked_mul(GLYPHS_PER_SIGIL).unwrap() / read_price(&s);
        let expected_split = split_swap_output(expected_gross, Some(DEV_MASTER_WALLET)).unwrap();
        assert_eq!(sigil_back, expected_split.user_share);
        assert_eq!(s.balance_of(&USER, &USDS), 0, "USDS burned");
        assert!(s.balance_of(&DEV_MASTER_WALLET, &NATIVE) > 0, "protocol earned a SIGIL fee on redemption too");
        // NATIVE is conserved throughout (locked, then reassigned on redeem) —
        // never minted or destroyed.
        assert_eq!(native_total(&s), before);
    }

    #[test]
    fn mint_without_price_fails() {
        let mut s = SigilState::new();
        let t = StateTransition { at_height: 0, mutations: vec![StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: 100_000_000 }] };
        commit_state_transition(&mut s, &t, 0).unwrap();
        assert!(matches!(mint(&mut s, 1, USER, 100_000_000), Err(UsdsError::NoPrice)));
    }

    #[test]
    fn mint_and_redeem_refuse_a_stale_price() {
        // A feeder that died must FREEZE the stablecoin, not misprice it.
        let mut s = genesis();
        let stale_h = (H - 1) + MAX_PRICE_AGE_BLOCKS + 1;
        assert!(matches!(
            plan_mint(&s, USER, GLYPHS_PER_SIGIL, stale_h),
            Err(UsdsError::StalePrice { pushed_at, at_height, max_age })
                if pushed_at == H - 1 && at_height == stale_h && max_age == MAX_PRICE_AGE_BLOCKS
        ));
        // Right at the edge it still works…
        let usds = mint(&mut s, (H - 1) + MAX_PRICE_AGE_BLOCKS, USER, GLYPHS_PER_SIGIL).unwrap();
        // …and one block later the SAME wallet cannot redeem until a fresh push.
        assert!(matches!(plan_redeem(&s, USER, usds, stale_h), Err(UsdsError::StalePrice { .. })));
        update_price(&mut s, stale_h, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        assert!(plan_redeem(&s, USER, usds, stale_h).is_ok(), "a fresh push unfreezes it");
    }

    #[test]
    fn a_price_without_a_height_stamp_is_refused() {
        // The pre-activation shape (price slot only, no stamp) — a price of
        // unknown age is not a price a mint may key off.
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: GLYPHS_PER_SIGIL },
                StateMutation::SetContractSlot {
                    contract: sigil_oracle::ORACLE_CONTRACT,
                    slot: sigil_oracle::PRICE_SLOT,
                    value: sigil_oracle::encode_price(PRICE_SCALE),
                },
            ],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        assert!(matches!(plan_mint(&s, USER, GLYPHS_PER_SIGIL, 5), Err(UsdsError::StalePrice { pushed_at: 0, .. })));
    }

    #[test]
    fn cannot_mint_more_than_collateral() {
        let mut s = genesis();
        assert!(matches!(mint(&mut s, H, USER, 999 * GLYPHS_PER_SIGIL), Err(UsdsError::InsufficientSigil)));
    }

    #[test]
    fn cannot_redeem_more_than_balance() {
        let mut s = genesis();
        mint(&mut s, H, USER, 21 * GLYPHS_PER_SIGIL).unwrap();
        let over = s.balance_of(&USER, &USDS) + 1;
        assert!(matches!(redeem(&mut s, H + 1, USER, over), Err(UsdsError::InsufficientUsds)));
    }

    #[test]
    fn activation_height_is_a_step() {
        assert!(!usds_active(USDS_LIVE_HEIGHT - 1));
        assert!(usds_active(USDS_LIVE_HEIGHT));
    }

    const TREASURY: WalletId = [0x57; 32];
    const CITIZEN: WalletId = [0x22; 32];

    fn welfare_state(treasury_glyphs: u128, price: u128) -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: TREASURY, token: NATIVE, amount: treasury_glyphs }],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        if price > 0 {
            update_price(&mut s, H - 1, ORACLE_AUTHORITY, price).unwrap();
        }
        s
    }

    #[test]
    fn welfare_mint_solves_for_collateral_exactly() {
        // $1.00 target at $2.00/SIGIL with the 105% buffer:
        // lock value = ceil(1e8 × 10500/10000) = 1.05e8 USD-e8
        // glyphs     = ceil(1.05e8 × 1e10 / 2e8) = 5.25e9 (0.525 SIGIL)
        let mut s = welfare_state(3 * GLYPHS_PER_SIGIL, 200_000_000);
        let fee = 5u128;
        let plan = plan_welfare_mint(&s, TREASURY, CITIZEN, 100_000_000, fee, H).unwrap();
        assert_eq!(plan.sigil_locked, 5_250_000_000);
        assert_eq!(plan.usds_to_recipient, 100_000_000);
        commit_state_transition(&mut s, &StateTransition { at_height: H, mutations: plan.mutations }, H).unwrap();
        assert_eq!(s.balance_of(&CITIZEN, &USDS), 100_000_000, "citizen holds exactly $1.00");
        assert_eq!(s.balance_of(&CITIZEN, &NATIVE), 0, "no NATIVE was needed or received");
        assert_eq!(s.balance_of(&VAULT, &NATIVE), 5_250_000_000, "vault holds the collateral");
        assert_eq!(
            s.balance_of(&TREASURY, &NATIVE),
            3 * GLYPHS_PER_SIGIL - 5_250_000_000 - fee,
            "treasury paid collateral + fee; the fee burned"
        );
        assert_eq!(read_supply(&s), 100_000_000, "the stipend is counted in the committed supply");
    }

    #[test]
    fn welfare_mint_rounds_collateral_up() {
        // A price that doesn't divide cleanly must round the lock UP, so the
        // vault never holds less than the buffered value of the USDS issued.
        let s = welfare_state(GLYPHS_PER_SIGIL, 333_333_333);
        let plan = plan_welfare_mint(&s, TREASURY, CITIZEN, 100_000_000, 0, H).unwrap();
        let locked_value_floor = plan.sigil_locked * 333_333_333 / GLYPHS_PER_SIGIL;
        assert!(
            locked_value_floor >= 105_000_000 - 1,
            "collateral value {locked_value_floor} must cover the buffered target"
        );
        assert_eq!(plan.sigil_locked, div_ceil(105_000_000u128 * GLYPHS_PER_SIGIL, 333_333_333));
    }

    #[test]
    fn welfare_mint_fails_closed() {
        // No oracle price → refuse, never mint.
        let s = welfare_state(GLYPHS_PER_SIGIL, 0);
        assert!(matches!(
            plan_welfare_mint(&s, TREASURY, CITIZEN, 100_000_000, 0, H),
            Err(UsdsError::NoPrice)
        ));
        // A stale one → refuse too.
        let s = welfare_state(GLYPHS_PER_SIGIL, 200_000_000);
        assert!(matches!(
            plan_welfare_mint(&s, TREASURY, CITIZEN, 100_000_000, 0, H + MAX_PRICE_AGE_BLOCKS + 5),
            Err(UsdsError::StalePrice { .. })
        ));
        // Underfunded payer → refuse with the exact have/need.
        let s = welfare_state(10, 200_000_000);
        assert!(matches!(
            plan_welfare_mint(&s, TREASURY, CITIZEN, 100_000_000, 3, H),
            Err(UsdsError::WelfarePayerUnderfunded { have: 10, need: 5_250_000_003 })
        ));
        // The vault can never be the payer.
        let s = welfare_state(GLYPHS_PER_SIGIL, 200_000_000);
        assert!(matches!(
            plan_welfare_mint(&s, VAULT, CITIZEN, 100_000_000, 0, H),
            Err(UsdsError::InsufficientSigil)
        ));
    }
}
