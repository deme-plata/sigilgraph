//! Notifications — the non-email feed Viktor asked to see in the same tab
//! as the inbox (wallet/system events: payment received, mining reward,
//! sync status). New in this port, not in the original.
//!
//! Deliberately a thin, standalone layer: any other SIGIL subsystem (the
//! wallet, the producer, the sync client) can call [`notify`] directly to
//! surface an event to a citizen without needing to know anything about
//! mail — it's the one function this whole module exists to expose.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{Notification, NotificationKind};
use crate::store::{MailStore, StoreError};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn new_id(wallet_id: &str, kind: NotificationKind) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"notification");
    h.update(wallet_id.as_bytes());
    h.update(format!("{kind:?}").as_bytes());
    h.update(&now_ms().to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..12])
}

/// Create and store a notification for `wallet_id`. `data` is an optional
/// structured payload (a tx hash, an amount, …) the UI can render richly
/// instead of just the plain `message` string — pass `None` for a
/// message-only notification.
pub fn notify(
    store: &MailStore,
    wallet_id: &str,
    kind: NotificationKind,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> Result<Notification, StoreError> {
    let notification = Notification {
        id: new_id(wallet_id, kind),
        wallet_id: wallet_id.to_string(),
        kind,
        message: message.into(),
        data,
        read: false,
        created_at: now_ms(),
    };
    store.create_notification(&notification)?;
    Ok(notification)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store(name: &str) -> MailStore {
        let dir = std::env::temp_dir().join(format!("sigil-mail-notify-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MailStore::open(dir).expect("open test store")
    }

    #[test]
    fn notify_then_list_returns_it_unread_newest_first() {
        let store = open_test_store("basic");
        notify(&store, "wallet-a", NotificationKind::MiningReward, "You mined a block!", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        notify(&store, "wallet-a", NotificationKind::PaymentReceived, "10 SIGIL received", None).unwrap();

        let list = store.list_notifications("wallet-a", 10).unwrap();
        assert_eq!(list.len(), 2);
        // Newest first.
        assert_eq!(list[0].message, "10 SIGIL received");
        assert_eq!(list[1].message, "You mined a block!");
        assert!(!list[0].read && !list[1].read);
    }

    #[test]
    fn mark_read_flips_the_flag() {
        let store = open_test_store("mark-read");
        let n = notify(&store, "wallet-a", NotificationKind::System, "hi", None).unwrap();
        store.mark_notification_read(&n.id).unwrap();
        let list = store.list_notifications("wallet-a", 10).unwrap();
        assert!(list[0].read);
    }

    #[test]
    fn notifications_are_scoped_per_wallet() {
        let store = open_test_store("scoping");
        notify(&store, "wallet-a", NotificationKind::System, "for a", None).unwrap();
        notify(&store, "wallet-b", NotificationKind::System, "for b", None).unwrap();
        assert_eq!(store.list_notifications("wallet-a", 10).unwrap().len(), 1);
        assert_eq!(store.list_notifications("wallet-b", 10).unwrap().len(), 1);
    }
}
