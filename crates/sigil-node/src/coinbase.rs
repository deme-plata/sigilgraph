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
///
/// If `SIGIL_PRODUCER_SIGNING_SEED_HEX` (see `producer_signing`) is configured,
/// its derived wallet takes precedence — so a node that opts into real Ed25519
/// signing for its self-mined blocks pays itself at the SAME address it signs
/// with (required: `producer_signing::maybe_sign` only signs when `header.
/// producer` matches the configured key). A node that hasn't set that env var
/// (every live node as of 2026-08-20, Epsilon included) is byte-for-byte
/// unaffected — this call is then a no-op passthrough to the legacy behavior.
/// A configured signing key that disagrees with an explicitly-set
/// `SIGIL_PRODUCER_WALLET` is a startup misconfiguration — fail loud rather
/// than silently pick one (an operator could otherwise sign for a wallet that
/// never receives its own mining reward, or vice versa).
pub fn producer_wallet() -> WalletId {
    let explicit = std::env::var("SIGIL_PRODUCER_WALLET").ok();
    match crate::producer_signing::reconcile_producer_wallet(explicit.as_deref()) {
        Ok(Some(w)) => return w,
        Ok(None) => {} // no signing key configured — fall through to legacy behavior
        Err(e) => panic!("producer_wallet misconfiguration: {e}"),
    }
    if let Some(h) = explicit {
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
        // SHIELDED COINBASE (2026-08-23). If this producer has published a shielded key,
        // the reward is minted straight into the pool instead of landing transparently.
        //
        // This is the only mechanism that grows the anonymity set without asking anyone to
        // change what they do: every miner who registers once becomes a pool participant
        // at block rate, with independently-funded notes — which is the difference between
        // a crowd and one entity's notes wearing different hats.
        //
        // Falls back to the transparent credit when the producer has NOT registered, so an
        // existing miner is never broken by this.
        // Same pool-ceiling guard as `shielded_credit`: a full pool must degrade to a
        // transparent credit, never to an unpayable block. `None` here takes the existing
        // unregistered path, which pays transparently.
        let producer_shield = if work.shielded().len() >= sigil_state::shielded::POOL_CAPACITY {
            None
        } else {
            work.shielded().shielded_address(&producer)
        };
        let cb = match producer_shield {
            Some(pk) => match sigil_shield::note_v1::coinbase_commitment_wire(height, &pk, reward) {
                Some(cm) => StateMutation::ShieldedCoinbase {
                    pk_shield: pk,
                    amount: reward,
                    cm,
                    ct: seal_coinbase_note(&work, &producer, &pk, reward, height),
                },
                // An unrepresentable reward (past the circuit's range bound) must not
                // silently vanish — pay it transparently rather than mint nothing.
                None => StateMutation::SetBalance {
                    wallet: producer,
                    token: NATIVE,
                    amount: work.balance_of(&producer, &NATIVE).saturating_add(reward),
                },
            },
            None => StateMutation::SetBalance {
                wallet: producer,
                token: NATIVE,
                amount: work.balance_of(&producer, &NATIVE).saturating_add(reward),
            },
        };
        if sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: vec![cb.clone()] }, height,
        ).is_ok() {
            mutations.push(cb);
        }
    }

    // 2. user sends, in order, against the evolving state
    for tx in txs {
        let Ok(res) = sigil_tx::apply_tx_at(&work, tx, height) else { continue };
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
        // FAIL LOUD, not silent. This used to be a bare `.is_ok()` with no `else`: when the
        // coinbase failed to apply, the ENTIRE coinbase — miner cut, master cut and commons
        // cut — was discarded and the block minted without it, with no error, no log line
        // and no counter. A registered miner hashed for nothing and there was no trace to
        // find (live, 2026-08-26: 31.66 MH/s credited +0 for hours, root cause a full
        // shielded pool). The tx loop below already learned this lesson — its own comment
        // reads "Fail LOUD, not silent: a dropped tx used to vanish with zero trace" — the
        // coinbase path never got the same treatment.
        match sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: cb_mutations.clone() }, height,
        ) {
            Ok(_) => mutations.extend(cb_mutations),
            Err(e) => eprintln!(
                "🚨 COINBASE DROPPED at h={height} ({e:?}) — miner, master AND commons cuts \
                 are unpaid for this block. {} mutation(s) discarded.",
                cb_mutations.len()
            ),
        }
    }

    for tx in txs {
        let res = match sigil_tx::apply_tx_at(&work, tx, height) {
            Ok(res) => res,
            Err(e) => {
                // Fail LOUD, not silent: a dropped tx used to vanish with zero trace,
                // which is exactly how the send-endpoint's "queued but never lands"
                // symptom went undiagnosed. tx.hash() is cheap and lets an operator
                // correlate this against the txid their wallet displayed.
                eprintln!("✗ tx dropped at h={height} (apply_tx: {e:?}) hash={}", hex::encode(tx.tx.hash()));
                continue;
            }
        };
        match sigil_state::commit_state_transition(
            &mut work, &StateTransition { at_height: height, mutations: res.mutations.clone() }, height,
        ) {
            Ok(_) => {
                mutations.extend(res.mutations);
                events.extend(res.events);
                included.push(tx.clone());
            }
            Err(e) => {
                eprintln!("✗ tx dropped at h={height} (commit_state_transition: {e:?}) hash={}", hex::encode(tx.tx.hash()));
            }
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

    // Every wallet credited by this coinbase — miner, master AND commons — mints a
    // shielded note instead of a transparent balance IF that wallet has published a
    // shield key. See `shielded_credit` for why this had to become one shared helper.
    //
    // The set of note commitments this block has already minted. `append_note` REJECTS
    // a duplicate commitment (`ShieldedError::DuplicateCommitment`), and that error
    // fails the whole `commit_state_transition` — which would silently drop the ENTIRE
    // coinbase for the block, not just the colliding cut. Two credits collide only when
    // they share `(height, pk_shield, amount)`, i.e. two distinct wallets registered the
    // SAME shield key and drew equal cuts at the same height. Rare, but the blast radius
    // is a lost block reward, so the second one falls back to a transparent credit
    // rather than risking it.
    let mut minted_cms: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();

    // One wallet's cut → the right mutation for it. Shielded when `to` has registered a
    // key AND the amount is inside the circuit's representable range AND the commitment
    // does not collide with one already minted in this block; transparent otherwise
    // (byte-for-byte the behavior every unregistered wallet always had).
    //
    // 2026-08-26: the shielded branch used to exist ONLY for the miner's own cut, so a
    // registered master or commons wallet was still paid in the clear. That asymmetry
    // had no justification — the registry is per-wallet and says nothing about what role
    // the wallet plays in a block — and it mattered a lot: master (5%) + commons (1%) is
    // taken from EVERY block regardless of who mined it or whether that miner ever opted
    // in, so it is the only coinbase cut whose shielding does not depend on persuading
    // individual miners. Live at the time of the fix: 86 MH/s across 5 rigs, 8,564 blocks
    // accepted, and a shielded pool holding exactly ONE note — because not one miner was
    // registered and the two protocol wallets could not contribute even if they were.
    fn shielded_credit(
        work: &SigilState,
        minted: &mut std::collections::HashSet<[u8; 32]>,
        to: WalletId,
        amount: u128,
        height: u64,
    ) -> Option<StateMutation> {
        let pk = work.shielded().shielded_address(&to)?;
        // POOL CEILING (live incident, 2026-08-26). The pool is a fixed-capacity tree,
        // and `append_note` returns PoolFull at the top. A registered wallet ALWAYS took
        // the shielded path — the transparent fallback keys off non-registration, not off
        // whether the note can actually be minted — so once the pool filled, that wallet's
        // cut could no longer be applied. And because a failed coinbase was dropped whole
        // and silently (see `build_block_body_for_shares`), the miner was simply paid
        // NOTHING: measured live at 31.66 MH/s earning +0 while unregistered miners were
        // paid normally.
        //
        // `minted` counts the notes THIS coinbase has already queued but not yet applied,
        // so several cuts in one block cannot collectively overshoot the ceiling.
        // Returning None here routes the cut through the caller's existing transparent
        // credit — "a credit that must never vanish" now also covers a full pool.
        // 2026-08-27: this early-return is GONE, and removing it is the whole point of
        // epoch rotation.
        //
        // It was added on 2026-08-26 as damage control: back then `append_note` returned
        // `PoolFull` at capacity, a failed coinbase was dropped whole and silently, and a
        // registered miner at 31.66 MH/s earned +0 for hours. Falling back to a
        // transparent credit was strictly better than losing the reward.
        //
        // Rotation changed what a full pool MEANS. Appending to a full pool now seals the
        // generation and opens a fresh one, so the append cannot fail — and because
        // rotation is triggered BY that append, this check prevented the very thing that
        // would have fixed it. Measured live: the pool sat at exactly 32,768/32,768 with
        // `epoch 0, sealed 0` for over two hours while every registered miner's reward
        // was quietly paid in the clear. No money was lost; privacy was, permanently and
        // without a single error line, because the mitigation had become the blocker.
        //
        // A mid-block rotation is deterministic — every node applies the same mutations in
        // the same order — so the generation boundary lands at the same transaction of the
        // same block everywhere.
        //
        // `minted` still guards a within-block commitment collision below.
        let cm = sigil_shield::note_v1::coinbase_commitment_wire(height, &pk, amount)?;
        if !minted.insert(cm) {
            return None;
        }
        let ct = seal_coinbase_note(work, &to, &pk, amount, height);
        Some(StateMutation::ShieldedCoinbase { pk_shield: pk, amount, cm, ct })
    }

    // A credit that must never vanish: shielded when the wallet is registered, else the
    // absolute-write transparent credit. `None` only on a u128 overflow of the
    // transparent balance, which the caller treats as "skip this cut" exactly as before.
    fn credit_mutation(
        work: &SigilState,
        minted: &mut std::collections::HashSet<[u8; 32]>,
        to: WalletId,
        amount: u128,
        height: u64,
    ) -> Option<StateMutation> {
        // DUST GATE (2026-08-28). From `TRANSPARENT_COINBASE_HEIGHT` every cut is paid
        // transparently, registered or not. A shielded coinbase minted one note per payee
        // per block, and with a 1-in/2-out spend circuit one note is the most a single
        // send can draw on — so a miner's balance was real and almost none of it was
        // spendable. Measured live: 822,556 notes, 34 wallets, ONE nullifier ever.
        //
        // Nothing is lost by dropping it: `sigil-chronos` measured coinbase notes as 620
        // of 620 publicly attributable to their miner at mint time, because the block a
        // note is minted in names the miner who mined it. They grew the note count without
        // growing the anonymity set, which counts distinct unlinkable owners. Privacy now
        // comes from a deliberate `Shield` — user-chosen timing, standard denomination,
        // and one note large enough to actually spend. The transparent balance is the
        // accrual bucket, which is why this needs no new consensus state: `SetBalance`
        // adds. See `sigil_tx::TRANSPARENT_COINBASE_HEIGHT` for the full argument.
        if !sigil_tx::coinbase_is_transparent(height) {
            if let Some(m) = shielded_credit(work, minted, to, amount, height) {
                return Some(m);
            }
        }
        let bal = work.balance_of(&to, &NATIVE);
        let new_bal = bal.checked_add(amount)?;
        Some(StateMutation::SetBalance { wallet: to, token: NATIVE, amount: new_bal })
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
            let Some(m) = credit_mutation(work, &mut minted_cms, to, to_credit, height) else { return 0 };
            step.push(m);
        }
        if let (Some(m), true) = (master, master_credit > 0) {
            let Some(mu) = credit_mutation(work, &mut minted_cms, m, master_credit, height) else { return 0 };
            step.push(mu);
        }
        if commons_credit > 0 {
            let Some(mu) = credit_mutation(work, &mut minted_cms, sigil_bank::COMMONS_WALLET, commons_credit, height) else { return 0 };
            step.push(mu);
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
    /// Registers `miner` for shielded pay and returns the wire pk.
    fn register_for_shielded_pay(st: &mut SigilState, miner: WalletId, seed: u8) -> [u8; 32] {
        let acct = sigil_shield::wallet::ShieldedAccount::from_seed([seed; 32]);
        let pk_shield = sigil_shield::note_v1::to_wire(acct.public_key());
        sigil_state::commit_state_transition(
            st,
            &StateTransition {
                at_height: 0,
                mutations: vec![StateMutation::RegisterShieldedAddress {
                    wallet: miner,
                    pk_shield,
                    pk_sqi: None, pk_encrypt: None,
                }],
            },
            0,
        )
        .unwrap();
        pk_shield
    }

    /// THE BOUNDARY, from below. One block before activation the old rule still holds, so
    /// every block already on the chain validates exactly as it did when it was produced.
    /// This is the half of a height gate that is easy to forget to test and expensive to
    /// get wrong: break it and 311k historical blocks stop verifying.
    #[test]
    fn the_block_before_activation_still_pays_a_registered_miner_privately() {
        let mut st = SigilState::new();
        let miner: WalletId = [0x99u8; 32];
        let pk = register_for_shielded_pay(&mut st, miner, 0x42);

        let h = sigil_tx::TRANSPARENT_COINBASE_HEIGHT - 1;
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let (tr, ..) = build_block_body_for_shares(&st, h, Some(1_000_000), &[], miner, &shares);

        assert!(
            tr.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedCoinbase { pk_shield: p, .. } if *p == pk)),
            "the last pre-activation block must still mint a shielded note: {:?}",
            tr.mutations
        );
    }

    /// THE BOUNDARY, from above. At the activation height the same registered miner is
    /// paid in the clear — an ADDITIVE `SetBalance`, no note, pool untouched.
    #[test]
    fn at_activation_height_a_registered_miner_is_paid_transparently() {
        let mut st = SigilState::new();
        let miner: WalletId = [0x99u8; 32];
        let _pk = register_for_shielded_pay(&mut st, miner, 0x42);

        let h = sigil_tx::TRANSPARENT_COINBASE_HEIGHT;
        let pool_before = st.shielded().len();
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let (tr, roots, ..) = build_block_body_for_shares(&st, h, Some(1_000_000), &[], miner, &shares);

        assert!(
            !tr.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedCoinbase { .. })),
            "no note may be minted at or after activation: {:?}",
            tr.mutations
        );
        assert!(
            tr.mutations.iter().any(|m| matches!(m, StateMutation::SetBalance { wallet, amount: 1_000_000, .. } if *wallet == miner)),
            "the registered miner must be paid transparently: {:?}",
            tr.mutations
        );

        let applied = sigil_state::commit_state_transition(&mut st, &tr, h).unwrap();
        assert_eq!(applied.wallet_state_root, roots.wallet_state_root, "predicted == applied roots");
        assert_eq!(st.balance_of(&miner, &NATIVE), 1_000_000, "the whole reward is in one spendable number");
        assert_eq!(st.shielded().len(), pool_before, "the pool did not grow");
        assert_eq!(st.shielded().value_locked(), 0, "no value is locked in the pool");
        assert_eq!(st.native_supply(), 1_000_000, "issuance is fully transparent now");
    }

    /// THE POINT OF THE WHOLE CHANGE, stated as a property.
    ///
    /// Mine the same 100 blocks under both rules and ask the only question a user cares
    /// about: how much can I send in one transaction?
    ///
    /// The spend circuit is 1-in/2-out — one note in, one payment and one change note out
    /// — so under the shielded coinbase the answer is "one block's reward", no matter how
    /// many blocks you mined. 100 blocks of work, and the largest single send is 1/100th
    /// of it. That is the live complaint ("0.006 out of 11 SIGIL") reproduced in a test.
    ///
    /// Transparent balances ADD, so after activation the answer is "all of it".
    #[test]
    fn after_activation_a_miner_can_spend_everything_they_mined() {
        const BLOCKS: u64 = 100;
        const REWARD: u128 = 1_000_000;
        let miner: WalletId = [0x99u8; 32];
        let shares = std::collections::HashMap::from([(miner, 1u64)]);

        // --- OLD RULE: 100 blocks, 100 notes, biggest single send = ONE reward. ---
        let mut dust = SigilState::new();
        register_for_shielded_pay(&mut dust, miner, 0x42);
        for i in 0..BLOCKS {
            let h = 1 + i; // safely below activation
            let (tr, ..) = build_block_body_for_shares(&dust, h, Some(REWARD), &[], miner, &shares);
            sigil_state::commit_state_transition(&mut dust, &tr, h).unwrap();
        }
        assert_eq!(dust.shielded().len() as u64, BLOCKS, "one note per block — the dust");
        assert_eq!(dust.shielded().value_locked(), REWARD * BLOCKS as u128, "the value is all there…");
        assert_eq!(dust.balance_of(&miner, &NATIVE), 0, "…and none of it is transparent");
        // The spendable-in-one-transaction ceiling is the LARGEST SINGLE NOTE, because the
        // circuit takes exactly one input. Every coinbase note is worth one reward.
        let old_max_single_send = REWARD;

        // --- NEW RULE: 100 blocks, 0 notes, biggest single send = EVERYTHING. ---
        let mut clean = SigilState::new();
        register_for_shielded_pay(&mut clean, miner, 0x42);
        for i in 0..BLOCKS {
            let h = sigil_tx::TRANSPARENT_COINBASE_HEIGHT + i;
            let (tr, ..) = build_block_body_for_shares(&clean, h, Some(REWARD), &[], miner, &shares);
            sigil_state::commit_state_transition(&mut clean, &tr, h).unwrap();
        }
        assert_eq!(clean.shielded().len(), 0, "not one dust note was created");
        let new_max_single_send = clean.balance_of(&miner, &NATIVE);
        assert_eq!(new_max_single_send, REWARD * BLOCKS as u128, "the whole haul, in one number");

        assert_eq!(
            new_max_single_send / old_max_single_send,
            BLOCKS as u128,
            "a miner can now send {BLOCKS}x more in a single transaction than before"
        );

        // Same money either way — this changes WHERE the value sits, never HOW MUCH.
        assert_eq!(
            dust.native_supply() + dust.shielded().value_locked(),
            clean.native_supply() + clean.shielded().value_locked(),
            "total issuance across both domains is identical under both rules"
        );
    }


    /// THE GAP THIS SESSION FOUND: `split_coinbase_mutations` — the path
    /// essentially every real block win goes through once external miners
    /// are active — used to credit every recipient with a plain, transparent
    /// `SetBalance`, even a miner who had called `/v1/shielded/register`.
    /// The legacy `build_block_body_for`'s ShieldedCoinbase branch only fires
    /// on the no-solve fallback, which real mining traffic rarely hits. This
    /// proves the pool-split path now honors registration too.
    #[test]
    fn pool_share_credit_shields_a_registered_miner() {
        let mut st = SigilState::new();
        let seed = [0x42u8; 32];
        let acct = sigil_shield::wallet::ShieldedAccount::from_seed(seed);
        let pk_shield = sigil_shield::note_v1::to_wire(acct.public_key());
        let miner: WalletId = [0x99u8; 32];

        sigil_state::commit_state_transition(
            &mut st,
            &StateTransition {
                at_height: 0,
                mutations: vec![StateMutation::RegisterShieldedAddress {
                    wallet: miner,
                    pk_shield,
                    pk_sqi: None, pk_encrypt: None,
                }],
            },
            0,
        )
        .unwrap();

        let pool_before = st.shielded().len();
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let (tr, roots, ..) = build_block_body_for_shares(&st, 1, Some(1_000_000), &[], miner, &shares);

        assert!(
            tr.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedCoinbase { pk_shield: pk, amount: 1_000_000, .. } if *pk == pk_shield)),
            "a registered miner's pool-share credit must mint a shielded note, not a transparent balance: {:?}",
            tr.mutations
        );
        assert!(
            !tr.mutations.iter().any(|m| matches!(m, StateMutation::SetBalance { wallet, .. } if *wallet == miner)),
            "the registered miner must receive NO transparent credit for this reward"
        );

        let applied = sigil_state::commit_state_transition(&mut st, &tr, 1).unwrap();
        assert_eq!(applied.wallet_state_root, roots.wallet_state_root, "predicted == applied roots");
        assert_eq!(st.balance_of(&miner, &NATIVE), 0, "registered miner's transparent balance stays untouched");
        assert_eq!(st.shielded().len(), pool_before + 1, "exactly one new note entered the pool");
        // `native_supply()` is transparent-wallets-only by design (PV-1 — see the
        // HARD SUPPLY CAP comment in commit_state_transition): a shielded credit
        // moves value OUT of it, into `shielded().value_locked()`. The real
        // conservation property spans both.
        assert_eq!(st.native_supply(), 0, "transparent supply is untouched by a shielded credit");
        assert_eq!(st.shielded().value_locked(), 1_000_000, "the reward is fully accounted for in the shielded pool");
        assert_eq!(st.native_supply() + st.shielded().value_locked(), 1_000_000, "total issuance (both domains) grows by exactly the reward");
    }

    /// Register `wallet`'s shield key derived from `seed`, returning the wire pk.
    /// THE FULL-POOL GATE — a registered miner must never be paid NOTHING.
    ///
    /// Live incident, 2026-08-26: the shielded pool reached its fixed capacity
    /// (32,768 notes). A registered wallet always took the shielded path — the
    /// transparent fallback keys off NON-REGISTRATION, not off whether the note can
    /// actually be minted — so its cut could no longer be applied. The failed coinbase
    /// was then dropped whole and silently, and the miner earned +0 while unregistered
    /// miners were paid normally. Measured: 31.66 MH/s, zero credit.
    ///
    /// A full pool must degrade to a transparent credit, never to an unpayable block.
    #[test]
    fn a_full_pool_rotates_and_keeps_paying_a_registered_miner_privately() {
        let mut st = SigilState::new();
        let seed = [0x42u8; 32];
        let acct = sigil_shield::wallet::ShieldedAccount::from_seed(seed);
        let pk_shield = sigil_shield::note_v1::to_wire(acct.public_key());
        let miner: WalletId = [0x99u8; 32];

        sigil_state::commit_state_transition(
            &mut st,
            &StateTransition {
                at_height: 0,
                mutations: vec![StateMutation::RegisterShieldedAddress {
                    wallet: miner,
                    pk_shield,
                    pk_sqi: None, pk_encrypt: None,
                }],
            },
            0,
        )
        .unwrap();

        // Fill the pool to the brim. Distinct heights give distinct commitments, so the
        // duplicate-commitment replay guard does not fire; amount 1 keeps the locked
        // value trivial.
        let filler: Vec<StateMutation> = (0..sigil_state::shielded::POOL_CAPACITY as u64)
            .map(|i| {
                let cm = sigil_shield::note_v1::coinbase_commitment_wire(i, &pk_shield, 1)
                    .expect("in range");
                StateMutation::ShieldedCoinbase { pk_shield, amount: 1, cm, ct: None }
            })
            .collect();
        sigil_state::commit_state_transition(
            &mut st,
            &StateTransition { at_height: 0, mutations: filler },
            0,
        )
        .expect("filling the pool to capacity must itself succeed");
        assert_eq!(
            st.shielded().len(),
            sigil_state::shielded::POOL_CAPACITY,
            "the pool must actually be full for this test to mean anything"
        );

        let before = st.balance_of(&miner, &NATIVE);
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let (tr, ..) = build_block_body_for_shares(&st, 1, Some(1_000_000), &[], miner, &shares);

        // 2026-08-27: this test used to assert the TRANSPARENT FALLBACK — that a full
        // pool pays a registered miner in the clear rather than not at all. That was the
        // right behaviour when a full pool meant `PoolFull` and a silently dropped
        // coinbase. Epoch rotation changed it: appending to a full pool now seals the
        // generation and opens a fresh one, so the note CAN be minted and the miner keeps
        // being paid PRIVATELY, which is what they registered for.
        //
        // Keeping the old assertion would have pinned the behaviour that made rotation
        // unreachable — measured live: the pool sat at exactly 32,768/32,768 with
        // `epoch 0, sealed 0` for over two hours, every registered miner's reward quietly
        // paid in the clear, no error line anywhere.
        assert!(
            tr.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedCoinbase { .. })),
            "a full pool must ROTATE and still mint a note — falling back to transparent \
             here is what prevented rotation from ever happening: {:?}",
            tr.mutations
        );

        // And it must actually apply: rotation happens inside the append, so this is also
        // the assertion that a mid-block rotation commits cleanly.
        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &tr, 1)
            .expect("the rotating coinbase must APPLY — the whole point is that the block is payable");
        assert_eq!(
            after.shielded().epoch(),
            1,
            "the pool must have rotated into a new generation"
        );
        assert_eq!(
            after.shielded().archive().len(),
            1,
            "the filled generation must be SEALED, not discarded — its notes stay spendable"
        );
        assert_eq!(
            after.shielded().archive()[0].notes.len(),
            sigil_state::shielded::POOL_CAPACITY,
            "the sealed epoch must hold every note it had"
        );
        assert!(
            after.shielded().len() >= 1,
            "the fresh generation must have taken the new note"
        );
        assert_eq!(
            after.balance_of(&miner, &NATIVE),
            before,
            "a registered miner keeps being paid privately — nothing should land transparently"
        );
        assert!(
            after.shielded().value_locked() > st.shielded().value_locked(),
            "the reward must be locked into the pool; rotation moves no value but the new \
             note adds to it"
        );
    }

    fn register_shield(st: &mut SigilState, wallet: WalletId, seed: [u8; 32]) -> [u8; 32] {
        let acct = sigil_shield::wallet::ShieldedAccount::from_seed(seed);
        let pk_shield = sigil_shield::note_v1::to_wire(acct.public_key());
        sigil_state::commit_state_transition(
            st,
            &StateTransition {
                at_height: 0,
                mutations: vec![StateMutation::RegisterShieldedAddress { wallet, pk_shield, pk_sqi: None, pk_encrypt: None }],
            },
            0,
        )
        .unwrap();
        pk_shield
    }

    /// THE FIX (2026-08-26). The master wallet takes 5% of EVERY block's reward,
    /// from every miner, whether or not that miner has opted into shielded rewards
    /// — so it is the one coinbase cut that can grow the anonymity set in
    /// proportion to total network hashrate without persuading anybody. Before
    /// this, `split_coinbase_mutations` looked up the shield registry only for the
    /// mining wallet's own cut, so a registered master was still paid in the clear
    /// and that leverage was simply unavailable.
    #[test]
    fn a_registered_master_wallets_cut_mints_a_shielded_note() {
        let mut st = SigilState::new();
        let master: WalletId = [0x11u8; 32];
        set_master(&mut st, master);
        let pk_master = register_shield(&mut st, master, [0x77u8; 32]);

        let miner: WalletId = [0x88u8; 32]; // deliberately NOT registered
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let reward = 1_000_000u128;
        let muts = split_coinbase_mutations(&st, 1, reward, miner, &shares);

        let master_note = muts.iter().find_map(|m| match m {
            StateMutation::ShieldedCoinbase { pk_shield, amount, .. } if *pk_shield == pk_master => Some(*amount),
            _ => None,
        });
        assert_eq!(
            master_note,
            Some(reward * sigil_bank::MASTER_MINING_FEE_BPS / 10_000),
            "the master's 5% must enter the shielded pool once it has registered a key: {muts:?}"
        );
        assert!(
            !muts.iter().any(|m| matches!(m, StateMutation::SetBalance { wallet, .. } if *wallet == master)),
            "a registered master must get NO transparent credit"
        );
        assert!(
            muts.iter().any(|m| matches!(m, StateMutation::SetBalance { wallet, .. } if *wallet == miner)),
            "the UNREGISTERED miner is untouched by this — still a transparent credit"
        );

        // Conservation still spans both domains exactly.
        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &StateTransition { at_height: 1, mutations: muts }, 1).unwrap();
        assert_eq!(
            after.native_supply() + after.shielded().value_locked(),
            reward,
            "every unit of the reward is still accounted for, just split across the two domains"
        );
    }

    /// The commons tithe is taken on every block too, so the same rule has to
    /// apply to it — otherwise a registered commons wallet silently stays public.
    #[test]
    fn a_registered_commons_wallets_tithe_mints_a_shielded_note() {
        let mut st = SigilState::new();
        let master: WalletId = [0x11u8; 32];
        set_master(&mut st, master);
        let pk_commons = register_shield(&mut st, sigil_bank::COMMONS_WALLET, [0x66u8; 32]);

        let miner: WalletId = [0x88u8; 32];
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let muts = split_coinbase_mutations(&st, 1, 1_000_000, miner, &shares);

        assert!(
            muts.iter().any(|m| matches!(m, StateMutation::ShieldedCoinbase { pk_shield, .. } if *pk_shield == pk_commons)),
            "the commons tithe must mint a note once the commons wallet is registered: {muts:?}"
        );
        assert!(
            !muts.iter().any(|m| matches!(m, StateMutation::SetBalance { wallet, .. } if *wallet == sigil_bank::COMMONS_WALLET)),
            "a registered commons wallet must get NO transparent credit"
        );
    }

    /// Guard on the one way this could destroy money rather than shield it.
    /// `ShieldedPool::append_note` REJECTS a duplicate commitment, and that error
    /// fails the whole `commit_state_transition` — dropping the ENTIRE block's
    /// coinbase, not just the colliding cut. Two credits collide only when they
    /// share `(height, pk_shield, amount)`, which needs two distinct wallets to have
    /// registered the SAME shield key and drawn equal cuts. Rare; the blast radius
    /// is a whole block reward, so the second one must fall back to transparent.
    #[test]
    fn a_colliding_commitment_falls_back_to_transparent_instead_of_losing_the_reward() {
        let mut st = SigilState::new();
        let seed = [0x55u8; 32];
        // Two DIFFERENT wallets sharing one shield key — the only way to collide.
        let a: WalletId = [0xA1u8; 32];
        let b: WalletId = [0xB2u8; 32];
        let pk = register_shield(&mut st, a, seed);
        assert_eq!(register_shield(&mut st, b, seed), pk, "both wallets share one shield key");

        // Equal weights ⇒ equal cuts ⇒ identical (height, pk, amount) ⇒ identical cm.
        let shares = std::collections::HashMap::from([(a, 1u64), (b, 1u64)]);
        let reward = 1_000_000u128;
        let muts = split_coinbase_mutations(&st, 1, reward, a, &shares);

        let notes = muts.iter().filter(|m| matches!(m, StateMutation::ShieldedCoinbase { .. })).count();
        assert_eq!(notes, 1, "exactly one of the two colliding cuts may mint a note: {muts:?}");

        // The decisive assertion: the block still commits and pays out in full.
        let mut after = st.clone();
        sigil_state::commit_state_transition(&mut after, &StateTransition { at_height: 1, mutations: muts }, 1)
            .expect("the coinbase must still commit — a collision must not fail the block");
        assert_eq!(
            after.native_supply() + after.shielded().value_locked(),
            reward,
            "the full reward is still issued despite the collision"
        );
    }

    /// Regression twin of the test above: an UNREGISTERED miner in the exact
    /// same pool-split path must be completely unaffected — transparent
    /// credit, same as before this session's change.
    #[test]
    fn pool_share_credit_stays_transparent_for_an_unregistered_miner() {
        let st = SigilState::new();
        let miner: WalletId = [0x88u8; 32];
        let shares = std::collections::HashMap::from([(miner, 1u64)]);
        let (tr, ..) = build_block_body_for_shares(&st, 1, Some(1_000_000), &[], miner, &shares);
        assert_eq!(
            tr.mutations,
            vec![StateMutation::SetBalance { wallet: miner, token: NATIVE, amount: 1_000_000 }],
            "unregistered miner keeps the exact pre-existing transparent-credit behavior"
        );
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
    /// Step 2 gate: two nodes building the same (coinbase + txs) body reach identical
    /// roots, and a REFUSED transaction is refused identically on both.
    ///
    /// 2026-08-28: this test used to assert that a transparent `SigilTx::Send` was
    /// INCLUDED and moved balances. That stopped being true when `SHIELDED_ONLY_HEIGHT`
    /// was set to 0 — `apply_tx_at` now returns `TransparentSendRetired` for a transparent
    /// send at EVERY height, deliberately, because a transparent send publishes exactly
    /// the payer/payee link and amount the pool exists to hide. The test did not fail at
    /// the time because sigil-node's lib-test target did not compile (three wrong module
    /// paths in `snapshot.rs`, a stale `mint_next_block` arity, a missing dev-dependency),
    /// so it had never actually run.
    ///
    /// Rewritten to assert what the chain really does, keeping the property this test is
    /// FOR — determinism. Refusal is a consensus outcome like any other: both nodes must
    /// drop the same transaction and land on the same root, or they fork on invalid input.
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

        // The transparent send is retired chain-wide, so neither node includes it.
        assert!(sigil_tx::SHIELDED_ONLY_HEIGHT == 0, "this test encodes the shielded-only policy");
        assert_eq!(inc1.len(), 0, "a transparent send is refused (TransparentSendRetired)");
        assert_eq!(inc2.len(), 0, "…and refused identically on the second node");
        assert_eq!(r1.wallet_state_root, r2.wallet_state_root, "two nodes → identical roots (deterministic)");
        assert!(ev1.is_empty(), "a refused send emits no Send/Receive events");

        // Applying the body must move no money: the refusal is total, not partial.
        let mut node = base.clone();
        let computed = sigil_state::commit_state_transition(&mut node, &tr1, 1).unwrap();
        assert_eq!(computed.wallet_state_root, r1.wallet_state_root, "predicted == applied roots");
        assert_eq!(node.balance_of(&recipient, &NATIVE), 0, "recipient received nothing");
        assert_eq!(node.balance_of(&sender, &NATIVE), 1_000_000_000, "sender was not debited — not even the fee");
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
                topology_commitment: None,
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
            final_blue_depth: None,
            saturated_self_heal_window: 64,
            // 2026-08-26: field added by the pending-eviction work; 0 = disabled,
            // which is the pre-existing behavior this determinism test asserts.
            pending_max_tip_lag: 0,
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

    /// Operator-directed 2026-08-16: before flipping SIGIL_EMISSION_ADAPTIVE=1
    /// on the live producer, prove the ADAPTIVE controller (not the flat
    /// schedule the test above covers) replays deterministically across
    /// independent nodes, conserves money exactly, never mints a 0-reward
    /// block while an era is active, and — the actual point of porting this
    /// from Quillon — holds annual emission close to the era-0 target EVEN
    /// THOUGH the simulated block rate swings through the real 8-60 blk/s
    /// adaptive-governor range instead of running flat. No existing test
    /// touched EmissionController through a real multi-block, multi-node
    /// chronos-style replay before this.
    #[test]
    fn chronos_adaptive_emission_multinode_determinism_and_rate_accuracy() {
        use sigil_emission::controller::EmissionController;
        use std::collections::HashMap;

        const NODES: usize = 4;
        const BLOCKS: u64 = 6_000;
        const GENESIS_TS: u64 = 1_786_752_000; // matches live_reward_projection.rs's live anchor

        // A deterministic rate CYCLE, not a flat rate — mimicking the real
        // adaptive governor's observed bursts and lulls (SIGIL_RATE_MIN=8,
        // SIGIL_RATE_MAX=60). Rate-independence is the property under test;
        // a constant rate could pass by accident even with a broken formula.
        let rate_cycle: [f64; 8] = [8.0, 55.0, 12.0, 60.0, 20.0, 8.0, 45.0, 30.0];

        // Producer: builds the canonical order with a REAL EmissionController,
        // starting at its own genesis (t=0 elapsed) — the cleanest reference
        // case for the algorithm itself. (Retrofitting onto an ALREADY-running
        // chain's historical over-mint is a separate, already-quantified
        // question — see sigil-emission/examples/live_reward_projection.rs.)
        let mut producer = SigilState::new();
        let mut emission = EmissionController::new(GENESIS_TS);
        let mut order: Vec<StateTransition> = Vec::new();
        let mut recorded_rewards: Vec<u128> = Vec::new();
        let mut sim_ts = GENESIS_TS;
        let mut zero_reward_blocks = 0u64;

        for h in 1..=BLOCKS {
            let rate = rate_cycle[(h as usize) % rate_cycle.len()];
            sim_ts += (1.0 / rate).round().max(1.0) as u64;
            // "live" sample: block_ts == now, so smoothed_rate() actually
            // reflects this simulated cadence instead of the cold-start
            // 1.0 blk/s fallback.
            emission.add_block(h, sim_ts, sim_ts);
            let reward = emission.calculate_block_reward(sim_ts, producer.native_supply());
            if reward == 0 {
                zero_reward_blocks += 1;
            }
            let winner = producer_wallet();
            let (tr, ..) = build_block_body_for_shares(
                &producer, h, Some(reward), &[], winner, &HashMap::from([(winner, 1u64)]),
            );
            sigil_state::commit_state_transition(&mut producer, &tr, h).unwrap();
            emission.record_emission(reward); // AFTER commit, per the doc'd rule
            order.push(tr);
            recorded_rewards.push(reward);
        }

        // N independent followers replay the IDENTICAL recorded order from
        // genesis — proving the STATE MACHINE is deterministic given the
        // amounts the producer already decided, exactly like the flat-schedule
        // test above, but now exercising adaptive-controller-sized reward
        // values (which vary block-to-block, unlike the flat schedule's
        // constant amount — a real chance for an integer-math edge case to
        // diverge that the flat test can't exercise).
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

        let p_root = producer.roots().wallet_state_root;
        let p_supply = producer.native_supply();
        for i in 0..NODES {
            assert_eq!(roots[i], p_root, "node {i} wallet_root diverged from producer under the ADAPTIVE controller");
            assert_eq!(supplies[i], p_supply, "node {i} supply diverged from producer under the ADAPTIVE controller");
        }

        // Conservation: chain supply == Σ recorded per-block rewards == the
        // controller's own persisted watermark. All three must agree, or
        // money is being created or lost somewhere between the controller,
        // the coinbase mutation, and the chokepoint.
        let sum_rewards: u128 = recorded_rewards.iter().sum();
        assert_eq!(p_supply, sum_rewards, "minted supply == Σ recorded rewards — no phantom emission under the adaptive controller");
        assert_eq!(p_supply, emission.total_cumulative_emission, "chain supply == controller's own watermark — the persisted number tells the truth");

        // Hard safety rule this codebase documents at the top of controller.rs:
        // "the caller MUST abort block production — never mint a 0-reward
        // block." Prove adaptive_reward's own floor actually holds it.
        assert_eq!(zero_reward_blocks, 0, "adaptive_reward must never return 0 while an era is active — {zero_reward_blocks} of {BLOCKS} blocks got zero");

        // Rate-independence: the actual point of porting this from Quillon.
        // Despite the simulated rate swinging through the full 8-60 blk/s
        // adaptive-governor range block-to-block, the ACHIEVED annual rate
        // over this run must land close to the era-0 target — a flat
        // height-halving schedule has no such property at all (its annual
        // rate is directly proportional to whatever the block rate happens
        // to be, which is the bug this whole activation is meant to fix).
        let elapsed_secs = sim_ts - GENESIS_TS;
        let target_annual = emission.annual_emission(0);
        let achieved_annual =
            (p_supply as f64) * (sigil_emission::controller::SECONDS_PER_YEAR as f64) / (elapsed_secs as f64);
        let pct_off = (achieved_annual - target_annual as f64).abs() / target_annual as f64 * 100.0;
        assert!(
            pct_off < 15.0,
            "achieved annual rate {achieved_annual:.0} should track the {target_annual}-raw/yr target within 15% \
             despite the simulated 8-60 blk/s rate swings — got {pct_off:.1}% off (this IS the property that \
             makes it 'Quillon-style': the same annual emission whether the chain runs fast or slow)"
        );
    }
}

/// Seal a coinbase note's `(value, blinding)` for its recipient, so the wallet can find
/// it by trial-decryption instead of scanning every block body.
///
/// Returns `None` when the wallet published a shield key but no delivery key (older
/// registrations), or if sealing fails. The note is still minted in that case — it is
/// simply as undiscoverable as it was before, never lost.
fn seal_coinbase_note(
    work: &SigilState,
    to: &WalletId,
    pk_shield_wire: &[u8; 32],
    amount: u128,
    height: u64,
) -> Option<String> {
    let enc = work.shielded().encrypt_key(to)?;
    let pk = sigil_shield::note_v1::from_wire(pk_shield_wire).ok()?;
    let value = u64::try_from(amount).ok()?;
    let pt = sigil_shield::note_cipher::NotePlaintext {
        value,
        blinding: sigil_shield::note_v1::coinbase_blinding(height, pk),
    };
    let addr = sigil_shield::note_cipher::ShieldedAddress::new(pk, &hex::encode(enc));
    sigil_shield::note_cipher::seal_note(&pt, &addr).ok().map(|c| c.0)
}

