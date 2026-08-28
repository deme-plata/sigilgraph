//! SMTP wire protocol — session state, command parsing, response
//! formatting, and raw-message header/body splitting.
//!
//! This is a FAITHFUL PORT (the "reuse the proven parts" half of the port-
//! vs-rebuild split) of
//! `beta-migration/fs/opt/orobit/shared/axum-mail-server/backend/src/protocols/{smtp_protocol,mod}.rs`.
//! It carries over the exact command grammar, response codes, and MIME
//! header-folding logic the original had — protocol-level RFC 5321
//! compliance code is exactly the kind of thing worth reusing once proven,
//! not rewriting.
//!
//! The only real changes from the original: `chrono::DateTime<Utc>` ->
//! `u64` millis and `uuid::Uuid` -> `String` (matching this crate's
//! conventions elsewhere), and `authenticated_user: Option<Uuid>` ->
//! `Option<String>` (a SIGIL wallet id, once `auth::authenticate` accepts
//! it — see `smtp_server.rs`). The original's own unit tests for the
//! command parser are carried over unchanged, since the parsing logic
//! itself is untouched.

use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct SmtpSession {
    pub id: String,
    pub client_addr: SocketAddr,
    pub helo_domain: Option<String>,
    /// The SIGIL wallet id this session authenticated as, once `AUTH`
    /// succeeds (see `smtp_server.rs::handle_smtp_auth`).
    pub authenticated_wallet: Option<String>,
    pub mail_from: Option<String>,
    pub rcpt_to: Vec<String>,
    pub data: Option<String>,
    pub started_at_ms: u64,
    pub tls_enabled: bool,
}

impl SmtpSession {
    pub fn new(client_addr: SocketAddr, id: String, now_ms: u64) -> Self {
        Self {
            id,
            client_addr,
            helo_domain: None,
            authenticated_wallet: None,
            mail_from: None,
            rcpt_to: Vec::new(),
            data: None,
            started_at_ms: now_ms,
            tls_enabled: false,
        }
    }

    pub fn reset_transaction(&mut self) {
        self.mail_from = None;
        self.rcpt_to.clear();
        self.data = None;
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated_wallet.is_some()
    }

    pub fn can_receive_mail(&self) -> bool {
        self.mail_from.is_some() && !self.rcpt_to.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum SmtpCommand {
    Helo { domain: String },
    Ehlo { domain: String },
    MailFrom { address: String, params: HashMap<String, String> },
    RcptTo { address: String, params: HashMap<String, String> },
    Data,
    Rset,
    Noop,
    Quit,
    StartTls,
    Auth { mechanism: String, data: Option<String> },
    Unknown { command: String, args: String },
}

#[derive(Debug, Clone)]
pub enum SmtpResponse {
    Ok { code: u16, message: String },
    Continue { code: u16, message: String },
    TempError { code: u16, message: String },
    PermError { code: u16, message: String },
}

impl SmtpResponse {
    pub fn format(&self) -> String {
        match self {
            SmtpResponse::Ok { code, message } => {
                if message.contains('\n') {
                    let lines: Vec<&str> = message.split('\n').collect();
                    let mut response = String::new();
                    for (i, line) in lines.iter().enumerate() {
                        if i == lines.len() - 1 {
                            response.push_str(&format!("{code} {line}\r\n"));
                        } else {
                            response.push_str(&format!("{code}-{line}\r\n"));
                        }
                    }
                    response
                } else {
                    format!("{code} {message}\r\n")
                }
            }
            SmtpResponse::Continue { code, message } => format!("{code} {message}\r\n"),
            SmtpResponse::TempError { code, message } => format!("{code} {message}\r\n"),
            SmtpResponse::PermError { code, message } => format!("{code} {message}\r\n"),
        }
    }

    pub fn service_ready() -> Self {
        SmtpResponse::Ok { code: 220, message: "sigil-mail ready".to_string() }
    }
    pub fn closing_connection() -> Self {
        SmtpResponse::Ok { code: 221, message: "Service closing transmission channel".to_string() }
    }
    pub fn ok() -> Self {
        SmtpResponse::Ok { code: 250, message: "OK".to_string() }
    }
    pub fn ok_with_message(message: &str) -> Self {
        SmtpResponse::Ok { code: 250, message: message.to_string() }
    }
    pub fn start_mail_input() -> Self {
        SmtpResponse::Continue { code: 354, message: "Start mail input; end with <CRLF>.<CRLF>".to_string() }
    }
    pub fn syntax_error() -> Self {
        SmtpResponse::PermError { code: 500, message: "Syntax error, command unrecognized".to_string() }
    }
    pub fn parameter_error() -> Self {
        SmtpResponse::PermError { code: 501, message: "Syntax error in parameters or arguments".to_string() }
    }
    pub fn not_implemented() -> Self {
        SmtpResponse::PermError { code: 502, message: "Command not implemented".to_string() }
    }
    pub fn bad_sequence() -> Self {
        SmtpResponse::PermError { code: 503, message: "Bad sequence of commands".to_string() }
    }
    pub fn auth_required() -> Self {
        SmtpResponse::PermError { code: 530, message: "Authentication required".to_string() }
    }
    pub fn mailbox_unavailable() -> Self {
        SmtpResponse::PermError { code: 550, message: "Requested action not taken: mailbox unavailable".to_string() }
    }
    pub fn relay_denied() -> Self {
        SmtpResponse::PermError { code: 554, message: "Relay access denied".to_string() }
    }
}

pub struct SmtpCommandParser;

impl SmtpCommandParser {
    pub fn parse(line: &str) -> SmtpCommand {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return SmtpCommand::Unknown { command: String::new(), args: String::new() };
        }

        let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
        let command = parts[0].to_uppercase();
        let args = if parts.len() > 1 { parts[1] } else { "" };

        match command.as_str() {
            "HELO" => {
                if args.is_empty() {
                    SmtpCommand::Unknown { command, args: args.to_string() }
                } else {
                    SmtpCommand::Helo { domain: args.to_string() }
                }
            }
            "EHLO" => {
                if args.is_empty() {
                    SmtpCommand::Unknown { command, args: args.to_string() }
                } else {
                    SmtpCommand::Ehlo { domain: args.to_string() }
                }
            }
            "MAIL" => {
                if let Some(from_addr) = Self::parse_addr_param(args, "FROM:") {
                    SmtpCommand::MailFrom { address: from_addr, params: HashMap::new() }
                } else {
                    SmtpCommand::Unknown { command, args: args.to_string() }
                }
            }
            "RCPT" => {
                if let Some(to_addr) = Self::parse_addr_param(args, "TO:") {
                    SmtpCommand::RcptTo { address: to_addr, params: HashMap::new() }
                } else {
                    SmtpCommand::Unknown { command, args: args.to_string() }
                }
            }
            "DATA" => SmtpCommand::Data,
            "RSET" => SmtpCommand::Rset,
            "NOOP" => SmtpCommand::Noop,
            "QUIT" => SmtpCommand::Quit,
            "STARTTLS" => SmtpCommand::StartTls,
            "AUTH" => {
                let auth_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if auth_parts.is_empty() || auth_parts[0].is_empty() {
                    SmtpCommand::Unknown { command, args: args.to_string() }
                } else {
                    SmtpCommand::Auth {
                        mechanism: auth_parts[0].to_string(),
                        data: auth_parts.get(1).map(|s| s.to_string()),
                    }
                }
            }
            _ => SmtpCommand::Unknown { command, args: args.to_string() },
        }
    }

    /// Shared by `MAIL FROM:<addr>` and `RCPT TO:<addr>` — same shape, only
    /// the keyword differs (the original had two near-identical private
    /// functions for this; merged here since they were byte-for-byte the
    /// same logic).
    fn parse_addr_param(args: &str, keyword: &str) -> Option<String> {
        let pos = args.to_uppercase().find(keyword)?;
        let addr_part = args[pos + keyword.len()..].trim();
        let email_part = match addr_part.find(' ') {
            Some(space_pos) => &addr_part[..space_pos],
            None => addr_part,
        };
        if email_part.starts_with('<') && email_part.ends_with('>') {
            Some(email_part[1..email_part.len() - 1].to_string())
        } else {
            Some(email_part.to_string())
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MailHeaders {
    pub headers: HashMap<String, String>,
}

impl MailHeaders {
    pub fn insert(&mut self, key: String, value: String) {
        self.headers.insert(key.to_lowercase(), value);
    }
    pub fn get(&self, key: &str) -> Option<&String> {
        self.headers.get(&key.to_lowercase())
    }
    pub fn get_first_value(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}

#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub headers: MailHeaders,
    pub body: String,
    #[allow(dead_code)]
    pub raw_message: String,
}

impl ParsedMessage {
    /// Split a raw RFC 5322 message into headers + body and unfold folded
    /// header lines. Ported verbatim from the original — pure text
    /// processing, no identity/storage coupling to adapt.
    pub fn new(raw_message: String) -> Self {
        let (headers_str, body) = if let Some(split_pos) = raw_message.find("\r\n\r\n") {
            (&raw_message[..split_pos], raw_message[split_pos + 4..].to_string())
        } else if let Some(split_pos) = raw_message.find("\n\n") {
            (&raw_message[..split_pos], raw_message[split_pos + 2..].to_string())
        } else {
            (raw_message.as_str(), String::new())
        };

        let mut headers = MailHeaders::default();
        let mut current_header: Option<(String, String)> = None;

        for line in headers_str.lines() {
            let line = line.trim_end_matches('\r');

            if line.is_empty() {
                if let Some((key, value)) = current_header.take() {
                    headers.insert(key, value);
                }
                continue;
            }

            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some((_, ref mut value)) = current_header {
                    value.push(' ');
                    value.push_str(line.trim());
                }
            } else if let Some(colon_pos) = line.find(':') {
                if let Some((key, value)) = current_header.take() {
                    headers.insert(key, value);
                }
                let key = line[..colon_pos].trim().to_string();
                let value = line[colon_pos + 1..].trim().to_string();
                current_header = Some((key, value));
            } else if let Some((_, ref mut value)) = current_header {
                value.push(' ');
                value.push_str(line.trim());
            }
        }
        if let Some((key, value)) = current_header {
            headers.insert(key, value);
        }

        Self { headers, body, raw_message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_helo() {
        match SmtpCommandParser::parse("HELO example.com") {
            SmtpCommand::Helo { domain } => assert_eq!(domain, "example.com"),
            other => panic!("expected HELO, got {other:?}"),
        }
    }

    #[test]
    fn parse_mail_from() {
        match SmtpCommandParser::parse("MAIL FROM:<user@example.com>") {
            SmtpCommand::MailFrom { address, .. } => assert_eq!(address, "user@example.com"),
            other => panic!("expected MAIL FROM, got {other:?}"),
        }
    }

    #[test]
    fn parse_rcpt_to() {
        match SmtpCommandParser::parse("RCPT TO:<user@example.com>") {
            SmtpCommand::RcptTo { address, .. } => assert_eq!(address, "user@example.com"),
            other => panic!("expected RCPT TO, got {other:?}"),
        }
    }

    #[test]
    fn parse_auth_plain_with_data() {
        match SmtpCommandParser::parse("AUTH PLAIN AGZvb0BiYXIAcGFzcw==") {
            SmtpCommand::Auth { mechanism, data } => {
                assert_eq!(mechanism, "PLAIN");
                assert_eq!(data.as_deref(), Some("AGZvb0BiYXIAcGFzcw=="));
            }
            other => panic!("expected AUTH, got {other:?}"),
        }
    }

    #[test]
    fn response_formats_multiline_ehlo_correctly() {
        let r = SmtpResponse::Ok { code: 250, message: "hi\nAUTH PLAIN\nSIZE 10240000".to_string() };
        assert_eq!(r.format(), "250-hi\r\n250-AUTH PLAIN\r\n250 SIZE 10240000\r\n");
    }

    #[test]
    fn parsed_message_splits_headers_and_body_and_unfolds_continuation_lines() {
        let raw = "Subject: hello\r\n Continued Subject\r\nFrom: a@b.c\r\n\r\nbody line one\r\nbody line two";
        let msg = ParsedMessage::new(raw.to_string());
        assert_eq!(msg.headers.get_first_value("subject").as_deref(), Some("hello Continued Subject"));
        assert_eq!(msg.headers.get_first_value("from").as_deref(), Some("a@b.c"));
        assert_eq!(msg.body, "body line one\r\nbody line two");
    }

    #[test]
    fn parsed_message_with_no_body_separator_treats_everything_as_headers() {
        let msg = ParsedMessage::new("Subject: just headers".to_string());
        assert_eq!(msg.headers.get_first_value("subject").as_deref(), Some("just headers"));
        assert_eq!(msg.body, "");
    }
}
