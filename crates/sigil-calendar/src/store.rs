//! Storage layer — flux-db backed, mirroring sigil-mail's conventions
//! (one column family per entity, a real time-ordered index for the
//! per-wallet listing query since that's the hot path, matching
//! `sigil-mail::store`'s `CF_NOTIFICATIONS_BY_WALLET` pattern exactly).

use flux_db::Database;

use crate::models::CalendarEvent;

const CF_EVENTS: &str = "calendar_events";
/// `"{wallet_id}\0{start_time:020}\0{id}"` -> id. Same zero-padded-decimal
/// trick as sigil-mail's notification index: lexicographic byte order ==
/// chronological order, so a prefix-bounded scan is also a date-ordered
/// scan for free.
const CF_EVENTS_BY_WALLET: &str = "calendar_events_by_wallet";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Db(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("event {0:?} not found")]
    EventNotFound(String),
}

impl From<String> for StoreError {
    fn from(s: String) -> Self {
        StoreError::Db(s)
    }
}

pub struct CalendarStore {
    db: Database,
}

impl CalendarStore {
    pub fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        let db = Database::open(path.into()).map_err(StoreError::Db)?;
        for cf in [CF_EVENTS, CF_EVENTS_BY_WALLET] {
            if db.cf(cf).is_none() {
                db.create_cf(cf).map_err(StoreError::Db)?;
            }
        }
        Ok(Self { db })
    }

    fn cf(&self, name: &str) -> Database {
        self.db
            .cf(name)
            .unwrap_or_else(|| panic!("column family {name:?} missing — open() should have created it"))
    }

    fn index_key(wallet_id: &str, start_time: u64, id: &str) -> Vec<u8> {
        let mut k = wallet_id.as_bytes().to_vec();
        k.push(0);
        k.extend_from_slice(format!("{start_time:020}").as_bytes());
        k.push(0);
        k.extend_from_slice(id.as_bytes());
        k
    }

    pub fn create_event(&self, event: &CalendarEvent) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(event)?;
        self.cf(CF_EVENTS).put(event.id.as_bytes(), &bytes)?;
        let key = Self::index_key(&event.wallet_id, event.start_time, &event.id);
        self.cf(CF_EVENTS_BY_WALLET).put(&key, event.id.as_bytes())?;
        Ok(())
    }

    pub fn get_event(&self, id: &str) -> Result<Option<CalendarEvent>, StoreError> {
        match self.cf(CF_EVENTS).get(id.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Overwrites the stored event AND its index entry — callers that
    /// change `start_time` must go through this (not a raw CF put) so the
    /// time-ordered index stays consistent; see `update_event`.
    fn put_event_and_reindex(
        &self,
        event: &CalendarEvent,
        old_start_time: u64,
    ) -> Result<(), StoreError> {
        if old_start_time != event.start_time {
            let old_key = Self::index_key(&event.wallet_id, old_start_time, &event.id);
            self.cf(CF_EVENTS_BY_WALLET).delete(&old_key)?;
        }
        self.create_event(event)
    }

    pub fn update_event(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut CalendarEvent),
    ) -> Result<CalendarEvent, StoreError> {
        let mut event = self.get_event(id)?.ok_or_else(|| StoreError::EventNotFound(id.to_string()))?;
        let old_start_time = event.start_time;
        mutate(&mut event);
        self.put_event_and_reindex(&event, old_start_time)?;
        Ok(event)
    }

    pub fn delete_event(&self, id: &str) -> Result<(), StoreError> {
        if let Some(event) = self.get_event(id)? {
            let key = Self::index_key(&event.wallet_id, event.start_time, &event.id);
            self.cf(CF_EVENTS_BY_WALLET).delete(&key)?;
            self.cf(CF_EVENTS).delete(id.as_bytes())?;
        }
        Ok(())
    }

    /// All of a wallet's events, ordered by `start_time` ascending, within
    /// `[from_ms, to_ms)` (pass `0`/`u64::MAX` for an open-ended bound).
    pub fn list_events(&self, wallet_id: &str, from_ms: u64, to_ms: u64) -> Result<Vec<CalendarEvent>, StoreError> {
        let prefix = {
            let mut p = wallet_id.as_bytes().to_vec();
            p.push(0);
            p
        };
        let mut ids = Vec::new();
        for (key, id_bytes) in self.cf(CF_EVENTS_BY_WALLET).iter_from(&prefix) {
            if !key.starts_with(&prefix) {
                break;
            }
            ids.push(String::from_utf8_lossy(&id_bytes).into_owned());
        }

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(bytes) = self.cf(CF_EVENTS).get(id.as_bytes())? {
                let event: CalendarEvent = serde_json::from_slice(&bytes)?;
                if event.start_time >= from_ms && event.start_time < to_ms {
                    out.push(event);
                }
            }
        }
        Ok(out)
    }

    /// Every non-cancelled event across every wallet whose `start_time`
    /// falls within `[from_ms, to_ms)` — what a reminder sweep polls
    /// (mirrors the original's `check_calendar_reminders`, called
    /// periodically by whatever process embeds this crate).
    pub fn events_starting_between(&self, from_ms: u64, to_ms: u64) -> Result<Vec<CalendarEvent>, StoreError> {
        let mut out = Vec::new();
        for (_, bytes) in self.cf(CF_EVENTS).iter_from(&[]) {
            let event: CalendarEvent = serde_json::from_slice(&bytes)?;
            if !event.cancelled && event.start_time >= from_ms && event.start_time < to_ms {
                out.push(event);
            }
        }
        out.sort_by_key(|e| e.start_time);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CalendarEventType;

    fn open_test_store(name: &str) -> CalendarStore {
        let dir = std::env::temp_dir().join(format!("sigil-calendar-store-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        CalendarStore::open(dir).expect("open test store")
    }

    fn event(id: &str, wallet_id: &str, start_time: u64) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            wallet_id: wallet_id.to_string(),
            title: "Test event".to_string(),
            description: None,
            event_type: CalendarEventType::Personal,
            start_time,
            end_time: None,
            all_day: false,
            recurring: None,
            color: None,
            reminder_minutes: None,
            scheduled_tx: None,
            shared: false,
            created_at: start_time,
            updated_at: None,
            cancelled: false,
            source_peer: None,
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let store = open_test_store("roundtrip");
        let e = event("e1", "wallet-a", 1000);
        store.create_event(&e).unwrap();
        assert_eq!(store.get_event("e1").unwrap(), Some(e));
    }

    #[test]
    fn list_events_is_time_ordered_and_scoped_per_wallet() {
        let store = open_test_store("listing");
        store.create_event(&event("e2", "wallet-a", 2000)).unwrap();
        store.create_event(&event("e1", "wallet-a", 1000)).unwrap();
        store.create_event(&event("e3", "wallet-b", 1500)).unwrap();

        let a_events = store.list_events("wallet-a", 0, u64::MAX).unwrap();
        assert_eq!(a_events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["e1", "e2"]);

        let b_events = store.list_events("wallet-b", 0, u64::MAX).unwrap();
        assert_eq!(b_events.len(), 1);
    }

    #[test]
    fn list_events_respects_the_time_window() {
        let store = open_test_store("window");
        store.create_event(&event("e1", "wallet-a", 1000)).unwrap();
        store.create_event(&event("e2", "wallet-a", 5000)).unwrap();
        let windowed = store.list_events("wallet-a", 2000, 6000).unwrap();
        assert_eq!(windowed.len(), 1);
        assert_eq!(windowed[0].id, "e2");
    }

    #[test]
    fn update_event_that_changes_start_time_keeps_the_index_consistent() {
        let store = open_test_store("reindex");
        store.create_event(&event("e1", "wallet-a", 1000)).unwrap();

        store.update_event("e1", |e| e.start_time = 9000).unwrap();

        // The OLD time-slot must no longer list it...
        assert!(store.list_events("wallet-a", 900, 1100).unwrap().is_empty());
        // ...and the NEW one must.
        let at_new_time = store.list_events("wallet-a", 8900, 9100).unwrap();
        assert_eq!(at_new_time.len(), 1);
        assert_eq!(at_new_time[0].start_time, 9000);
    }

    #[test]
    fn delete_event_removes_it_from_storage_and_the_index() {
        let store = open_test_store("delete");
        store.create_event(&event("e1", "wallet-a", 1000)).unwrap();
        store.delete_event("e1").unwrap();
        assert_eq!(store.get_event("e1").unwrap(), None);
        assert!(store.list_events("wallet-a", 0, u64::MAX).unwrap().is_empty());
    }

    #[test]
    fn events_starting_between_spans_wallets_and_skips_cancelled() {
        let store = open_test_store("reminders");
        store.create_event(&event("e1", "wallet-a", 1000)).unwrap();
        store.create_event(&event("e2", "wallet-b", 1500)).unwrap();
        store.update_event("e2", |e| e.cancelled = true).unwrap();
        store.create_event(&event("e3", "wallet-a", 1800)).unwrap();

        let due = store.events_starting_between(0, 2000).unwrap();
        assert_eq!(due.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["e1", "e3"]);
    }
}
