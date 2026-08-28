//! The SMTP server — accepts connections, drives the command loop, and
//! delivers/queues mail. A FAITHFUL PORT of the connection-handling shape
//! in
//! `beta-migration/fs/opt/orobit/shared/axum-mail-server/backend/src/servers/smtp.rs`,
//! with the identity boundary rebuilt:
//!
//!   - `authenticate_user` (email + bcrypt password lookup) is replaced by
//!     [`authenticate_smtp`]: `AUTH PLAIN`'s password slot is treated as a
//!     SIGIL OAuth bearer token (the same "OAuth as the SMTP password"
//!     pattern real providers like Gmail use for XOAUTH2) and verified via
//!     [`crate::auth::authenticate`] — no password ever touches this
//!     server.
//!   - `is_local_domain` + `user_exists` (a naive domain-suffix check plus
//!     a direct-address-only lookup) are replaced by
//!     [`crate::store::MailStore::route_address`], which — unlike the
//!     original — correctly resolves ALIASES too, not just primary
//!     addresses. This is a real improvement the port picked up for free:
//!     the original's `deliver_local_message` looked recipients up by
//!     `get_user_by_email` directly, which would have silently mis-filed
//!     mail sent to an alias address.
//!
//! NOT yet built: TLS (STARTTLS replies "not implemented", same as the
//! original left it), the actual outbound MTA delivery loop (this server
//! only enqueues via `MailStore::queue_outbound` — something else has to
//! drain that queue), and IMAP (so mail can be read back). Don't read this
//! file's existence as "email is live."

use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine;
use sigil_oauth::AnchorResolver;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};

use crate::auth;
use crate::models::{OutboundMessage, OutboundStatus};
use crate::protocol::{ParsedMessage, SmtpCommand, SmtpCommandParser, SmtpResponse, SmtpSession};
use crate::store::{MailStore, RouteResult};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_session_id() -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"smtp-session");
    h.update(&now_ms().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..12])
}

/// The shared state every accepted connection needs — bundles the storage
/// layer and however this deployment resolves OAuth issuer anchors (a real
/// DoH resolver in production, [`sigil_oauth::StaticResolver`] in tests).
///
/// Generic over `R`, not a `dyn AnchorResolver` trait object: `AnchorResolver`
/// is used by `sigil_oauth::verify_token_via_dns<R: AnchorResolver>`, a
/// generic function with an implicit `R: Sized` bound — a trait object
/// (`dyn AnchorResolver`, unsized) can't instantiate it. Staying generic all
/// the way through keeps `R` concrete, so `state.resolver` can be handed
/// straight to `auth::authenticate` with no erasure in between.
pub struct MailServerState<R: AnchorResolver + Send + Sync + 'static> {
    pub store: Arc<MailStore>,
    pub resolver: R,
    /// Domains this server maxes out its `RCPT TO` relay-denied check
    /// against for messages NOT addressed to a local account — informational
    /// only right now (used in error text); actual outbound relay is the
    /// not-yet-built MTA's job.
    pub primary_domain: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SmtpServerError {
    #[error("bind failed: {0}")]
    Bind(std::io::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct SmtpServer<R: AnchorResolver + Send + Sync + 'static> {
    state: Arc<MailServerState<R>>,
    listener: TcpListener,
}

impl<R: AnchorResolver + Send + Sync + 'static> SmtpServer<R> {
    pub async fn bind(state: Arc<MailServerState<R>>, addr: &str) -> Result<Self, SmtpServerError> {
        let listener = TcpListener::bind(addr).await.map_err(SmtpServerError::Bind)?;
        tracing::info!("📧 [sigil-mail] SMTP server bound to {addr}");
        Ok(Self { state, listener })
    }

    /// Accept connections until `shutdown` resolves. One task per
    /// connection — a slow or hostile client only ever blocks its own task.
    pub async fn run(&self, mut shutdown: tokio::sync::oneshot::Receiver<()>) -> Result<(), SmtpServerError> {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, addr)) => {
                            let state = self.state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(state, stream, addr).await {
                                    tracing::warn!("SMTP connection from {addr} ended with error: {e}");
                                }
                            });
                        }
                        Err(e) => tracing::warn!("SMTP accept() failed: {e}"),
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("📧 [sigil-mail] SMTP server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_connection<R: AnchorResolver + Send + Sync + 'static>(
    state: Arc<MailServerState<R>>,
    stream: TcpStream,
    client_addr: SocketAddr,
) -> Result<(), SmtpServerError> {
    let mut session = SmtpSession::new(client_addr, new_session_id(), now_ms());
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    write_half.write_all(SmtpResponse::service_ready().format().as_bytes()).await?;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // client disconnected
        }

        let command = SmtpCommandParser::parse(&line);
        let response = process_command(&state, &mut session, command).await;
        write_half.write_all(response.format().as_bytes()).await?;

        match &response {
            SmtpResponse::Ok { code: 221, .. } => break, // QUIT
            SmtpResponse::Continue { code: 354, .. } => {
                let raw = read_message_data(&mut reader).await?;
                session.data = Some(raw);
                let outcome = accept_message(&state, &session).await;
                let reply = match outcome {
                    Ok(()) => SmtpResponse::ok_with_message("Message accepted"),
                    Err(e) => {
                        tracing::warn!("message accept failed for session {}: {e}", session.id);
                        SmtpResponse::TempError { code: 451, message: "local error in processing".to_string() }
                    }
                };
                write_half.write_all(reply.format().as_bytes()).await?;
                session.reset_transaction();
            }
            _ => {}
        }
    }

    Ok(())
}

async fn process_command<R: AnchorResolver + Send + Sync + 'static>(
    state: &Arc<MailServerState<R>>,
    session: &mut SmtpSession,
    command: SmtpCommand,
) -> SmtpResponse {
    match command {
        SmtpCommand::Helo { domain } => {
            session.helo_domain = Some(domain.clone());
            SmtpResponse::ok_with_message(&format!("Hello {domain}"))
        }
        SmtpCommand::Ehlo { domain } => {
            session.helo_domain = Some(domain.clone());
            SmtpResponse::Ok {
                code: 250,
                message: format!("Hello {domain}\nAUTH PLAIN\nSIZE 26214400"),
            }
        }
        SmtpCommand::StartTls => {
            // TODO: real TLS termination — same gap the original left open.
            SmtpResponse::not_implemented()
        }
        SmtpCommand::Auth { mechanism, data } => authenticate_smtp(state, session, &mechanism, data).await,
        SmtpCommand::MailFrom { address, .. } => {
            // Only the submission port (587) requires auth to originate mail;
            // port 25 accepts unauthenticated MAIL FROM for inbound-from-the-
            // internet delivery, same split the original made.
            if session.client_addr.port() == 587 && !session.is_authenticated() {
                return SmtpResponse::auth_required();
            }
            if !is_plausible_address(&address) {
                return SmtpResponse::parameter_error();
            }
            session.mail_from = Some(address.clone());
            SmtpResponse::ok_with_message(&format!("Sender {address} OK"))
        }
        SmtpCommand::RcptTo { address, .. } => {
            if session.mail_from.is_none() {
                return SmtpResponse::bad_sequence();
            }
            if !is_plausible_address(&address) {
                return SmtpResponse::parameter_error();
            }
            match state.store.route_address(&address) {
                Ok(RouteResult::NotLocal) => {
                    if session.is_authenticated() {
                        session.rcpt_to.push(address.clone());
                        SmtpResponse::ok_with_message(&format!("Recipient {address} OK (relay)"))
                    } else {
                        SmtpResponse::relay_denied()
                    }
                }
                Ok(_) => {
                    session.rcpt_to.push(address.clone());
                    SmtpResponse::ok_with_message(&format!("Recipient {address} OK"))
                }
                Err(e) => {
                    tracing::warn!("route_address failed for {address}: {e}");
                    SmtpResponse::TempError { code: 451, message: "local error in processing".to_string() }
                }
            }
        }
        SmtpCommand::Data => {
            if session.can_receive_mail() {
                SmtpResponse::start_mail_input()
            } else {
                SmtpResponse::bad_sequence()
            }
        }
        SmtpCommand::Rset => {
            session.reset_transaction();
            SmtpResponse::ok()
        }
        SmtpCommand::Noop => SmtpResponse::ok(),
        SmtpCommand::Quit => SmtpResponse::closing_connection(),
        SmtpCommand::Unknown { command, .. } => {
            tracing::debug!("unrecognized SMTP command: {command:?}");
            SmtpResponse::syntax_error()
        }
    }
}

/// `AUTH PLAIN <base64(\0 identity-hint \0 bearer-token)>` — the identity
/// hint is logged for diagnostics only; the bearer token is what actually
/// gets verified. No password ever exists to check.
async fn authenticate_smtp<R: AnchorResolver + Send + Sync + 'static>(
    state: &Arc<MailServerState<R>>,
    session: &mut SmtpSession,
    mechanism: &str,
    data: Option<String>,
) -> SmtpResponse {
    if !mechanism.eq_ignore_ascii_case("PLAIN") {
        return SmtpResponse::not_implemented();
    }
    let Some(encoded) = data else {
        // A real PLAIN exchange has a two-step form (334 continuation, then
        // the client sends the blob on the next line) — not implemented
        // here; every real client we care about sends it inline as
        // `AUTH PLAIN <blob>` in one command, so this just asks for it.
        return SmtpResponse::Continue { code: 334, message: String::new() };
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return SmtpResponse::parameter_error();
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return SmtpResponse::parameter_error();
    };
    let parts: Vec<&str> = text.split('\0').collect();
    if parts.len() < 3 {
        return SmtpResponse::parameter_error();
    }
    let bearer_token = parts[2];

    match auth::authenticate(&state.resolver, bearer_token, now_ms()) {
        Ok(claims) => {
            session.authenticated_wallet = Some(auth::wallet_id_of(&claims).to_string());
            SmtpResponse::Ok { code: 235, message: "Authentication successful".to_string() }
        }
        Err(e) => {
            tracing::debug!("SMTP AUTH failed: {e}");
            SmtpResponse::PermError { code: 535, message: "Authentication failed".to_string() }
        }
    }
}

async fn read_message_data(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<String, std::io::Error> {
    let mut out = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        if line.trim() == "." {
            break;
        }
        if let Some(stripped) = line.strip_prefix('.') {
            if stripped.starts_with('.') {
                out.push_str(stripped); // dot-stuffing unstuff
                continue;
            }
        }
        out.push_str(&line);
    }
    Ok(out)
}

/// Route the just-received message to every recipient: local accounts get
/// it filed straight into their INBOX, everything else gets queued for the
/// (not-yet-built) outbound MTA to deliver over real SMTP.
async fn accept_message<R: AnchorResolver + Send + Sync + 'static>(
    state: &Arc<MailServerState<R>>,
    session: &SmtpSession,
) -> Result<(), crate::store::StoreError> {
    let raw = session.data.as_deref().unwrap_or_default();
    let from = session.mail_from.as_deref().unwrap_or_default();
    let parsed = ParsedMessage::new(raw.to_string());

    for recipient in &session.rcpt_to {
        match state.store.route_address(recipient)? {
            RouteResult::Account(account) | RouteResult::Alias { account, .. } => {
                deliver_local(&state.store, from, recipient, &account.wallet_id, &parsed)?;
            }
            RouteResult::NotLocal => {
                queue_outbound(&state.store, from, recipient, &parsed)?;
            }
        }
    }
    Ok(())
}

fn deliver_local(
    store: &MailStore,
    from: &str,
    to: &str,
    wallet_id: &str,
    message: &ParsedMessage,
) -> Result<(), crate::store::StoreError> {
    let now = now_ms();
    let inbox = store.get_or_create_inbox(wallet_id, now)?;
    let id = content_id("message", &format!("{from}{to}{}", message.body));

    let stored = crate::models::Message {
        id: id.clone(),
        mailbox_id: inbox.id,
        subject: message.headers.get_first_value("subject").or_else(|| Some("No Subject".to_string())),
        sender: from.to_string(),
        recipient: to.to_string(),
        body: Some(message.body.clone()),
        message_id: message.headers.get_first_value("message-id").or_else(|| Some(format!("<{id}@sigilgraph.org>"))),
        in_reply_to: message.headers.get_first_value("in-reply-to"),
        thread_id: None,
        seen: false,
        recent: true,
        flagged: false,
        deleted: false,
        draft: false,
        custom_flags: vec![],
        size_bytes: message.body.len() as u32,
        created_at: now,
        updated_at: now,
    };
    store.create_message(&stored)?;
    tracing::info!("📬 [sigil-mail] delivered to {to}");
    Ok(())
}

fn queue_outbound(
    store: &MailStore,
    from: &str,
    to: &str,
    message: &ParsedMessage,
) -> Result<(), crate::store::StoreError> {
    let now = now_ms();
    let domain = to.split_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
    let id = content_id("outbound", &format!("{from}{to}{now}"));
    let out = OutboundMessage {
        id: id.clone(),
        message_id: id,
        sender: from.to_string(),
        recipient: to.to_string(),
        subject: message.headers.get_first_value("subject"),
        body: message.body.clone(),
        headers: message.headers.headers.clone(),
        is_local: false,
        domain,
        status: OutboundStatus::Pending,
        retry_count: 0,
        max_retries: 5,
        created_at: now,
        next_retry: now,
        delivered_at: None,
        failed_at: None,
        last_error: None,
        updated_at: now,
        priority: 0,
    };
    store.queue_outbound(&out)?;
    tracing::info!("📤 [sigil-mail] queued outbound from {from} to {to}");
    Ok(())
}

fn content_id(kind: &str, seed: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(kind.as_bytes());
    h.update(seed.as_bytes());
    h.update(&now_ms().to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..12])
}

/// A deliberately loose check (not full RFC 5321 grammar) — just enough to
/// reject obvious garbage before it reaches routing. Matches the strictness
/// level the original's `validator::is_valid_email` had.
fn is_plausible_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

