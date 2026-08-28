//! The SIGIL Bank's labeled mass-mail sender — Viktor's explicit ask
//! ("sigil bank shuold have a label also because i should be able to mass
//! mail to all sigil users from bank"). New in this port, no original to
//! draw from.
//!
//! **What this deliberately does NOT do yet — read before wiring this to
//! anything an HTTP endpoint can trigger:**
//!   - Does not itself check that `authorized_by` is actually allowed to
//!     send bank mail. That's a real authorization policy (which wallet(s)
//!     count as "the bank"?) that belongs at whatever layer calls this —
//!     baking a hardcoded wallet check in here would make it untestable
//!     and inflexible. Callers MUST verify authorization first.
//!   - Does not rate-limit or throttle. Queuing thousands of
//!     [`OutboundMessage`]s in one call is exactly what it will do if
//!     asked to — a bulk sender that can reach every registered mailbox is
//!     real reach, and shipping it without a deliberate pacing/opt-out
//!     story is how a domain's mail reputation gets burned. Flagging this
//!     loudly rather than quietly shipping a footgun.
//!
//! What it DOES give you: every broadcast is a permanent, reviewable
//! [`BankBroadcast`] record — who authorized it, how many recipients, and
//! live progress — not just "a Message sent N times" with no trace.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{BankBroadcast, BankBroadcastStatus, OutboundMessage, OutboundStatus};
use crate::store::{MailStore, StoreError};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn new_id(seed: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"bank-broadcast");
    h.update(seed.as_bytes());
    h.update(&now_ms().to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..12])
}

/// Queue a broadcast to every registered mail account. Returns the
/// [`BankBroadcast`] record (status `Completed` once every recipient has
/// been queued — "queued for outbound delivery," NOT "confirmed delivered";
/// actual delivery is the MTA's job downstream, same as any other outbound
/// message).
///
/// `sender_label` should be a real address on a domain this deployment
/// controls DKIM/SPF/DMARC for (e.g. `"bank@sigilgraph.org"`) — that's what
/// makes the label mean something to a recipient's mail client rather than
/// being purely cosmetic.
pub fn send_broadcast(
    store: &MailStore,
    sender_label: &str,
    subject: &str,
    body: &str,
    authorized_by: &str,
) -> Result<BankBroadcast, StoreError> {
    let recipients = store.all_account_addresses()?;
    let now = now_ms();
    let mut broadcast = BankBroadcast {
        id: new_id(authorized_by),
        sender_label: sender_label.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
        authorized_by: authorized_by.to_string(),
        recipient_count: recipients.len() as u32,
        sent_count: 0,
        failed_count: 0,
        status: BankBroadcastStatus::Sending,
        created_at: now,
        completed_at: None,
    };
    store.create_bank_broadcast(&broadcast)?;

    for recipient in &recipients {
        let outbound = OutboundMessage {
            id: new_id(&format!("{authorized_by}{recipient}{}", broadcast.sent_count)),
            message_id: broadcast.id.clone(),
            sender: sender_label.to_string(),
            recipient: recipient.clone(),
            subject: Some(subject.to_string()),
            body: body.to_string(),
            headers: std::collections::HashMap::new(),
            is_local: true, // recipient is always a local account here — no MX hop needed
            domain: recipient.split('@').nth(1).unwrap_or_default().to_string(),
            status: OutboundStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            created_at: now,
            next_retry: now,
            delivered_at: None,
            failed_at: None,
            last_error: None,
            updated_at: now,
            priority: -1, // lower priority than a citizen's own mail
        };
        match store.queue_outbound(&outbound) {
            Ok(()) => broadcast.sent_count += 1,
            Err(_) => broadcast.failed_count += 1,
        }
    }

    broadcast.status = BankBroadcastStatus::Completed;
    broadcast.completed_at = Some(now_ms());
    store.update_bank_broadcast(&broadcast)?;
    Ok(broadcast)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store(name: &str) -> MailStore {
        let dir = std::env::temp_dir().join(format!("sigil-mail-broadcast-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MailStore::open(dir).expect("open test store")
    }

    #[test]
    fn broadcast_reaches_every_registered_account_and_records_the_audit_trail() {
        let store = open_test_store("basic");
        store.create_account("wallet-a", "alice", "sigilgraph.org", None, 1000).unwrap();
        store.create_account("wallet-b", "bob", "sigilgraph.org", None, 1000).unwrap();
        store.create_account("wallet-c", "carol", "sigilgraph.org", None, 1000).unwrap();

        let broadcast =
            send_broadcast(&store, "bank@sigilgraph.org", "Network Update", "Something happened.", "wallet-operator")
                .expect("broadcast");

        assert_eq!(broadcast.recipient_count, 3);
        assert_eq!(broadcast.sent_count, 3);
        assert_eq!(broadcast.failed_count, 0);
        assert_eq!(broadcast.status, BankBroadcastStatus::Completed);
        assert_eq!(broadcast.authorized_by, "wallet-operator");

        // The record is permanently retrievable — the accountability trail
        // the doc comment promises, not just an in-memory return value.
        let reloaded = store.get_bank_broadcast(&broadcast.id).unwrap().expect("stored");
        assert_eq!(reloaded, broadcast);
    }

    #[test]
    fn zero_accounts_is_a_completed_no_op_not_an_error() {
        let store = open_test_store("empty");
        let broadcast =
            send_broadcast(&store, "bank@sigilgraph.org", "Hi", "Body", "wallet-operator").expect("broadcast");
        assert_eq!(broadcast.recipient_count, 0);
        assert_eq!(broadcast.status, BankBroadcastStatus::Completed);
    }
}
