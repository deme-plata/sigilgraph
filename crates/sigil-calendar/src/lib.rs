//! sigil-calendar — SIGIL's citizen calendar.
//!
//! See `Cargo.toml`'s `description` for the port-vs-rebuild framing this
//! crate follows (same discipline as `sigil-mail`). Status as of
//! 2026-08-24 (first pass): the event model, per-wallet storage +
//! time-ordered listing, reminder sweep, scheduled-payment PLANNING
//! (deliberately not execution — see `scheduled.rs`), and wallet-token
//! auth (identical pattern to `sigil-mail::auth`) are real and tested.
//! NOT built yet: an HTTP API surface (this is a library, nothing serves
//! it over the network yet), P2P sharing of community events (the
//! `shared`/`source_peer` fields exist in the model but nothing
//! publishes/subscribes to gossipsub yet), and a real execution engine for
//! scheduled payments (deliberately absent, money-safety reasons
//! documented in `scheduled.rs`).

pub mod auth;
pub mod models;
pub mod reminders;
pub mod scheduled;
pub mod store;

pub use auth::{authenticate, wallet_id_of, AuthError, CALENDAR_SCOPE};
pub use models::*;
pub use reminders::{due_reminders, DueReminder};
pub use scheduled::{cancel as cancel_scheduled_payment, create_planned_payment};
pub use store::{CalendarStore, StoreError};
