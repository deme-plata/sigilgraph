//! Scheduled-payment PLANNING — deliberately not execution. Read this
//! before anyone is tempted to add a "fire the payment" loop here.
//!
//! **Why the original's approach can't be ported:** Quillon's
//! `calendar_api.rs` executed a scheduled transaction by taking a write
//! lock on an in-memory `wallet_balances: HashMap<[u8;32], u128>`,
//! subtracting from the sender and adding to the recipient directly, and
//! recording a FAKE "tx hash" (`hex::encode(&event.wallet[..8])` — not a
//! real transaction, just the first 8 bytes of the sender's own address).
//! No signature, no chokepoint, no real chain record. That bypasses every
//! one of this workspace's balance-integrity rules and would be an actual
//! money-safety incident if reproduced on SIGIL's real ledger.
//!
//! **What a REAL scheduled-payment executor needs — none of which exists
//! yet, and none of which this crate should build without operator
//! sign-off, given it moves real funds:**
//!   1. A way for a citizen to pre-authorize a FUTURE send without being
//!      online at execution time — e.g. a time-locked, pre-signed
//!      transaction the citizen signs NOW (binding exactly this payment,
//!      this amount, this recipient, not-valid-before this timestamp) that
//!      the network can submit later. This is a real signature-scheme /
//!      chain-rule design question, not a storage-layer one.
//!   2. Submission through the REAL `sigil-tx` apply path — the same
//!      chokepoint a live send goes through — never a direct balance edit.
//!   3. A revocation window (the citizen must be able to cancel a planned
//!      payment before it fires).
//!
//! Until that exists, a [`crate::models::ScheduledTransaction`] is exactly
//! what its status enum says: `Planned`. This module's only job is to
//! create that plan and let the citizen see/cancel it — see
//! [`create_planned_payment`] and [`cancel`].

use crate::models::{CalendarEvent, CalendarEventType, ScheduledTransaction, ScheduledTxStatus};
use crate::store::{CalendarStore, StoreError};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_id(wallet_id: &str, to_wallet: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"calendar-scheduled-tx");
    h.update(wallet_id.as_bytes());
    h.update(to_wallet.as_bytes());
    h.update(&now_ms().to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..12])
}

/// Create a calendar event carrying a PLANNED (not executed) payment.
/// Returns the stored [`CalendarEvent`] — its `scheduled_tx.status` is
/// always [`ScheduledTxStatus::Planned`]; nothing in this crate ever moves
/// it out of that state.
pub fn create_planned_payment(
    store: &CalendarStore,
    wallet_id: &str,
    title: &str,
    start_time: u64,
    to_wallet: &str,
    token: &str,
    amount: &str,
) -> Result<CalendarEvent, StoreError> {
    let now = now_ms();
    let event = CalendarEvent {
        id: new_id(wallet_id, to_wallet),
        wallet_id: wallet_id.to_string(),
        title: title.to_string(),
        description: None,
        event_type: CalendarEventType::ScheduledTransaction,
        start_time,
        end_time: None,
        all_day: false,
        recurring: None,
        color: None,
        reminder_minutes: None,
        scheduled_tx: Some(ScheduledTransaction {
            to_wallet: to_wallet.to_string(),
            token: token.to_string(),
            amount: amount.to_string(),
            status: ScheduledTxStatus::Planned,
            tx_hash: None,
            error: None,
        }),
        shared: false,
        created_at: now,
        updated_at: None,
        cancelled: false,
        source_peer: None,
    };
    store.create_event(&event)?;
    Ok(event)
}

/// Cancel a planned payment. Marks the EVENT cancelled and the plan's
/// status `Cancelled` — a real cancellation record, not a silent delete,
/// so "I planned this and then changed my mind" stays visible in history.
pub fn cancel(store: &CalendarStore, event_id: &str) -> Result<CalendarEvent, StoreError> {
    store.update_event(event_id, |e| {
        e.cancelled = true;
        e.updated_at = Some(now_ms());
        if let Some(tx) = e.scheduled_tx.as_mut() {
            tx.status = ScheduledTxStatus::Cancelled;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store(name: &str) -> CalendarStore {
        let dir = std::env::temp_dir().join(format!("sigil-calendar-scheduled-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        CalendarStore::open(dir).expect("open test store")
    }

    #[test]
    fn a_planned_payment_starts_and_stays_planned() {
        let store = open_test_store("plan");
        let event =
            create_planned_payment(&store, "wallet-a", "Rent", 5000, "wallet-landlord", "SIGIL", "100").unwrap();
        let tx = event.scheduled_tx.as_ref().expect("has a scheduled_tx");
        assert_eq!(tx.status, ScheduledTxStatus::Planned);
        assert!(tx.tx_hash.is_none(), "nothing in this crate should ever set a tx_hash");
    }

    #[test]
    fn cancel_marks_both_the_event_and_the_plan_cancelled() {
        let store = open_test_store("cancel");
        let event =
            create_planned_payment(&store, "wallet-a", "Rent", 5000, "wallet-landlord", "SIGIL", "100").unwrap();
        let cancelled = cancel(&store, &event.id).unwrap();
        assert!(cancelled.cancelled);
        assert_eq!(cancelled.scheduled_tx.unwrap().status, ScheduledTxStatus::Cancelled);
    }
}
