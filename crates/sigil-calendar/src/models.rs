//! sigil-calendar data model — a faithful port of the EVENT shape from
//! Quillon's `crates/q-api-server/src/calendar_api.rs` /
//! `crates/q-types/src/lib.rs` (`CalendarEvent`, `CalendarEventType`,
//! `RecurrenceRule`, `ScheduledTransaction`), onto this workspace's
//! conventions: `wallet: [u8; 32]` -> `wallet_id: String` (matching
//! `sigil-oauth::wallet_id`'s hex output, same as sigil-mail), and no
//! chrono/uuid — plain `u64` millis + content-addressed hex ids.
//!
//! **`ScheduledTransaction` is carried over as a DATA SHAPE ONLY — its
//! money-moving behavior is deliberately NOT ported.** See `scheduled.rs`'s
//! module doc for exactly why: the original executed a scheduled payment by
//! directly mutating an in-memory balance `HashMap`, bypassing the real
//! signed-transaction chokepoint and fabricating a fake tx hash. That's not
//! safe to reproduce on SIGIL's real money path. Here, a scheduled
//! transaction is a PLAN the citizen can see and get reminded about — not
//! something that autonomously moves funds.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarEventType {
    Personal,
    ScheduledTransaction,
    VestingUnlock,
    GovernanceVote,
    NetworkMilestone,
    CommunityEvent,
    PriceAlert,
}

impl Default for CalendarEventType {
    fn default() -> Self {
        CalendarEventType::Personal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecurrenceFrequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceRule {
    pub frequency: RecurrenceFrequency,
    #[serde(default = "default_interval")]
    pub interval: u32,
    #[serde(default)]
    pub until: Option<u64>,
    #[serde(default)]
    pub count: Option<u32>,
}

fn default_interval() -> u32 {
    1
}

/// The PLAN for a future payment — see the module doc for why this stops
/// at "plan," not "autonomous execution." `status` replaces the original's
/// bare `executed: bool` + separate `error: Option<String>` with a real
/// enum (the same "a stored field that's really one of a fixed set of
/// states deserves an enum" fix used in sigil-mail's `OutboundStatus`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTransaction {
    pub to_wallet: String,
    pub token: String,
    /// Kept as a string, exactly like the original — this crate has no
    /// opinion on decimal precision/token semantics, that belongs to
    /// whatever real transaction system eventually executes the plan.
    pub amount: String,
    pub status: ScheduledTxStatus,
    /// Set only once a REAL execution path exists and actually runs this
    /// plan — see `scheduled.rs`. Always `None` today.
    pub tx_hash: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTxStatus {
    Planned,
    Executed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub wallet_id: String,
    pub title: String,
    pub description: Option<String>,
    pub event_type: CalendarEventType,
    pub start_time: u64,
    pub end_time: Option<u64>,
    pub all_day: bool,
    pub recurring: Option<RecurrenceRule>,
    pub color: Option<String>,
    pub reminder_minutes: Option<Vec<u32>>,
    pub scheduled_tx: Option<ScheduledTransaction>,
    /// Shared to the community feed via gossipsub — the original's P2P
    /// sharing model. Kept as a plain flag here; the actual gossip wiring
    /// is future work (not built in this pass, same as the original's
    /// broader P2P community-events plumbing wasn't re-derived line for
    /// line — only the flag's PRESENCE in the model is ported now).
    pub shared: bool,
    pub created_at: u64,
    pub updated_at: Option<u64>,
    pub cancelled: bool,
    pub source_peer: Option<String>,
}
