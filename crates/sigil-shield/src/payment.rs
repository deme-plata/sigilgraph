//! Turning "send X to Y" into a plan the wallet can execute — the piece that was missing.
//!
//! # Why this module exists
//!
//! [`crate::wallet::build_spend`] takes a `store_position`: the CALLER decides which note
//! to spend. Nothing in this crate ever helped it decide, so every client — the Android
//! core, the browser wallet, the MCP tools — invented its own rule, and all of them
//! inherited the same wrong mental model: that a wallet has *a balance* it can draw on.
//!
//! It does not. The spend circuit is 1-in/2-out: exactly ONE note goes in, and a payment
//! and a change note come out. So the amount you can send in one transaction is not your
//! balance — it is your **largest single note**. A wallet holding 11 SIGIL spread over 500
//! notes can send 0.02 SIGIL, and every honest-looking balance it shows you is a lie about
//! what you can do with it.
//!
//! That is not a hypothetical. Measured live 2026-08-28 at height 311,862: 836,536 notes,
//! 18,039 SIGIL locked in them, 34 wallets — an average note of 0.0216 SIGIL, and exactly
//! one nullifier ever revealed. Someone reported it from the phone app as "I can send
//! 0.006 out of 11 SIGIL". They were reading the instrument correctly; the instrument was
//! reporting a number that did not mean what it appeared to mean.
//!
//! # What the plan does about it
//!
//! There is a second pot of money the old rule ignored: the **transparent balance**, which
//! is additive and (after `sigil_tx::TRANSPARENT_COINBASE_HEIGHT`) is where mining rewards
//! land. A `Shield` turns any part of it into ONE note of a standard denomination. So when
//! no single note is big enough, the wallet does not have to fail — it can mint a note that
//! IS big enough, out of money the user already has, and then spend it.
//!
//! [`plan_payment`] returns which of those two things to do, or an honest account of why
//! neither works. It answers in the user's terms — an amount they wanted to send — and
//! never asks them to think about notes, pools, denominations or anonymity sets.
//!
//! # What it deliberately does NOT do
//!
//! It never combines two notes, because the circuit cannot: 1-in means one input, so
//! consolidation is arithmetically impossible today, not merely unimplemented. A K-in
//! circuit is the real fix and is separate work. Until it lands, `ShieldThenSpend` is the
//! honest workaround and this module says so rather than pretending otherwise.

use crate::wallet::NoteStore;

/// What the wallet should actually do to make a payment happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentPlan {
    /// One existing note already covers it. Spend it directly.
    SpendNote {
        /// Index into `NoteStore::notes` — pass straight to `build_spend`.
        store_position: usize,
        /// That note's value, for display.
        note_value: u64,
        /// What comes back as the self-change output.
        change: u64,
    },

    /// No single note is big enough, but the transparent balance can mint one that is.
    ///
    /// Execute as: `Shield { amount: shield }` → wait for it to land → `build_spend` on the
    /// resulting note. To the user this is one action; the two steps are ours, not theirs.
    ShieldThenSpend {
        /// A standard denomination, so the ramp does not leak an unusual amount.
        shield: u128,
        /// The note that shield produces; `shield` as a `u64` note value.
        then_spend_value: u64,
        /// What will come back as change once it is spent.
        change: u64,
    },

    /// The payment cannot be made. The fields say exactly why, in the user's units.
    Insufficient {
        /// Amount + fee, what the payment actually needs.
        needed: u64,
        /// Best single note — the real one-transaction ceiling today.
        best_note: u64,
        /// Total across every unspent note. Bigger than `best_note` is the whole problem.
        shielded_total: u128,
        /// Transparent balance available to shield from.
        transparent: u128,
        /// Which wall was hit.
        reason: Shortfall,
    },
}

/// Why a payment could not be planned. Distinguished because they need different words to
/// the user and different fixes from us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortfall {
    /// There genuinely is not enough money anywhere. The only honest "no".
    NotEnoughMoney,

    /// Enough money exists in total, but it is scattered across notes too small to use and
    /// the transparent balance cannot mint a big enough one either.
    ///
    /// This is the 1-in/2-out wall, and it is the case a K-in circuit removes. Worth
    /// naming separately because telling a user "insufficient funds" while showing them a
    /// balance that covers it is the single most confusing thing a wallet can do.
    FragmentedAcrossNotes,

    /// Enough transparent money, but no standard denomination fits between what is needed
    /// and what is available — e.g. needing 6 while holding 7, with a 5/10 ladder.
    /// Narrow, and it resolves itself as the balance grows.
    NoDenominationFits,
}

/// The largest amount this wallet can send in ONE transaction, right now.
///
/// This — not the sum of the notes — is what a Send screen should offer as "max", because
/// it is the only number that is true about a single payment.
pub fn spendable_in_one_go(store: &NoteStore) -> u64 {
    store
        .notes
        .iter()
        .filter(|n| !n.spent && n.position.is_some())
        .map(|n| n.value)
        .max()
        .unwrap_or(0)
}

/// Everything the wallet holds in the pool, spendable or not.
///
/// Report it as *held*, never as *sendable*. The gap between this and
/// [`spendable_in_one_go`] is exactly the fragmentation the user is feeling.
pub fn shielded_total(store: &NoteStore) -> u128 {
    store
        .notes
        .iter()
        .filter(|n| !n.spent && n.position.is_some())
        .map(|n| n.value as u128)
        .sum()
}

/// Plan a payment of `amount` with `fee`, given the transparent balance and the chain's
/// denomination ladder (`sigil_state::shielded::DENOMINATIONS`).
///
/// `shield_fee` is what a `Shield` transaction itself costs, charged transparently — so a
/// ramp needs `shield + shield_fee` of transparent balance, not just `shield`.
///
/// Note choice is **smallest note that covers it**. Spending the largest would work too and
/// is tempting, but it destroys the only note capable of making a large payment in order to
/// make a small one. Smallest-that-covers keeps the big notes intact.
pub fn plan_payment(
    store: &NoteStore,
    transparent: u128,
    denominations: &[u128],
    amount: u64,
    fee: u64,
    shield_fee: u128,
) -> PaymentPlan {
    let shielded_total = shielded_total(store);
    let best_note = spendable_in_one_go(store);

    let Some(needed) = amount.checked_add(fee) else {
        return PaymentPlan::Insufficient {
            needed: u64::MAX,
            best_note,
            shielded_total,
            transparent,
            reason: Shortfall::NotEnoughMoney,
        };
    };

    // 1. Smallest unspent, on-chain note that covers amount + fee.
    let pick = store
        .notes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.spent && n.position.is_some() && n.value >= needed)
        .min_by_key(|(_, n)| n.value);

    if let Some((store_position, note)) = pick {
        return PaymentPlan::SpendNote {
            store_position,
            note_value: note.value,
            change: note.value - needed,
        };
    }

    // 2. No note is big enough. Mint one from the transparent balance: the smallest
    //    standard denomination that covers the payment AND that the balance can afford
    //    once the ramp's own fee is paid.
    let affordable = |d: u128| d.checked_add(shield_fee).is_some_and(|c| c <= transparent);
    let fits = denominations
        .iter()
        .copied()
        .filter(|d| *d >= needed as u128 && u64::try_from(*d).is_ok())
        .find(|d| affordable(*d));

    if let Some(shield) = fits {
        let v = shield as u64; // checked by the try_from filter above
        return PaymentPlan::ShieldThenSpend {
            shield,
            then_spend_value: v,
            change: v - needed,
        };
    }

    // 3. Neither route works — say which wall was hit.
    let could_ever_afford = denominations
        .iter()
        .any(|d| *d >= needed as u128 && affordable(*d));
    let reason = if shielded_total + transparent < needed as u128 {
        Shortfall::NotEnoughMoney
    } else if !could_ever_afford && transparent >= needed as u128 + shield_fee {
        // The money is transparent and sufficient; only the ladder is in the way.
        Shortfall::NoDenominationFits
    } else {
        Shortfall::FragmentedAcrossNotes
    };

    PaymentPlan::Insufficient { needed, best_note, shielded_total, transparent, reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{NoteStore, OwnedNote};
    use winterfell::math::fields::f64::BaseElement;

    /// The ladder the chain actually uses, truncated to what these tests need.
    const DENOMS: &[u128] = &[1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];

    fn note(value: u64, on_chain: bool, spent: bool) -> OwnedNote {
        OwnedNote {
            index: None,
            value,
            blinding: BaseElement::new(7),
            position: if on_chain { Some(0) } else { None },
            spent,
        }
    }

    fn store_of(values: &[u64]) -> NoteStore {
        let mut s = NoteStore::new();
        s.notes = values.iter().map(|v| note(*v, true, false)).collect();
        s
    }

    /// THE BUG, REPRODUCED. A wallet holding plenty, in pieces, could send almost none of
    /// it — and this is what the phone app was reporting.
    #[test]
    fn a_fragmented_wallet_cannot_send_its_own_balance() {
        let store = store_of(&[5; 500]); // 2,500 held across 500 notes
        assert_eq!(shielded_total(&store), 2_500, "the balance is real");
        assert_eq!(spendable_in_one_go(&store), 5, "…and 5 is all a single send can move");

        // With no transparent balance there is no way out — and the reason must NOT be
        // "not enough money", because there plainly is enough.
        let plan = plan_payment(&store, 0, DENOMS, 1_000, 0, 0);
        assert!(
            matches!(plan, PaymentPlan::Insufficient { reason: Shortfall::FragmentedAcrossNotes, .. }),
            "fragmentation must be named as fragmentation, not as poverty: {plan:?}"
        );
    }

    /// THE FIX. The same fragmented wallet, now with a transparent balance behind it —
    /// which is where mining rewards land after `TRANSPARENT_COINBASE_HEIGHT`.
    #[test]
    fn a_transparent_balance_rescues_the_same_payment() {
        let store = store_of(&[5; 500]);
        let plan = plan_payment(&store, 2_000, DENOMS, 1_000, 0, 0);
        assert_eq!(
            plan,
            PaymentPlan::ShieldThenSpend { shield: 1_000, then_spend_value: 1_000, change: 0 },
            "mint exactly one note big enough, on a standard denomination"
        );
    }

    /// A ramp must round UP to a real denomination, never to the raw amount — an unusual
    /// amount on the transparent side links the two halves of the ramp for free.
    #[test]
    fn the_ramp_lands_on_a_standard_denomination() {
        let store = NoteStore::new();
        let plan = plan_payment(&store, 10_000, DENOMS, 637, 3, 0);
        let PaymentPlan::ShieldThenSpend { shield, change, .. } = plan else {
            panic!("expected a ramp: {plan:?}");
        };
        assert!(DENOMS.contains(&shield), "{shield} is not a standard denomination");
        assert_eq!(shield, 1_000, "smallest denomination covering 637 + 3");
        assert_eq!(change, 360, "the rest comes back as change");
    }

    /// Spend the SMALLEST note that covers it. Spending the largest also "works" and is the
    /// obvious thing to write, but it burns the only note capable of a large payment to
    /// make a small one — and the wallet cannot rebuild it, because 1-in/2-out can never
    /// merge notes back together.
    #[test]
    fn picks_the_smallest_covering_note_and_keeps_the_big_one_intact() {
        let store = store_of(&[10, 5_000, 50, 200]);
        let plan = plan_payment(&store, 0, DENOMS, 40, 2, 0);
        let PaymentPlan::SpendNote { store_position, note_value, change } = plan else {
            panic!("expected a direct spend: {plan:?}");
        };
        assert_eq!(note_value, 50, "50 is the smallest note covering 42");
        assert_eq!(store_position, 2);
        assert_eq!(change, 8);
        assert_eq!(spendable_in_one_go(&store), 5_000, "the big note is untouched");
    }

    /// Notes that are spent, or not yet on chain, are not money you can send. Counting
    /// either one inflates the ceiling and produces a payment that fails at proving time.
    #[test]
    fn spent_and_unconfirmed_notes_are_not_spendable() {
        let mut store = NoteStore::new();
        store.notes = vec![
            note(1_000, true, true),   // spent
            note(2_000, false, false), // not yet on chain
            note(7, true, false),      // the only real one
        ];
        assert_eq!(spendable_in_one_go(&store), 7);
        assert_eq!(shielded_total(&store), 7);
        assert!(matches!(
            plan_payment(&store, 0, DENOMS, 100, 0, 0),
            PaymentPlan::Insufficient { best_note: 7, .. }
        ));
    }

    /// A ramp costs a fee of its own, paid transparently. A balance that covers the
    /// denomination but not the denomination PLUS its fee cannot ramp — and must not be
    /// told it can, or the wallet builds a transaction the chain will refuse.
    #[test]
    fn the_ramp_fee_is_charged_against_the_transparent_balance() {
        let store = NoteStore::new();
        assert!(
            matches!(plan_payment(&store, 1_000, DENOMS, 900, 0, 0), PaymentPlan::ShieldThenSpend { shield: 1_000, .. }),
            "1,000 covers a 1,000 ramp when the ramp is free"
        );
        assert!(
            matches!(plan_payment(&store, 1_000, DENOMS, 900, 0, 5), PaymentPlan::Insufficient { .. }),
            "…but not once the ramp itself costs 5"
        );
    }

    /// Genuinely broke is its own answer, and the only one that should say so.
    #[test]
    fn actually_insufficient_says_so() {
        let store = store_of(&[3]);
        let plan = plan_payment(&store, 2, DENOMS, 1_000, 0, 0);
        assert!(
            matches!(plan, PaymentPlan::Insufficient { reason: Shortfall::NotEnoughMoney, .. }),
            "{plan:?}"
        );
    }

    /// The ladder itself can be the obstacle: 7 in hand, 6 needed, rungs at 5 and 10.
    /// Rare and self-resolving, but it must not masquerade as fragmentation — the user has
    /// no notes at all here, so "your money is split up" would be nonsense.
    #[test]
    fn a_denomination_gap_is_reported_as_a_denomination_gap() {
        let store = NoteStore::new();
        let plan = plan_payment(&store, 7, DENOMS, 6, 0, 0);
        assert!(
            matches!(plan, PaymentPlan::Insufficient { reason: Shortfall::NoDenominationFits, .. }),
            "{plan:?}"
        );
    }
}
