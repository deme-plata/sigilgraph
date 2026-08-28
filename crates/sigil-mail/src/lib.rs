//! sigil-mail — SIGIL's self-hosted mail service.
//!
//! See the crate-level doc in `Cargo.toml`'s `description` for the
//! port-vs-rebuild framing. Status as of 2026-08-23 (first pass): the data
//! model and the account/alias storage + routing layer are real and tested
//! (`store.rs`). The SMTP/IMAP protocol servers, the outbound delivery
//! queue (MTA), notifications, and the bank broadcast sender are NOT yet
//! built — this crate is the foundation, not the finished service. Don't
//! read its existence as "email works now."

pub mod auth;
pub mod bank_broadcast;
pub mod models;
pub mod mta;
pub mod notify;
pub mod protocol;
pub mod smtp_server;
pub mod store;

pub use auth::{authenticate, wallet_id_of, AuthError, MAIL_SCOPE};
pub use bank_broadcast::send_broadcast;
pub use models::*;
pub use notify::notify;
pub use smtp_server::{MailServerState, SmtpServer, SmtpServerError};
pub use store::{MailStore, RouteResult, StoreError};
