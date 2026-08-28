//! Reminder sweep — a faithful port of the original's
//! `check_calendar_reminders` (called every 60s from `main.rs` there): find
//! events whose `start_time` minus one of their `reminder_minutes` entries
//! falls within the sweep window, and hand them back so the caller can
//! notify however this deployment surfaces notifications (e.g.
//! `sigil-mail::notify`, wiring these two crates together is the caller's
//! job — this crate doesn't depend on sigil-mail).

use crate::models::CalendarEvent;
use crate::store::{CalendarStore, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueReminder {
    pub event: CalendarEvent,
    /// Which entry in `event.reminder_minutes` triggered this — e.g. `15`
    /// for "15 minutes before."
    pub minutes_before: u32,
}

/// Events with a reminder that fires between `now_ms` and `now_ms +
/// window_ms` (the original's default window was 60 minutes ahead, swept
/// every 60 seconds — pass whatever cadence/window this deployment wants;
/// this function doesn't assume a specific sweep interval).
pub fn due_reminders(store: &CalendarStore, now_ms: u64, window_ms: u64) -> Result<Vec<DueReminder>, StoreError> {
    // A reminder minutes_before M fires at (start_time - M*60_000). We want
    // that fire time to land in [now_ms, now_ms + window_ms), which means
    // start_time itself must land in [now_ms, now_ms + window_ms + M*60_000)
    // for SOME M the event carries — so scan a window wide enough to catch
    // the largest configured reminder_minutes value, then filter precisely
    // per-event below. 24h covers every sane reminder lead time.
    const MAX_LEAD_MS: u64 = 24 * 60 * 60 * 1000;
    let candidates = store.events_starting_between(now_ms, now_ms.saturating_add(window_ms).saturating_add(MAX_LEAD_MS))?;

    let mut out = Vec::new();
    for event in candidates {
        let Some(minutes_list) = &event.reminder_minutes else { continue };
        for &minutes_before in minutes_list {
            let fire_at = event.start_time.saturating_sub(u64::from(minutes_before) * 60_000);
            if fire_at >= now_ms && fire_at < now_ms + window_ms {
                out.push(DueReminder { event: event.clone(), minutes_before });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CalendarEventType;

    fn open_test_store(name: &str) -> CalendarStore {
        let dir = std::env::temp_dir().join(format!("sigil-calendar-reminders-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        CalendarStore::open(dir).expect("open test store")
    }

    fn event_with_reminders(id: &str, start_time: u64, reminder_minutes: Vec<u32>) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            wallet_id: "wallet-a".to_string(),
            title: "Meeting".to_string(),
            description: None,
            event_type: CalendarEventType::Personal,
            start_time,
            end_time: None,
            all_day: false,
            recurring: None,
            color: None,
            reminder_minutes: Some(reminder_minutes),
            scheduled_tx: None,
            shared: false,
            created_at: start_time,
            updated_at: None,
            cancelled: false,
            source_peer: None,
        }
    }

    #[test]
    fn a_reminder_fires_exactly_minutes_before_start() {
        let store = open_test_store("basic");
        // Event starts at t=1_000_000ms with a 15-minute reminder ->
        // fires at 1_000_000 - 15*60_000 = 100_000ms.
        store.create_event(&event_with_reminders("e1", 1_000_000, vec![15])).unwrap();

        let due = due_reminders(&store, 99_000, 2_000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].minutes_before, 15);
        assert_eq!(due[0].event.id, "e1");
    }

    #[test]
    fn a_reminder_outside_the_sweep_window_does_not_fire() {
        let store = open_test_store("outside");
        store.create_event(&event_with_reminders("e1", 1_000_000, vec![15])).unwrap();
        // Sweep window nowhere near the 100_000ms fire time.
        let due = due_reminders(&store, 500_000, 2_000).unwrap();
        assert!(due.is_empty());
    }

    #[test]
    fn an_event_with_multiple_reminders_can_fire_more_than_once() {
        let store = open_test_store("multi");
        // start_time = 2_000_000ms. 10-min-before fires at 2_000_000 -
        // 600_000 = 1_400_000; 20-min-before fires at 2_000_000 -
        // 1_200_000 = 800_000. Two independent, non-overlapping windows.
        store.create_event(&event_with_reminders("e1", 2_000_000, vec![10, 20])).unwrap();
        let due_10 = due_reminders(&store, 1_399_000, 2_000).unwrap();
        assert_eq!(due_10.len(), 1);
        assert_eq!(due_10[0].minutes_before, 10);

        let due_20 = due_reminders(&store, 799_000, 2_000).unwrap();
        assert_eq!(due_20.len(), 1);
        assert_eq!(due_20[0].minutes_before, 20);
    }
}
