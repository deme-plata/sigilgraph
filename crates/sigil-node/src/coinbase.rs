//! ONE-CHAIN step 1 — money enters the braid.
//!
//! The braid producer historically minted EMPTY blocks (a throughput dyno).
//! To make the DAGKnight braid THE money chain, every minted block now carries
//! a **coinbase**: the producer credits itself the block reward through the ONE
//! money chokepoint (`commit_state_transition` — same 21M cap + conservation as
//! the rpcd path). The reward mutation lives in the block body, so every
//! follower re-applies the identical mutation and `ChainTip::apply`'s
//! root-match check passes — deterministic, no special-casing.
//!
//! Reward: block-based `sigil_emission::block_reward(height)` — a pure function
//! of height, so all nodes agree with zero shared clock state. (Time-based
//! wall-clock emission, as rpcd uses, is a follow-on before the live launch;
//! block-based is correct + deterministic for the dev proof and can't drift.)

use sigil_header::BlockHash;
use sigil_state::{SigilState, StateMutation, StateRoots, StateTransition, TokenId, WalletId};
use sigil_tx::SignedTx;

/// Native SIGIL token id (matches sigil_state::NATIVE).
const NATIVE: TokenId = [0u8; 32];

/// Resolve the producer's payout wallet: `SIGIL_PRODUCER_WALLET` (64-hex) or,
/// unset, a deterministic dev wallet so a fresh node still mints coherently.
pub fn producer_wallet() -> WalletId {
    if let Ok(h) = std::env::var("SIGIL_PRODUCER_WALLET") {
        if let Some(w) = hex64(h.trim()) {
            return w;
        }
    }
    // Deterministic dev default (all-0xC1 — visibly "coinbase").
    [0xC1; 32]
}

fn hex64(s: &str) -> Option<WalletId> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Build the coinbase transition for a block at `height`: credit `producer`
/// with `block_reward(height)` on top of its current balance. Empty (no-op) at
/// heights where the reward is zero (post-tail), so late blocks stay valid.
pub fn coinbase_transition(state: &SigilState, height: u64, producer: WalletId) -> StateTransition {
    let reward = sigil_emission::block_reward(height);
    let mut mutations = Vec::new();
    if reward > 0 {
        let bal = state.balance_of(&producer, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: producer,
            token: NATIVE,
            amount: bal.saturating_add(reward),
        });
    }
    StateTransition { at_height: height, mutations }
}

/// Compute the four state roots AFTER applying `transition` to a CLONE of the
/// live state — without mutating it. The producer stamps these into the header;
/// the real `ChainTip::apply` then re-applies to the true state and asserts the
/// roots match (verify-don't-trust holds across the coinbase).
pub fn roots_after(state: &SigilState, transition: &StateTransition, height: u64) -> StateRoots {
    let mut clone = state.clone();
    // A well-formed coinbase never trips the cap on a dev chain; if it ever did,
    // the producer must not mint that block — surface by returning parent roots
    // so apply() fails root-match loudly rather than minting an invalid block.
    match sigil_state::commit_state_transition(&mut clone, transition, height) {
        Ok(roots) => roots,
        Err(_) => state.roots(),
    }
}

/// Convenience: (transition, post-apply roots) for the current tip.
pub fn coinbase_for(
    state: &SigilState,
    height: u64,
    _parent: BlockHash,
) -> (StateTransition, StateRoots) {
    let producer = producer_wallet();
    let tr = coinbase_transition(state, height, producer);
    let roots = roots_after(state, &tr, height);
    (tr, roots)
}

/// Coinbase for an EXPLICIT reward amount (the adaptive-controller path). The
/// producer computes `reward` from the stateful EmissionController and bakes the
/// exact amount into the block body, so followers apply it verbatim and the
/// root-match holds — determinism comes from the amount being IN the block, not
/// a per-node recompute. Returns `(transition, post-apply roots)`.
pub fn coinbase_for_reward(
    state: &SigilState,
    height: u64,
    reward: u128,
) -> (StateTransition, StateRoots) {
    let producer = producer_wallet();
    let mut mutations = Vec::new();
    if reward > 0 {
        let bal = state.balance_of(&producer, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: producer,
            token: NATIVE,
            amount: bal.saturating_add(reward),
        });
    }
    let tr = StateTransition { at_height: height, mutations };
    let roots = roots_after(state, &tr, height);
    (tr, roots)
}

/// Build the FULL block body: coinbase + user sends, applied in order against an
/// evolving clone of state, so a later tx sees earlier txs' effects. Returns the
/// accumulated `(transition, post-apply roots, typed events, included txs)`.
///
/// - `reward` = the coinbase amount (`Some(0)` = no coinbase; `None` = the pure
///   height schedule). Baked into the block body → deterministic on followers.
/// - Each send goes through `apply_tx` (which already emits the balance mutations
///   AND the `PushEventHash` event commitments). An invalid tx (bad sig already
///   filtered at ingest; here: insufficient balance / overflow) is SKIPPED, never
///   included — the block only carries txs that cleanly applied.
/// - `work.roots()` after the sequence == exactly what `ChainTip::apply` computes
///   when it re-applies the accumulated transition, so the header roots match.
pub fn build_block_body(
    state: &SigilState,
    height: u64,
    reward: Option<u128>,
    txs: &[SignedTx],
) -> (StateTransition, StateRoots, Vec<sigil_events::SigilEvent>, Vec<SignedTx>) {
    build_block_body_for(state, height, reward, txs, producer_wallet())
}

/// [`build_block_body`] with an EXPLICIT coinbase beneficiary — the mining path.
/// When a braid block is minted for a verified dual-lane solve, the reward
/// belongs to the miner who did the work, not to the node's configured producer
/// wallet. The beneficiary is a consequence of the block body (a `SetBalance`
/// mutation), so followers re-apply it verbatim and the root-match holds; and it
/// is independently checkable, because the same wallet is committed in the
/// header's `producer` field, which is what the proof-of-work binds to.
///
/// Pool payout note: with `SIGIL_SHARE_EASE_BITS=0` (the default) there are no
/// sub-difficulty shares, so winner-takes-all is exactly correct. Proportional
/// splitting over a share window lands with pool mode.
pub fn build_block_body_for(
    state: &SigilState,
    height: u64,
    reward: Option<u128>,
    txs: &[SignedTx],
    producer: WalletId,
) -> (StateTransition, StateRoots, Vec<sigil_events::SigilEvent>, Vec<SignedTx>) {
    let mut work = state.clone();
    let mut mutations: Vec<StateMutation> = Vec::new();
    let mut events: Vec<sigil_events::SigilEvent> = Vec::new();
    let mut included: Vec<SignedTx> = Vec::new();

    // 1. coinbase first
    let reward = reward.unwrap_or_else(|| sigil_emission::block_reward(height));
    if reward > 0 {
        let bal = work.balance_of(&producer, &NATIVE);
        let cb = StateMutation::SetBalance {
            wallet: producer, token: NATIVE, amount: bal.saturating_add(reward),
        };
        if sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: vec![cb.clone()] }, height,
        ).is_ok() {
            mutations.push(cb);
        }
    }

    // 2. user sends, in order, against the evolving state
    for tx in txs {
        let Ok(res) = sigil_tx::apply_tx(&work, tx) else { continue };
        if sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: res.mutations.clone() }, height,
        ).is_ok() {
            mutations.extend(res.mutations);
            events.extend(res.events);
            included.push(tx.clone());
        }
    }

    (StateTransition { at_height: height, mutations }, work.roots(), events, included)
}

/// [`build_block_body_for`], but the coinbase is [`split_coinbase_mutations`]
/// instead of a single flat `SetBalance` — real pool-share economics (dev-fee +
/// commons split, proportional payout) instead of winner-takes-all-no-fee. This
/// is the mining path: `winner` is who solved the block, `shares` is the full
/// weight map for this height's window (winner's own weight already folded in
/// by the caller). Degenerates to today's single-beneficiary behavior byte-for-
/// byte whenever no master wallet is genesis-committed on this chain (the split
/// then returns 100% to the credited wallet — see `split_mining_reward`), so
/// this is safe to use for BOTH the mined-block case and the default
/// free-running producer case (a one-entry shares map).
pub fn build_block_body_for_shares(
    state: &SigilState,
    height: u64,
    reward: Option<u128>,
    txs: &[SignedTx],
    winner: WalletId,
    shares: &std::collections::HashMap<WalletId, u64>,
) -> (StateTransition, StateRoots, Vec<sigil_events::SigilEvent>, Vec<SignedTx>) {
    let reward = reward.unwrap_or_else(|| sigil_emission::block_reward(height));
    let cb_mutations = split_coinbase_mutations(state, height, reward, winner, shares);

    // Re-apply the coinbase mutations against a fresh evolving clone (mirrors
    // build_block_body_for's pattern exactly) so txs see the post-coinbase
    // balances, then run user sends in order on top.
    let mut work = state.clone();
    let mut mutations: Vec<StateMutation> = Vec::new();
    let mut events: Vec<sigil_events::SigilEvent> = Vec::new();
    let mut included: Vec<SignedTx> = Vec::new();

    if !cb_mutations.is_empty() {
        if sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: cb_mutations.clone() }, height,
        ).is_ok() {
            mutations.extend(cb_mutations);
        }
    }

    for tx in txs {
        let Ok(res) = sigil_tx::apply_tx(&work, tx) else { continue };
        if sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: res.mutations.clone() }, height,
        ).is_ok() {
            mutations.extend(res.mutations);
            events.extend(res.events);
            included.push(tx.clone());
        }
    }

    (StateTransition { at_height: height, mutations }, work.roots(), events, included)
}

/// Full pool-share coinbase: split `reward` proportionally over `shares`
/// (wallet → weight, the winner's own solve already folded in by the
/// caller), applying `sigil-bank`'s master/commons dev-fee split PER credited
/// wallet — the exact economics `sigil-rpc::distribute_block_reward` uses for
/// `sigil-rpcd`, ported here as a deterministic `Vec<StateMutation>` for block-
/// body embedding (every follower replays the SAME mutations from the header,
/// no local RPC-daemon bookkeeping needed) instead of committing straight to a
/// live `SigilState`.
///
/// Degenerates to solo winner-takes-all when `shares` has exactly one entry
/// (the mining bridge always includes the winner's own weight there) — so this
/// is the ONE coinbase path for both cases; a share-less solve and a pool
/// solve never drift apart because they're the same code.
///
/// Conservation is EXACT: every wallet's cut is computed against a RUNNING
/// clone of state (each mutation committed immediately, so the next wallet's
/// `balance_of` sees the prior credit — required because `SetBalance` is an
/// absolute write, not a delta), the winner's cut absorbs the u128 integer-
/// division remainder, and a cut that would push a wallet over the 21M cap is
/// SKIPPED (under-mint is safe, over-mint never happens) — identical semantics
/// to `distribute_block_reward`, just building mutations instead of applying
/// them to a second, separate state.
pub fn split_coinbase_mutations(
    state: &SigilState,
    height: u64,
    reward: u128,
    winner: WalletId,
    shares: &std::collections::HashMap<WalletId, u64>,
) -> Vec<StateMutation> {
    let mut work = state.clone();
    let mut mutations: Vec<StateMutation> = Vec::new();
    if reward == 0 {
        return mutations;
    }

    // One wallet's cut → mutations, applying the master/commons split exactly
    // like `sigil_rpc::credit_share`: master's own cut is skipped when the
    // credited wallet IS the master (self-mining keeps the full share), the
    // commons tithe is taken whenever a master/bank exists regardless of who
    // mines. Returns the amount actually allocated to `to` (0 if the cap
    // rejected it — the caller must not silently redirect that to someone
    // else, which is why this commits eagerly into `work` and returns).
    let mut credit_one = |work: &mut SigilState, to: WalletId, cut: u128| -> u128 {
        if cut == 0 {
            return 0;
        }
        let master = work.master_wallet();
        let Ok(split) = sigil_bank::split_mining_reward(cut, master) else {
            return 0;
        };
        let master_credit = match master {
            Some(m) if m != to => split.master_share,
            _ => 0,
        };
        let commons_credit = if master.is_some() { split.commons_share } else { 0 };
        let Some(to_credit) = cut.checked_sub(master_credit).and_then(|r| r.checked_sub(commons_credit)) else {
            return 0;
        };

        let mut step: Vec<StateMutation> = Vec::with_capacity(3);
        if to_credit > 0 {
            let bal = work.balance_of(&to, &NATIVE);
            let Some(new_bal) = bal.checked_add(to_credit) else { return 0 };
            step.push(StateMutation::SetBalance { wallet: to, token: NATIVE, amount: new_bal });
        }
        if let (Some(m), true) = (master, master_credit > 0) {
            let bal = work.balance_of(&m, &NATIVE);
            let Some(new_bal) = bal.checked_add(master_credit) else { return 0 };
            step.push(StateMutation::SetBalance { wallet: m, token: NATIVE, amount: new_bal });
        }
        if commons_credit > 0 {
            let bal = work.balance_of(&sigil_bank::COMMONS_WALLET, &NATIVE);
            let Some(new_bal) = bal.checked_add(commons_credit) else { return 0 };
            step.push(StateMutation::SetBalance { wallet: sigil_bank::COMMONS_WALLET, token: NATIVE, amount: new_bal });
        }
        if step.is_empty() {
            return 0;
        }
        // Commit eagerly so the NEXT wallet's balance_of() sees this credit —
        // SetBalance is absolute, so two credits to the same wallet computed
        // against a stale base would silently drop the first one.
        if sigil_state::commit_state_transition(work, &StateTransition { at_height: height, mutations: step.clone() }, height).is_err() {
            return 0; // 21M cap edge — skip this wallet's cut, under-mint stays safe
        }
        mutations.extend(step);
        cut
    };

    let total: u128 = shares.values().map(|&c| c as u128).sum();
    if total == 0 {
        // Defensive: caller bug (empty shares) — solo semantics for the winner.
        credit_one(&mut work, winner, reward);
        return mutations;
    }

    let mut allocated: u128 = 0;
    for (&w, &cnt) in shares.iter() {
        if w == winner {
            continue; // winner is credited last so it absorbs the remainder
        }
        let Some(cut) = reward.checked_mul(cnt as u128).map(|v| v / total) else { continue };
        if cut == 0 {
            continue;
        }
        allocated += cut; // counts the cut whether or not the cap accepted it —
                           // an unmintable cut must not silently flow to the winner
        credit_one(&mut work, w, cut);
    }
    let winner_cut = reward.saturating_sub(allocated.min(reward));
    credit_one(&mut work, winner, winner_cut);
    mutations
}

// ── adaptive emission controller: live-reward lifecycle ─────────────────────
use sigil_emission::controller::EmissionController;

fn controller_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("emission-controller.json")
}

/// Load the persisted emission controller (the watermark survives restarts), or
/// mint a fresh one anchored at `genesis_ts_secs`. `None` when adaptive emission
/// is off (`SIGIL_EMISSION_ADAPTIVE` unset/0) — the caller then uses the pure
/// height schedule.
pub fn load_controller(dir: &std::path::Path, genesis_ts_secs: u64) -> Option<EmissionController> {
    if std::env::var("SIGIL_EMISSION_ADAPTIVE").map(|v| v != "0").unwrap_or(false) == false {
        return None;
    }
    let p = controller_path(dir);
    if let Ok(bytes) = std::fs::read(&p) {
        if let Some(c) = EmissionController::restore_from_bytes(&bytes) {
            return Some(c);
        }
    }
    Some(EmissionController::new(genesis_ts_secs))
}

/// Persist the controller watermark (best-effort; atomic via tmp+rename).
pub fn save_controller(dir: &std::path::Path, c: &EmissionController) {
    let p = controller_path(dir);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, c.serialize_state()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_master(st: &mut SigilState, master: WalletId) {
        sigil_state::commit_state_transition(
            st,
            &StateTransition { at_height: 0, mutations: vec![StateMutation::SetMasterWallet { wallet: master }] },
            0,
        ).unwrap();
    }

    fn total_native_delta(before: &SigilState, after: &SigilState, wallets: &[WalletId]) -> u128 {
        wallets.iter().map(|w| after.balance_of(w, &NATIVE) - before.balance_of(w, &NATIVE)).sum()
    }

    #[test]
    fn split_coinbase_degenerates_to_full_reward_with_no_master() {
        // No master wallet set: split_mining_reward returns 100% validator_share,
        // so a one-wallet shares map must credit the FULL reward, byte-identical
        // to the pre-pool-share coinbase path.
        let st = SigilState::new();
        let winner: WalletId = [0x42; 32];
        let shares = std::collections::HashMap::from([(winner, 1u64)]);
        let muts = split_coinbase_mutations(&st, 1, 1000, winner, &shares);
        assert_eq!(muts.len(), 1, "no master => exactly one SetBalance, no fee splits");
        match &muts[0] {
            StateMutation::SetBalance { wallet, amount, .. } => {
                assert_eq!(*wallet, winner);
                assert_eq!(*amount, 1000, "full reward, no dev fee, when no master is set");
            }
            other => panic!("expected SetBalance, got {other:?}"),
        }
    }

    #[test]
    fn split_coinbase_takes_dev_fee_when_master_is_set() {
        let mut st = SigilState::new();
        let master: WalletId = [0x99; 32];
        set_master(&mut st, master);
        let winner: WalletId = [0x42; 32];
        let shares = std::collections::HashMap::from([(winner, 1u64)]);
        let reward = 100_000u128;
        let muts = split_coinbase_mutations(&st, 1, reward, winner, &shares);

        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &StateTransition { at_height: 1, mutations: muts }, 1).unwrap();

        // 5% dev fee (500 bps) + 1.2% commons (120 bps) + 0.1% operator pool (10 bps,
        // folded into validator per credit_share's own accounting — no separate
        // operator wallet here, matching sigil-rpc's model) taken from the winner.
        let winner_bal = after.balance_of(&winner, &NATIVE);
        let master_bal = after.balance_of(&master, &NATIVE);
        let commons_bal = after.balance_of(&sigil_bank::COMMONS_WALLET, &NATIVE);
        assert_eq!(master_bal, reward * 500 / 10_000, "master gets exactly the mining dev fee");
        assert_eq!(commons_bal, reward * 120 / 10_000, "commons gets exactly the mining tithe");
        assert!(winner_bal < reward, "winner does NOT get the full reward once a master exists");
        // EXACT CONSERVATION: no unit created or destroyed by the split.
        assert_eq!(
            total_native_delta(&st, &after, &[winner, master, sigil_bank::COMMONS_WALLET]),
            reward
        );
    }

    #[test]
    fn split_coinbase_self_mining_master_skips_its_own_fee_but_not_commons() {
        let mut st = SigilState::new();
        let master: WalletId = [0x99; 32];
        set_master(&mut st, master);
        // The master wallet mines its own block: it keeps its own cut (no
        // fee-on-yourself), but the commons tithe is still taken — a
        // network-wide carve independent of who mined.
        let shares = std::collections::HashMap::from([(master, 1u64)]);
        let reward = 100_000u128;
        let muts = split_coinbase_mutations(&st, 1, reward, master, &shares);
        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &StateTransition { at_height: 1, mutations: muts }, 1).unwrap();
        let commons_bal = after.balance_of(&sigil_bank::COMMONS_WALLET, &NATIVE);
        assert_eq!(commons_bal, reward * 120 / 10_000, "commons tithe still applies to self-mined blocks");
        assert_eq!(
            total_native_delta(&st, &after, &[master, sigil_bank::COMMONS_WALLET]),
            reward,
            "master + commons must sum to exactly the reward (no double count, no leak)"
        );
    }

    #[test]
    fn split_coinbase_multi_wallet_proportional_exact_conservation() {
        let mut st = SigilState::new();
        let master: WalletId = [0x99; 32];
        set_master(&mut st, master);
        let winner: WalletId = [0x01; 32];
        let other: WalletId = [0x02; 32];
        // winner did 3x the work of `other` — proportional payout, winner absorbs
        // the integer-division remainder.
        let shares = std::collections::HashMap::from([(winner, 3u64), (other, 1u64)]);
        let reward = 100_003u128; // deliberately not evenly divisible by 4
        let muts = split_coinbase_mutations(&st, 1, reward, winner, &shares);
        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &StateTransition { at_height: 1, mutations: muts }, 1).unwrap();

        let winner_bal = after.balance_of(&winner, &NATIVE);
        let other_bal = after.balance_of(&other, &NATIVE);
        let master_bal = after.balance_of(&master, &NATIVE);
        let commons_bal = after.balance_of(&sigil_bank::COMMONS_WALLET, &NATIVE);
        assert!(other_bal > 0, "the minority contributor must be paid something");
        assert!(winner_bal > other_bal * 2, "winner's cut roughly tracks its 3x weight");
        // EXACT CONSERVATION is the property that matters most: every unit of
        // the reward is accounted for across all four wallets, nothing minted
        // or destroyed by rounding.
        assert_eq!(
            total_native_delta(&st, &after, &[winner, other, master, sigil_bank::COMMONS_WALLET]),
            reward
        );
    }

    #[test]
    fn build_block_body_for_shares_matches_legacy_path_when_no_master() {
        // The critical regression-safety property: wiring pool-share mining in
        // must NOT change behavior for the untouched default (no external miner,
        // no master wallet) case. Same producer, same reward, same roots.
        let st = SigilState::new();
        let producer = producer_wallet();
        let (legacy_tr, legacy_roots, ..) = build_block_body_for(&st, 1, None, &[], producer);
        let shares = std::collections::HashMap::from([(producer, 1u64)]);
        let (shares_tr, shares_roots, ..) = build_block_body_for_shares(&st, 1, None, &[], producer, &shares);
        assert_eq!(legacy_tr.mutations, shares_tr.mutations, "identical mutations");
        assert_eq!(legacy_roots.wallet_state_root, shares_roots.wallet_state_root, "identical roots");
    }

    #[test]
    fn coinbase_credits_reward_and_moves_root() {
        let mut st = SigilState::new();
        let root0 = st.roots().wallet_state_root;
        let prod = [0xC1; 32];
        let (tr, roots) = coinbase_for(&st, 1, [0u8; 32]);
        assert_eq!(tr.mutations.len(), 1, "height 1 pays a reward");
        // roots_after must equal a real apply
        let applied = sigil_state::commit_state_transition(&mut st, &tr, 1).unwrap();
        assert_eq!(roots.wallet_state_root, applied.wallet_state_root, "predicted roots == applied roots");
        assert_ne!(root0, applied.wallet_state_root, "coinbase moves the wallet root — money entered the graph");
        assert_eq!(st.balance_of(&prod, &NATIVE), sigil_emission::block_reward(1));
        assert_eq!(st.native_supply(), sigil_emission::block_reward(1), "supply grew by exactly the reward");
    }

    #[test]
    fn deterministic_across_nodes() {
        // two independent producers at the same height compute identical roots
        let a = SigilState::new();
        let b = SigilState::new();
        let (_, ra) = coinbase_for(&a, 5, [0u8; 32]);
        let (_, rb) = coinbase_for(&b, 5, [0u8; 32]);
        assert_eq!(ra.wallet_state_root, rb.wallet_state_root);
    }

    /// CHRONOS GATE (the flux-way money-on-braid proof): N independent nodes
    /// replay the SAME ordered sequence of coinbase blocks and must converge to
    /// a byte-identical wallet_state_root + native_supply — divergence == 0.
    /// This is what makes it SAFE to settle money over the braid's finalized
    /// order: the order determines the money, uniquely, on every node.
    /// Step 2 gate: a real SEND applied in a braid block moves balances, and two
    /// nodes building the same (coinbase + send) body reach identical roots.
    #[test]
    fn send_applies_in_block_and_is_deterministic() {
        use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};
        // fund a sender through the chokepoint
        let (sk, pk, sender) = ed25519_keygen();
        let recipient = [0x77u8; 32];
        let mut base = SigilState::new();
        sigil_state::commit_state_transition(
            &mut base,
            &StateTransition { at_height: 0, mutations: vec![
                StateMutation::SetBalance { wallet: sender, token: NATIVE, amount: 1_000_000_000 },
            ] }, 0,
        ).unwrap();

        let tx = ed25519_sign_tx(
            SigilTx::Send { from: sender, to: recipient, amount: 250_000_000, token: NATIVE, fee: 1_000 },
            &sk, &pk,
        );

        // build the block body (coinbase + the send) on two independent nodes
        let (tr1, r1, ev1, inc1) = build_block_body(&base, 1, Some(0), &[tx.clone()]);
        let (tr2, r2, _ev2, inc2) = build_block_body(&base, 1, Some(0), &[tx]);

        assert_eq!(inc1.len(), 1, "the valid send is included");
        assert_eq!(inc2.len(), 1);
        assert_eq!(r1.wallet_state_root, r2.wallet_state_root, "two nodes → identical roots (deterministic)");
        assert!(!ev1.is_empty(), "send emits typed events (Send+Receive)");

        // apply on a node and check the money actually moved
        let mut node = base.clone();
        let computed = sigil_state::commit_state_transition(&mut node, &tr1, 1).unwrap();
        assert_eq!(computed.wallet_state_root, r1.wallet_state_root, "predicted == applied roots");
        assert_eq!(node.balance_of(&recipient, &NATIVE), 250_000_000, "recipient received the send");
        assert_eq!(node.balance_of(&sender, &NATIVE), 1_000_000_000 - 250_000_000 - 1_000, "sender debited amount+fee");
        let _ = tr2;
    }

    /// STEP 3 GATE — money settlement over a WEAVE (the flux-way chronos proof).
    /// Two producers mint coinbase blocks; some heights fork (both produce), so
    /// the braid must pick ONE selected-spine block per height. The property:
    /// two nodes that receive the woven blocks in DIFFERENT gossip orders must
    /// (a) linearize to the identical order (order_hash), and (b) settle to the
    /// identical supply + wallet_root — with each height's reward counted EXACTLY
    /// once (merged/off-spine blocks never double-emit).
    #[test]
    fn chronos_weave_divergence_zero_no_double_emission() {
        use crate::block::Block;
        use crate::chain::ChainTip;
        use sigil_dagknight::{Braid, BraidConfig, BlockView, InsertOutcome};
        use sigil_header::*;

        const REWARD: u128 = 5_00000000;
        let pa = [0xAAu8; 32];
        let pb = [0xBBu8; 32];

        // Build a real coinbase Block for an explicit producer (no env coupling).
        fn mk(state: &SigilState, height: u64, parent: BlockHash, producer: WalletId,
              merge_parents: Vec<BlockHash>) -> Block {
            let bal = state.balance_of(&producer, &NATIVE);
            let tr = StateTransition {
                at_height: height,
                mutations: vec![StateMutation::SetBalance {
                    wallet: producer, token: NATIVE, amount: bal.saturating_add(REWARD),
                }],
            };
            let roots = roots_after(state, &tr, height);
            let nonce = SqiSignature::from_array([0u8; SQISIGN_L5_LEN]);
            let mut hsh = blake3::Hasher::new();
            hsh.update(&parent);
            hsh.update(nonce.as_bytes());
            let vdf_input = *hsh.finalize().as_bytes();
            let header = SigilBlockHeaderV0 {
                version: HEADER_VERSION, network_id: NETWORK_ID, height, parent_hash: parent,
                merge_parents, timestamp_ms: 0, nonce_sqisign: nonce, vdf_input,
                vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 }, difficulty: 0,
                wallet_state_root: roots.wallet_state_root, dex_state_root: roots.dex_state_root,
                event_log_root: roots.event_log_root, contract_state_root: roots.contract_state_root,
                state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
                txs_merkle_root: [0u8; 32], tx_count: 0,
                fluxc_artifact_proof: ProofBundle { artifact_blake3: [0u8; 32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None },
                sig_scheme: SigScheme::SqiSign5, producer: [0u8; 32],
                producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            };
            Block { header, transition: tr, events: vec![] }
        }

        // Genesis parent (ChainTip::new tip). Build a woven set of candidate
        // blocks: two forks at h0, a merge at h1, a fork at h1, a merge at h2.
        let genesis = ChainTip::new().parent_hash();
        let empty = SigilState::new();
        let a0 = mk(&empty, 0, genesis, pa, vec![]);          // PA @0
        let b0 = mk(&empty, 0, genesis, pb, vec![]);          // PB @0 (fork)
        // spine @0 = the min-hash of {a0,b0}; both nodes agree. h1 extends it + merges the other.
        let (spine0, other0) = if a0.hash() <= b0.hash() { (&a0, &b0) } else { (&b0, &a0) };
        let s0 = spine0.hash();
        // apply spine0 to a scratch chain to get the state h1 is minted against
        let mut scratch = ChainTip::new();
        scratch.apply(spine0.clone()).unwrap();
        let a1 = mk(&scratch.state_snapshot(), 1, s0, pa, vec![other0.hash()]); // merges the off-spine fork
        let b1 = mk(&scratch.state_snapshot(), 1, s0, pb, vec![]);              // another fork @1
        let (spine1, _other1) = if a1.hash() <= b1.hash() { (&a1, &b1) } else { (&b1, &a1) };
        scratch.apply(spine1.clone()).unwrap();
        let a2 = mk(&scratch.state_snapshot(), 2, spine1.hash(), pa, vec![]);

        let all = [&a0, &b0, &a1, &b1, &a2];
        let by_hash: std::collections::HashMap<BlockHash, &Block> =
            all.iter().map(|b| (b.hash(), *b)).collect();
        let view = |b: &Block| BlockView::from(&b.header);
        let cfg = || BraidConfig {
            final_depth: 2,
            max_window: 64,
            max_pending: 64,
            max_merge_parents: 4,
            ghostdag_k: None,
        };

        // Two nodes, two DIFFERENT gossip arrival orders.
        let order_x = [&a0, &b0, &a1, &b1, &a2];
        let order_y = [&b0, &a0, &b1, &a1, &a2];
        let mut settled = Vec::new();
        let mut order_hashes = Vec::new();
        for order in [order_x, order_y] {
            let mut braid = Braid::new(cfg());
            for b in order {
                // some inserts may be MissingParents until the parent arrives; re-insert at end
                let _ = braid.insert(view(b));
            }
            // ensure all landed (arrival-order independence): re-offer any missing
            for b in all.iter() {
                if !matches!(braid.insert(view(b)), InsertOutcome::Duplicate | InsertOutcome::Inserted { .. } | InsertOutcome::BelowFinal { .. }) {
                    let _ = braid.insert(view(b));
                }
            }
            let lin = braid.linearize();
            order_hashes.push(braid.order_hash());
            // Settle money: apply blocks in linearized order that EXTEND the tip
            // (dag_drain_apply's v0 rule); off-spine/merged blocks are ordered but
            // NOT state-applied → no double-emission.
            let mut chain = ChainTip::new();
            for oh in &lin {
                if let Some(b) = by_hash.get(oh) {
                    if b.header.parent_hash == chain.parent_hash() && b.header.height == chain.height() {
                        let _ = chain.apply((*b).clone());
                    }
                }
            }
            let st = chain.state_snapshot();
            settled.push((st.roots().wallet_state_root, st.native_supply()));
        }

        // (a) both nodes converge on the identical order despite different arrival
        assert_eq!(order_hashes[0], order_hashes[1], "weave order_hash must converge across gossip orders");
        // (b) both nodes settle to the identical wallet_root + supply
        assert_eq!(settled[0], settled[1], "money must be identical on both nodes over the weave — divergence 0");
        // (c) no double-emission: supply is the spine length × reward (h0,h1,h2 = 3),
        // strictly less than all 5 candidate blocks × reward.
        let supply = settled[0].1;
        assert_eq!(supply, 3 * REWARD, "exactly the 3 spine heights minted — merged forks never double-emit");
        assert!(supply < 5 * REWARD, "off-spine candidates did not pay coinbase");
    }

    #[test]
    fn chronos_coinbase_replay_divergence_zero() {
        const NODES: usize = 4;
        const BLOCKS: u64 = 200;

        // Producer builds the canonical order: coinbase transition per height.
        let mut producer = SigilState::new();
        let mut order: Vec<StateTransition> = Vec::new();
        for h in 1..=BLOCKS {
            let tr = coinbase_transition(&producer, h, producer_wallet());
            // apply on the producer to advance its own state (as a real node does)
            sigil_state::commit_state_transition(&mut producer, &tr, h).unwrap();
            order.push(tr);
        }

        // N followers replay the identical order from genesis.
        let mut roots = Vec::new();
        let mut supplies = Vec::new();
        for _ in 0..NODES {
            let mut node = SigilState::new();
            for (i, tr) in order.iter().enumerate() {
                let h = (i as u64) + 1;
                sigil_state::commit_state_transition(&mut node, tr, h).unwrap();
            }
            roots.push(node.roots().wallet_state_root);
            supplies.push(node.native_supply());
        }

        // Divergence must be zero: every node agrees with the producer + each other.
        let p_root = producer.roots().wallet_state_root;
        let p_supply = producer.native_supply();
        for i in 0..NODES {
            assert_eq!(roots[i], p_root, "node {i} wallet_root diverged from producer");
            assert_eq!(supplies[i], p_supply, "node {i} supply diverged from producer");
        }
        // and the emission is exactly the schedule sum (conservation)
        let expected: u128 = (1..=BLOCKS).map(sigil_emission::block_reward).sum();
        assert_eq!(p_supply, expected, "minted supply == Σ block_reward(h) — no phantom emission");
    }
}
