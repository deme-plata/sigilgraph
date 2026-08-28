//! sigil-mail data model.
//!
//! Faithfully mirrors the shape of the original axum-mail-server models
//! (`beta-migration/fs/opt/orobit/shared/axum-mail-server/backend/src/storage/models.rs`)
//! wherever the original was identity-agnostic protocol/storage plumbing —
//! `Mailbox`, `Message`, `OutboundMessage`, `DeliveryAttempt`, `EmailAlias`,
//! `CustomDomain`, `SpamRule`, `RateLimit`, `DkimKey` all carry over with the
//! same fields, same purpose.
//!
//! REBUILT, not ported, at the identity boundary (the agreed port-vs-rebuild
//! split):
//!   - The original `User` had `password_hash` — SIGIL wallets don't have
//!     passwords. Replaced by `MailAccount`, keyed by `wallet_id` (the same
//!     hex identity `sigil_oauth::wallet_id()` produces), with login handled
//!     by a real wallet-signature challenge (see `auth.rs`), not a password.
//!   - The original `Subscription`/`UsageLog` were Stripe billing tiers —
//!     dropped entirely, SIGIL has no paid tiers today. `RateLimit` alone
//!     covers the "don't let one account hammer the server" need.
//!
//! NEW, not in the original at all (Viktor's 2026-08-23 asks):
//!   - `Notification` — surfaced in the same UI tab as email, but distinct
//!     from real `Message`s (wallet/system events, not mail).
//!   - `BankBroadcast` — the SIGIL Bank's labeled, rate-limited mass-mail
//!     capability. Deliberately its own record type (not just "a Message
//!     from a a normal account") so it can carry real accountability: who
//!     authorized it, how many recipients, and a permanent audit trail —
//!     a bulk sender that can silently blast every registered mailbox is
//!     exactly the kind of thing that needs its own logged, reviewable path
//!     rather than reusing the same code an individual send uses.
//!
//! Timestamps are `u64` milliseconds-since-epoch (matching
//! `sigil-header`'s `timestamp_ms` convention) rather than `chrono::DateTime`
//! — one less dependency, consistent with the rest of this workspace.

use serde::{Deserialize, Serialize};

/// A mailbox-owning identity. Keyed by the SIGIL wallet id
/// (`sigil_oauth::wallet_id(&pubkey)`, a hex string) — there is no
/// password; login is a signed wallet-assertion challenge (see `auth.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAccount {
    /// Hex wallet id — the primary key. Same value `sigil_oauth::wallet_id`
    /// produces for this account's Ed25519 pubkey.
    pub wallet_id: String,
    /// The chosen local-part for this account's PRIMARY address, e.g.
    /// `"viktor"` for `viktor@sigilgraph.org`. Validated unique at creation
    /// (see `store.rs::create_account`). Additional custom names are
    /// `EmailAlias` records pointing back at this account, not additional
    /// `MailAccount`s — one wallet, one account, any number of names.
    pub local_part: String,
    /// The domain this account's primary address lives on (almost always
    /// `sigilgraph.org` today; `CustomDomain` exists for future expansion).
    pub domain: String,
    /// Optional human display name shown in mail clients.
    pub display_name: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl MailAccount {
    /// The account's primary address, e.g. `viktor@sigilgraph.org`.
    pub fn primary_address(&self) -> String {
        format!("{}@{}", self.local_part, self.domain)
    }
}

/// An IMAP-style mailbox folder (INBOX, Sent, etc.) belonging to an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub wallet_id: String,
    pub name: String,
    pub uid_validity: i64,
    pub uid_next: i64,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A stored message (inbound or already-sent, once filed into a mailbox).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub mailbox_id: String,
    pub subject: Option<String>,
    pub sender: String,
    pub recipient: String,
    pub body: Option<String>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub thread_id: Option<String>,
    pub seen: bool,
    pub recent: bool,
    pub flagged: bool,
    pub deleted: bool,
    pub draft: bool,
    pub custom_flags: Vec<String>,
    pub size_bytes: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A queued outbound message awaiting SMTP delivery (the MTA's work queue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: String,
    pub message_id: String,
    pub sender: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub body: String,
    pub headers: std::collections::HashMap<String, String>,
    pub is_local: bool,
    pub domain: String,
    pub status: OutboundStatus,
    pub retry_count: u32,
    pub max_retries: u32,
    pub created_at: u64,
    pub next_retry: u64,
    pub delivered_at: Option<u64>,
    pub failed_at: Option<u64>,
    pub last_error: Option<String>,
    pub updated_at: u64,
    pub priority: i32,
}

/// Mirrors the original's free-text `status: String` as a real enum — a
/// stored string that's meant to only ever be one of a fixed set of values
/// is exactly what an enum is for, and it removes a whole class of "typo'd
/// the status string" bugs the original was exposed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundStatus {
    Pending,
    Delivered,
    Failed,
    Retrying,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryAttempt {
    pub id: String,
    pub message_id: String,
    pub recipient: String,
    pub mx_host: String,
    pub attempt_time: u64,
    pub success: bool,
    pub error_message: Option<String>,
    pub smtp_response: Option<String>,
    pub delivery_time_ms: Option<u32>,
}

/// A domain this account has registered mail for (the multi-domain
/// capability the original was built with — kept, since it costs nothing to
/// keep and directly enables "add more domains later" without a redesign).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomDomain {
    pub id: String,
    pub wallet_id: String,
    pub domain: String,
    pub verified: bool,
    pub txt_record_name: Option<String>,
    pub txt_record_value: Option<String>,
    pub mx_record_configured: bool,
    pub spf_record_configured: bool,
    pub dkim_record_configured: bool,
    pub dmarc_record_configured: bool,
    pub active: bool,
    pub created_at: u64,
    pub verified_at: Option<u64>,
}

/// A custom name an account can also receive mail as — Viktor's "alias"
/// ask: e.g. `wallet abc123...` owns `viktor@sigilgraph.org` (its
/// `MailAccount.local_part`) AND registers `hello@sigilgraph.org` as an
/// alias routing to the same mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAlias {
    pub id: String,
    pub wallet_id: String,
    /// The alias address itself, e.g. `"hello@sigilgraph.org"`.
    pub alias: String,
    /// Where mail to the alias actually lands, e.g. `"viktor@sigilgraph.org"`.
    pub destination: String,
    pub active: bool,
    pub created_at: u64,
}

// No `Eq` here — `score: f64` doesn't implement it (floats aren't totally
// ordered, NaN != NaN), only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpamRule {
    pub id: String,
    pub name: String,
    pub rule_type: String,
    pub pattern: String,
    pub score: f64,
    pub description: Option<String>,
    pub active: bool,
    pub created_at: u64,
    pub created_by: Option<String>,
}

/// Generic abuse/quota guard — covers what `Subscription`'s per-tier limits
/// used to (minus the billing), e.g. "max N sends per hour per wallet."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub id: String,
    pub wallet_id: Option<String>,
    pub ip_address: Option<String>,
    pub rate_type: String,
    pub current_count: u32,
    pub limit_value: u32,
    pub window_start: u64,
    pub window_duration_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkimKey {
    pub id: String,
    pub domain: String,
    pub selector: String,
    /// PEM-encoded PKCS#1 private key. Same format `openssl genrsa` /
    /// `openssl rsa -pubout` produce — matches the keypair already generated
    /// for sigilgraph.org at `/root/.config/sigil/dkim/sigilgraph.org/`.
    pub private_key_pem: String,
    pub public_key_pem: String,
    pub active: bool,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailConfig {
    pub key: String,
    pub value: String,
    pub description: Option<String>,
    pub updated_at: u64,
    pub updated_by: Option<String>,
}

/// NEW (not in the original): a non-email notification surfaced in the same
/// UI tab as the inbox — wallet/system events (payment received, mining
/// reward, sync status, etc.), kept out of `Message`/real SMTP entirely so
/// they never get delivered, bounced, or leak into anyone else's inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub wallet_id: String,
    pub kind: NotificationKind,
    pub message: String,
    /// Optional structured payload (e.g. a tx hash, an amount) for the UI
    /// to render richly instead of just the plain `message` string.
    pub data: Option<serde_json::Value>,
    pub read: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    PaymentReceived,
    PaymentSent,
    MiningReward,
    SyncStatus,
    System,
}

/// NEW (not in the original): the SIGIL Bank's labeled mass-mail record.
/// Deliberately a first-class, permanently-logged record — not just "a
/// Message sent N times" — because a sender with reach to every registered
/// mailbox needs its own accountable, reviewable, rate-limited path rather
/// than reusing an individual account's send code. `authorized_by` records
/// which bank-operator identity approved it; `status` tracks the send as a
/// long-running job (this can be thousands of recipients), not a single
/// fire-and-forget call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankBroadcast {
    pub id: String,
    /// The bank's labeled sender address, e.g. `"bank@sigilgraph.org"` —
    /// distinct from any individual wallet's mail address so recipients can
    /// tell at a glance this came from the bank, not an impersonator (the
    /// domain's DKIM/SPF/DMARC records are what make that label trustworthy
    /// rather than just cosmetic).
    pub sender_label: String,
    pub subject: String,
    pub body: String,
    /// The wallet id of whoever authorized this broadcast — mass mail sent
    /// under the bank's name must always be attributable to a person.
    pub authorized_by: String,
    pub recipient_count: u32,
    pub sent_count: u32,
    pub failed_count: u32,
    pub status: BankBroadcastStatus,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BankBroadcastStatus {
    Queued,
    Sending,
    Completed,
    Failed,
}
