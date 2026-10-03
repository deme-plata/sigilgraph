//! SIGIL Veil — the egress gate for private AI.
//!
//! zkAPI (Ethereum Foundation, live 2026-10-01) hides WHO PAYS for an AI call. It
//! leaves the prompt, the IP and the writing style with the provider. A sigil-top
//! node answers locally by default (flux-moe on loopback), so this crate governs
//! the one moment that matters: a request that is about to LEAVE the machine.
//!
//! The gate, in order:
//!   1. **Hard stop** — a seed phrase or a labelled secret key is never sent, not
//!      even redacted. The request is refused and the reason is returned.
//!   2. **Pseudonymise** — identifiers (64-hex values, qnk/0x addresses, API tokens,
//!      e-mails, IPs, phone numbers, home-directory paths) are replaced with stable
//!      typed placeholders like `[ADDR_1]` BEFORE the bytes leave. The map stays on
//!      the device.
//!   3. **Re-hydrate** — the remote answer talks about `[ADDR_1]`; the gate puts the
//!      real value back locally, so the user gets a coherent answer and the provider
//!      never saw the value.
//!   4. **Ledger** — every outbound request is appended to a local JSONL file: time,
//!      destination, BLAKE3 of the exact bytes, the redacted text, what was found.
//!
//! Pure `std` + `blake3`, no network, no regex engine: the gate must be auditable,
//! offline-decidable and buildable for wasm32.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Veil Sessions: pay for remote AI with one ordinary shielded payment (L2).
pub mod session;
/// L4: vetting an on-device rewrite before it leaves (shared by sigil-top + browser).
pub mod rewrite;

/// What a span of text was recognised as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A run of ≥12 BIP-39 words — a recovery phrase. Never sent.
    Mnemonic,
    /// A 64-hex value next to the words seed/secret/private/key/mnemonic. Never sent.
    SecretKey,
    /// Any other 64-hex value (SIGIL address, coin key, tx hash): pseudonymised.
    Hex64,
    /// `qnk` + 64 hex — a Quillon address.
    QnkAddr,
    /// `0x` + 40 hex — an EVM address.
    EvmAddr,
    /// API tokens: sk-…, ghp_…, github_pat_…, glpat-…, xox?-…, AKIA…, JWTs.
    Token,
    Email,
    Ipv4,
    Phone,
    /// /home/<user>/…, /root/…, C:\Users\<name>\…
    HomePath,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Mnemonic => "MNEMONIC",
            Kind::SecretKey => "SECRET",
            Kind::Hex64 => "HEX",
            Kind::QnkAddr => "QNK",
            Kind::EvmAddr => "EVM",
            Kind::Token => "TOKEN",
            Kind::Email => "EMAIL",
            Kind::Ipv4 => "IP",
            Kind::Phone => "PHONE",
            Kind::HomePath => "PATH",
        }
    }
    /// Never leaves the device in any form.
    pub fn is_hard_stop(self) -> bool {
        matches!(self, Kind::Mnemonic | Kind::SecretKey)
    }
}

/// One recognised span, byte offsets into the ORIGINAL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

fn bip39() -> &'static std::collections::HashSet<&'static str> {
    static SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| include_str!("bip39-english.txt").lines().filter(|l| !l.is_empty()).collect())
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Scan `text` for every sensitive span. Overlaps resolve to the earliest,
/// longest match, so `qnk<64hex>` is one QNK finding, not QNK + HEX.
pub fn scan(text: &str) -> Vec<Finding> {
    let b = text.as_bytes();
    let mut raw: Vec<Finding> = Vec::new();
    scan_mnemonics(text, &mut raw);
    scan_hex_family(text, b, &mut raw);
    scan_tokens(b, &mut raw);
    scan_emails(b, &mut raw);
    scan_ipv4(b, &mut raw);
    scan_phones(b, &mut raw);
    scan_paths(b, &mut raw);
    raw.sort_by(|x, y| x.start.cmp(&y.start).then((y.end - y.start).cmp(&(x.end - x.start))));
    let mut out: Vec<Finding> = Vec::new();
    for f in raw {
        if out.last().map(|l| f.start < l.end).unwrap_or(false) {
            continue;
        }
        out.push(f);
    }
    out
}

fn scan_mnemonics(text: &str, out: &mut Vec<Finding>) {
    // Words with their byte spans; a run of ≥12 consecutive BIP-39 words is a
    // recovery phrase. Ordinary prose almost never manages 12 in a row: "the",
    // "and", "is", "of" are not in the list.
    let set = bip39();
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    let mut run_len = 0usize;
    let mut i = 0usize;
    let b = text.as_bytes();
    let flush = |out: &mut Vec<Finding>, s: Option<usize>, e: usize, n: usize| {
        if let Some(s) = s {
            if n >= 12 {
                out.push(Finding { kind: Kind::Mnemonic, start: s, end: e });
            }
        }
    };
    while i < b.len() {
        if b[i].is_ascii_alphabetic() {
            let s = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            let w = text[s..i].to_ascii_lowercase();
            if set.contains(w.as_str()) {
                if run_start.is_none() {
                    run_start = Some(s);
                    run_len = 0;
                }
                run_len += 1;
                run_end = i;
            } else {
                flush(out, run_start, run_end, run_len);
                run_start = None;
                run_len = 0;
            }
        } else {
            // separators allowed inside a phrase: whitespace, commas, numbering like "1."
            let c = b[i];
            if !(c.is_ascii_whitespace() || c == b',' || c == b'.' || c.is_ascii_digit() || c == b')' || c == b'-') {
                flush(out, run_start, run_end, run_len);
                run_start = None;
                run_len = 0;
            }
            i += 1;
        }
    }
    flush(out, run_start, run_end, run_len);
}

fn preceded_by_secret_word(text: &str, start: usize) -> bool {
    let lo = start.saturating_sub(40);
    let mut lo = lo;
    while lo > 0 && !text.is_char_boundary(lo) {
        lo -= 1;
    }
    let before = text[lo..start].to_ascii_lowercase();
    ["seed", "secret", "private", "priv", "mnemonic", "sk_hex", "sk:", "key:", "key=", "password"]
        .iter()
        .any(|w| before.contains(w))
}

fn scan_hex_family(text: &str, b: &[u8], out: &mut Vec<Finding>) {
    let mut i = 0usize;
    while i < b.len() {
        // a hex run must not be glued to other word characters on the left
        if is_hex(b[i]) && (i == 0 || !is_word_byte(b[i - 1]) || (i >= 3 && &b[i - 3..i] == b"qnk") || (i >= 2 && &b[i - 2..i] == b"0x")) {
            let s = i;
            while i < b.len() && is_hex(b[i]) {
                i += 1;
            }
            let n = i - s;
            let glued_right = i < b.len() && is_word_byte(b[i]);
            if glued_right {
                continue;
            }
            let qnk = s >= 3 && &b[s - 3..s] == b"qnk" && (s == 3 || !is_word_byte(b[s - 4]));
            let ox = s >= 2 && (&b[s - 2..s] == b"0x" || &b[s - 2..s] == b"0X") && (s == 2 || !is_word_byte(b[s - 3]));
            if qnk && n == 64 {
                out.push(Finding { kind: Kind::QnkAddr, start: s - 3, end: i });
            } else if ox && n == 40 {
                out.push(Finding { kind: Kind::EvmAddr, start: s - 2, end: i });
            } else if n == 64 || (ox && n == 64) {
                let start = if ox { s - 2 } else { s };
                let kind = if preceded_by_secret_word(text, start) { Kind::SecretKey } else { Kind::Hex64 };
                out.push(Finding { kind, start, end: i });
            } else if n >= 96 && preceded_by_secret_word(text, s) {
                // long hex blobs labelled as keys (e.g. a 1410-hex SQIsign SK)
                out.push(Finding { kind: Kind::SecretKey, start: s, end: i });
            }
        } else {
            i += 1;
        }
    }
}

fn scan_tokens(b: &[u8], out: &mut Vec<Finding>) {
    const PREFIXES: &[&[u8]] = &[
        b"sk-", b"sk_live_", b"sk_test_", b"rk_live_", b"ghp_", b"gho_", b"ghs_", b"ghu_", b"github_pat_",
        b"glpat-", b"xoxb-", b"xoxp-", b"xoxa-", b"AKIA", b"AIza",
    ];
    let mut i = 0usize;
    while i < b.len() {
        let at_boundary = i == 0 || !is_word_byte(b[i - 1]);
        if at_boundary {
            if let Some(p) = PREFIXES.iter().find(|p| b[i..].starts_with(p)) {
                let mut j = i + p.len();
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                    j += 1;
                }
                if j - i - p.len() >= 16 {
                    out.push(Finding { kind: Kind::Token, start: i, end: j });
                    i = j;
                    continue;
                }
            }
            // JWT: eyJ<b64url>.eyJ<b64url>.<b64url>
            if b[i..].starts_with(b"eyJ") {
                let mut j = i;
                let mut dots = 0;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-' || b[j] == b'.') {
                    if b[j] == b'.' {
                        dots += 1;
                    }
                    j += 1;
                }
                if dots == 2 && j - i >= 40 {
                    out.push(Finding { kind: Kind::Token, start: i, end: j });
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
}

fn scan_emails(b: &[u8], out: &mut Vec<Finding>) {
    let local = |c: u8| c.is_ascii_alphanumeric() || b"._%+-".contains(&c);
    let domain = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'.';
    for (at, &c) in b.iter().enumerate() {
        if c != b'@' || at == 0 {
            continue;
        }
        let mut s = at;
        while s > 0 && local(b[s - 1]) {
            s -= 1;
        }
        let mut e = at + 1;
        while e < b.len() && domain(b[e]) {
            e += 1;
        }
        while e > at + 1 && b[e - 1] == b'.' {
            e -= 1;
        }
        let dom = &b[at + 1..e];
        let has_tld = dom.iter().rposition(|&x| x == b'.').map(|p| dom.len() - p - 1 >= 2 && p > 0).unwrap_or(false);
        if s < at && has_tld {
            out.push(Finding { kind: Kind::Email, start: s, end: e });
        }
    }
}

fn scan_ipv4(b: &[u8], out: &mut Vec<Finding>) {
    let mut i = 0usize;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.')) {
            let s = i;
            let mut parts = 0;
            let mut ok = true;
            let mut j = i;
            loop {
                let ps = j;
                while j < b.len() && b[j].is_ascii_digit() && j - ps < 4 {
                    j += 1;
                }
                let n = j - ps;
                if n == 0 || n > 3 {
                    ok = false;
                    break;
                }
                let v: u32 = std::str::from_utf8(&b[ps..j]).ok().and_then(|x| x.parse().ok()).unwrap_or(999);
                if v > 255 {
                    ok = false;
                    break;
                }
                parts += 1;
                if parts == 4 {
                    break;
                }
                if j < b.len() && b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit() {
                    j += 1;
                } else {
                    ok = false;
                    break;
                }
            }
            let glued = j < b.len() && (b[j].is_ascii_alphanumeric() || (b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit()));
            if ok && parts == 4 && !glued {
                // loopback and the unspecified address identify nobody
                let txt = std::str::from_utf8(&b[s..j]).unwrap_or("");
                if !txt.starts_with("127.") && txt != "0.0.0.0" {
                    out.push(Finding { kind: Kind::Ipv4, start: s, end: j });
                }
                i = j;
                continue;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
}

fn scan_phones(b: &[u8], out: &mut Vec<Finding>) {
    // International form only (+CC …): 8–15 digits with spaces/dashes. Bare
    // 8-digit numbers are too often amounts or heights to redact blindly.
    for i in 0..b.len() {
        if b[i] != b'+' || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
            continue;
        }
        let mut j = i + 1;
        let mut digits = 0;
        while j < b.len() && (b[j].is_ascii_digit() || b[j] == b' ' || b[j] == b'-') {
            if b[j].is_ascii_digit() {
                digits += 1;
            }
            j += 1;
        }
        while j > i + 1 && !b[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if (8..=15).contains(&digits) && !(j < b.len() && b[j].is_ascii_alphanumeric()) {
            out.push(Finding { kind: Kind::Phone, start: i, end: j });
        }
    }
}

fn scan_paths(b: &[u8], out: &mut Vec<Finding>) {
    let pathc = |c: u8| !(c.is_ascii_whitespace() || b"\"'`<>|,;()[]{}".contains(&c));
    let mut i = 0usize;
    while i < b.len() {
        let boundary = i == 0 || !pathc(b[i - 1]);
        let hit = boundary
            && (b[i..].starts_with(b"/home/")
                || b[i..].starts_with(b"/root/")
                || b[i..].starts_with(b"/Users/")
                || (b[i..].len() > 9 && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/') && b[i + 3..].to_ascii_lowercase().starts_with(b"users")));
        if hit {
            let mut j = i;
            while j < b.len() && pathc(b[j]) {
                j += 1;
            }
            out.push(Finding { kind: Kind::HomePath, start: i, end: j });
            i = j;
        } else {
            i += 1;
        }
    }
}

/// The placeholder ↔ original map for one conversation. Stable: the same value
/// always gets the same placeholder, so the remote model can reason about
/// "[ADDR_1] sent to [ADDR_2]" consistently across turns.
#[derive(Debug, Clone, Default)]
pub struct VeilMap {
    by_value: BTreeMap<String, String>,
    by_placeholder: BTreeMap<String, String>,
    counters: BTreeMap<Kind, usize>,
}

impl VeilMap {
    pub fn new() -> Self {
        Self::default()
    }
    fn placeholder_for(&mut self, kind: Kind, value: &str) -> String {
        if let Some(p) = self.by_value.get(value) {
            return p.clone();
        }
        let n = self.counters.entry(kind).or_insert(0);
        *n += 1;
        let p = format!("[{}_{}]", kind.tag(), n);
        self.by_value.insert(value.to_string(), p.clone());
        self.by_placeholder.insert(p.clone(), value.to_string());
        p
    }
    pub fn len(&self) -> usize {
        self.by_value.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_value.is_empty()
    }
}

/// What the gate decided for one outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Safe to send THIS text (already pseudonymised).
    Send { text: String, findings: Vec<(Kind, String)> },
    /// Refused: contains material that never leaves the device.
    Refuse { reasons: Vec<Kind> },
}

/// Gate one outbound request. Hard stops refuse the whole request; everything
/// else sensitive is replaced by placeholders recorded in `map`.
pub fn gate(text: &str, map: &mut VeilMap) -> Verdict {
    let found = scan(text);
    let mut stops: Vec<Kind> = found.iter().filter(|f| f.kind.is_hard_stop()).map(|f| f.kind).collect();
    if !stops.is_empty() {
        stops.sort();
        stops.dedup();
        return Verdict::Refuse { reasons: stops };
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut findings = Vec::new();
    for f in &found {
        out.push_str(&text[last..f.start]);
        let p = map.placeholder_for(f.kind, &text[f.start..f.end]);
        findings.push((f.kind, p.clone()));
        out.push_str(&p);
        last = f.end;
    }
    out.push_str(&text[last..]);
    Verdict::Send { text: out, findings }
}

/// Pseudonymise WITHOUT refusing — for text the user didn't write (system
/// prompts, skill text) where a hard stop would lock the feature: hard-stop kinds
/// are replaced like everything else, so the value still never leaves.
pub fn pseudonymize(text: &str, map: &mut VeilMap) -> (String, Vec<(Kind, String)>) {
    let found = scan(text);
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut findings = Vec::new();
    for f in &found {
        out.push_str(&text[last..f.start]);
        let p = map.placeholder_for(f.kind, &text[f.start..f.end]);
        findings.push((f.kind, p.clone()));
        out.push_str(&p);
        last = f.end;
    }
    out.push_str(&text[last..]);
    (out, findings)
}

/// Put the real values back into a remote answer./// Put the real values back into a remote answer. Unknown placeholders are left
/// as they are (the model may invent `[ADDR_9]`; that must not crash or guess).
pub fn rehydrate(answer: &str, map: &VeilMap) -> String {
    let mut out = String::with_capacity(answer.len());
    let mut rest = answer;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        match tail.find(']') {
            Some(close) if close <= 24 => {
                let ph = &tail[..=close];
                match map.by_placeholder.get(ph) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(ph),
                }
                rest = &tail[close + 1..];
            }
            _ => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Rehydrate a STREAMED answer. Tokens arrive in arbitrary pieces, so a
/// placeholder can be split ("[HE" + "X_1]"); text from an unmatched `[` is
/// held back until it closes or grows past any placeholder's length.
#[derive(Debug, Default)]
pub struct StreamRehydrator {
    held: String,
}

/// Longest placeholder we ever mint is "[MNEMONIC_99999]"; anything longer than
/// this inside brackets is ordinary text and is released.
const MAX_PLACEHOLDER: usize = 24;

impl StreamRehydrator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns the text that can be shown now.
    pub fn push(&mut self, chunk: &str, map: &VeilMap) -> String {
        self.held.push_str(chunk);
        // Everything up to the last '[' that is still open can go out; the open
        // tail stays held unless it is already too long to be a placeholder.
        let cut = match self.held.rfind('[') {
            Some(open) if !self.held[open..].contains(']') && self.held.len() - open <= MAX_PLACEHOLDER => open,
            _ => self.held.len(),
        };
        let ready: String = self.held[..cut].to_string();
        self.held.drain(..cut);
        rehydrate(&ready, map)
    }

    /// End of stream: release whatever is held.
    pub fn finish(&mut self, map: &VeilMap) -> String {
        let rest = std::mem::take(&mut self.held);
        rehydrate(&rest, map)
    }
}

/// One line of the egress ledger./// One line of the egress ledger.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub ts: u64,
    pub destination: String,
    /// BLAKE3 of the exact bytes that left (after pseudonymisation).
    pub bytes_blake3: String,
    pub bytes: usize,
    pub sent_text: String,
    pub kinds: Vec<Kind>,
}

impl LedgerEntry {
    pub fn new(destination: &str, sent_text: &str, findings: &[(Kind, String)]) -> Self {
        // wasm32-unknown-unknown has no clock: SystemTime::now() PANICS there, and a
        // panic inside a wasm-bindgen method leaves the object's borrow flag stuck
        // (found by the browser E2E, 2026-10-03). Browser callers use `new_at`.
        #[cfg(not(target_arch = "wasm32"))]
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        #[cfg(target_arch = "wasm32")]
        let ts = 0;
        Self::new_at(ts, destination, sent_text, findings)
    }

    /// Same as [`new`](Self::new) with the caller's clock (unix seconds).
    pub fn new_at(ts: u64, destination: &str, sent_text: &str, findings: &[(Kind, String)]) -> Self {
        let mut kinds: Vec<Kind> = findings.iter().map(|(k, _)| *k).collect();
        kinds.sort();
        kinds.dedup();
        LedgerEntry {
            ts,
            destination: destination.to_string(),
            bytes_blake3: blake3::hash(sent_text.as_bytes()).to_hex().to_string(),
            bytes: sent_text.len(),
            sent_text: sent_text.to_string(),
            kinds,
        }
    }

    pub fn to_json_line(&self) -> String {
        let kinds: Vec<String> = self.kinds.iter().map(|k| format!("\"{}\"", k.tag())).collect();
        format!(
            "{{\"ts\":{},\"destination\":{},\"bytes\":{},\"blake3\":\"{}\",\"redacted\":[{}],\"sent\":{}}}\n",
            self.ts,
            json_str(&self.destination),
            self.bytes,
            self.bytes_blake3,
            kinds.join(","),
            json_str(&self.sent_text)
        )
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

// ── "what did my AI send out?" ────────────────────────────────────────────────

/// Aggregate of an egress ledger, for a user-facing privacy view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerSummary {
    pub sends: usize,
    pub bytes: u64,
    pub first_ts: u64,
    pub last_ts: u64,
    /// destination → sends
    pub destinations: BTreeMap<String, usize>,
    /// placeholder kind tag → number of sends that hid one
    pub hidden: BTreeMap<String, usize>,
    /// newest first: (ts, destination, bytes, hidden kinds)
    pub recent: Vec<(u64, String, u64, Vec<String>)>,
}

fn field_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let i = line.find(key)? + key.len();
    Some(&line[i..])
}

fn json_string_at(s: &str) -> Option<String> {
    // s starts at the opening quote of a JSON string we wrote ourselves
    let mut out = String::new();
    let mut chars = s.strip_prefix('"')?.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let h: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

fn number_at(s: &str) -> u64 {
    s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
}

/// Summarise ledger JSONL written by [`LedgerEntry::to_json_line`]. Lines that
/// don't parse are skipped, never fatal — the view must always open.
pub fn ledger_summary(jsonl: &str, recent: usize) -> LedgerSummary {
    let mut s = LedgerSummary::default();
    let mut all: Vec<(u64, String, u64, Vec<String>)> = Vec::new();
    for line in jsonl.lines() {
        let Some(dest) = field_after(line, "\"destination\":").and_then(json_string_at) else { continue };
        let ts = field_after(line, "\"ts\":").map(number_at).unwrap_or(0);
        let bytes = field_after(line, "\"bytes\":").map(number_at).unwrap_or(0);
        let kinds: Vec<String> = field_after(line, "\"redacted\":[")
            .and_then(|r| r.split(']').next())
            .map(|r| r.split(',').filter_map(|k| { let k = k.trim().trim_matches('"'); (!k.is_empty()).then(|| k.to_string()) }).collect())
            .unwrap_or_default();
        s.sends += 1;
        s.bytes += bytes;
        if s.first_ts == 0 || (ts > 0 && ts < s.first_ts) {
            s.first_ts = ts;
        }
        s.last_ts = s.last_ts.max(ts);
        *s.destinations.entry(dest.clone()).or_insert(0) += 1;
        for k in &kinds {
            *s.hidden.entry(k.clone()).or_insert(0) += 1;
        }
        all.push((ts, dest, bytes, kinds));
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));
    s.recent = all.into_iter().take(recent).collect();
    s
}

/// Plain-text privacy view (sigil-top's AI tab; the browser renders its own).
pub fn render_summary(s: &LedgerSummary, now: u64) -> String {
    let ago = |ts: u64| {
        let d = now.saturating_sub(ts);
        if ts == 0 { "?".to_string() }
        else if d < 120 { format!("{d}s ago") }
        else if d < 7200 { format!("{}m ago", d / 60) }
        else if d < 172_800 { format!("{}h ago", d / 3600) }
        else { format!("{}d ago", d / 86_400) }
    };
    if s.sends == 0 {
        return "🕶 Nothing has ever left this machine for an AI model.".to_string();
    }
    let mut out = vec![format!(
        "🕶 {} request(s) left this machine for an off-device model — {:.1} KB in total, last {}.",
        s.sends,
        s.bytes as f64 / 1024.0,
        ago(s.last_ts)
    )];
    let dests: Vec<String> = s.destinations.iter().map(|(d, n)| format!("{d} ×{n}")).collect();
    out.push(format!("   to: {}", dests.join(", ")));
    if s.hidden.is_empty() {
        out.push("   nothing needed hiding in them.".into());
    } else {
        let h: Vec<String> = s.hidden.iter().map(|(k, n)| format!("{k} in {n}")).collect();
        out.push(format!("   hidden before sending (as placeholders, restored only here): {}", h.join(", ")));
    }
    for (ts, d, b, k) in &s.recent {
        out.push(format!(
            "   · {} → {} ({} B){}",
            ago(*ts),
            d,
            b,
            if k.is_empty() { String::new() } else { format!(", hid {}", k.join("+")) }
        ));
    }
    out.push("   Recovery phrases and secret keys are never sent; a message containing one is refused.".into());
    out.join("\n")
}

/// Append an entry to the local egress ledger (JSONL)./// Append an entry to the local egress ledger (JSONL). Not on wasm32 — the
/// browser keeps its ledger in its own storage via `to_json_line`.
#[cfg(not(target_arch = "wasm32"))]
pub fn ledger_append(path: &std::path::Path, entry: &LedgerEntry) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(entry.to_json_line().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "4973498a9865b291636faef205f728a49d98890f001e9e806479043f038ebf6c";

    fn send(text: &str) -> (String, Vec<(Kind, String)>, VeilMap) {
        let mut m = VeilMap::new();
        match gate(text, &mut m) {
            Verdict::Send { text, findings } => (text, findings, m),
            v => panic!("expected Send, got {v:?}"),
        }
    }

    #[test]
    fn wordlist_is_the_standard_2048() {
        assert_eq!(bip39().len(), 2048);
        assert!(bip39().contains("abandon") && bip39().contains("zoo"));
    }

    #[test]
    fn mnemonic_is_a_hard_stop() {
        let t = "my wallet: abandon ability able about above absent absorb abstract absurd abuse access accident — is it safe?";
        let mut m = VeilMap::new();
        assert_eq!(gate(t, &mut m), Verdict::Refuse { reasons: vec![Kind::Mnemonic] });
        assert!(m.is_empty(), "nothing may be recorded from a refused request");
    }

    #[test]
    fn numbered_mnemonic_is_caught_too() {
        let t = "1. legal 2. winner 3. thank 4. year 5. wave 6. sausage 7. worth 8. useful 9. legal 10. winner 11. thank 12. yellow";
        assert!(matches!(gate(t, &mut VeilMap::new()), Verdict::Refuse { .. }));
    }

    #[test]
    fn ordinary_prose_is_not_a_mnemonic() {
        let t = "Can you explain why the compile time of the node keeps rising after we added the producer feature and the GPU stack?";
        let (out, f, _) = send(t);
        assert_eq!(out, t);
        assert!(f.is_empty());
    }

    #[test]
    fn labelled_secret_hex_is_a_hard_stop_bare_hex_is_pseudonymised() {
        let t = format!("my seed is {HEX}");
        assert_eq!(gate(&t, &mut VeilMap::new()), Verdict::Refuse { reasons: vec![Kind::SecretKey] });
        let t = format!("why did sending to {HEX} fail?");
        let (out, f, m) = send(&t);
        assert_eq!(out, "why did sending to [HEX_1] fail?");
        assert_eq!(f[0].0, Kind::Hex64);
        assert_eq!(rehydrate("Check that [HEX_1] is registered.", &m), format!("Check that {HEX} is registered."));
    }

    #[test]
    fn qnk_and_evm_addresses_are_one_finding_each() {
        let t = format!("pay qnk{HEX} and 0x52908400098527886E0F7030069857D2E4169EE7 please");
        let (out, f, _) = send(&t);
        assert_eq!(out, "pay [QNK_1] and [EVM_1] please");
        assert_eq!(f.iter().map(|x| x.0).collect::<Vec<_>>(), vec![Kind::QnkAddr, Kind::EvmAddr]);
    }

    #[test]
    fn same_value_same_placeholder_across_turns() {
        let mut m = VeilMap::new();
        let a = format!("from {HEX} to qnk{HEX}");
        let Verdict::Send { text: t1, .. } = gate(&a, &mut m) else { panic!() };
        let Verdict::Send { text: t2, .. } = gate(&format!("again {HEX}"), &mut m) else { panic!() };
        assert_eq!(t1, "from [HEX_1] to [QNK_1]");
        assert_eq!(t2, "again [HEX_1]");
    }

    #[test]
    fn tokens_emails_ips_phones_paths() {
        let t = "key sk-proj-AbCdEfGhIjKlMnOpQrStUv, mail overdrevetfedmetodologi@proton.me, box 89.149.241.126 \
                 not 127.0.0.1, call +45 12 34 56 78, file /home/storage/deepseek-codewhale/sigil/Cargo.toml";
        let (out, f, m) = send(t);
        let kinds: Vec<Kind> = f.iter().map(|x| x.0).collect();
        assert_eq!(kinds, vec![Kind::Token, Kind::Email, Kind::Ipv4, Kind::Phone, Kind::HomePath], "{out}");
        assert!(out.contains("127.0.0.1"), "loopback identifies nobody: {out}");
        assert!(!out.contains("proton.me") && !out.contains("89.149") && !out.contains("deepseek-codewhale"), "{out}");
        assert_eq!(rehydrate(&out, &m), t, "round trip must restore the exact original");
    }

    #[test]
    fn heights_amounts_and_short_hex_are_left_alone() {
        let t = "block 27000000 at height 26950000 moved 12.5 SIGIL; commit 7f2ac48b; version 10.4.18";
        let (out, f, _) = send(t);
        assert_eq!(out, t);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn jwt_and_github_pat_are_tokens() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let (out, f, _) = send(&format!("token {jwt} and ghp_16C7e42F292c6912E7710c838347Ae178B4a"));
        assert_eq!(f.len(), 2, "{out}");
        assert_eq!(out, "token [TOKEN_1] and [TOKEN_2]");
    }

    #[test]
    fn pseudonymize_never_refuses_but_still_hides() {
        let mut m = VeilMap::new();
        let (out, f) = pseudonymize(&format!("example: seed = {HEX}"), &mut m);
        assert_eq!(out, "example: seed = [SECRET_1]");
        assert_eq!(f[0].0, Kind::SecretKey);
    }

    #[test]
    fn rehydrate_leaves_invented_placeholders_and_brackets() {
        let m = VeilMap::new();
        assert_eq!(rehydrate("see [ADDR_9] and [1] and [unclosed", &m), "see [ADDR_9] and [1] and [unclosed");
    }

    #[test]
    fn streamed_placeholders_split_across_chunks_are_restored() {
        let (_, _, m) = send(&format!("tx {HEX} to qnk{HEX}"));
        let chunks = ["Check [H", "EX_1", "] then send to [QN", "K_1]. Also [not a placeholder at all, just text] ok", " [ADDR_7] [", "end"];
        let mut r = StreamRehydrator::new();
        let mut out = String::new();
        for c in chunks {
            out.push_str(&r.push(c, &m));
        }
        out.push_str(&r.finish(&m));
        assert_eq!(out, format!("Check {HEX} then send to qnk{HEX}. Also [not a placeholder at all, just text] ok [ADDR_7] [end"));
    }

    #[test]
    fn stream_rehydrator_does_not_hold_ordinary_text() {
        let m = VeilMap::new();
        let mut r = StreamRehydrator::new();
        assert_eq!(r.push("hello ", &m), "hello ");
        assert_eq!(r.push("world", &m), "world");
        assert_eq!(r.push(" [x", &m), " ");
        assert_eq!(r.finish(&m), "[x");
    }

    #[test]
    fn ledger_line_is_json_and_hashes_exact_bytes() {
        let (out, f, _) = send(&format!("explain tx {HEX}"));
        let e = LedgerEntry::new("https://api.example/v1/chat", &out, &f);
        let line = e.to_json_line();
        assert!(line.ends_with("}\n"));
        assert!(line.contains("\"redacted\":[\"HEX\"]"));
        assert_eq!(e.bytes_blake3, blake3::hash(out.as_bytes()).to_hex().to_string());
        assert!(!line.contains(HEX), "the ledger stores what LEFT, never the original value");
    }

    #[test]
    fn ledger_summary_reads_back_what_we_wrote() {
        let (out, f, _) = send(&format!("explain tx {HEX} for viktor@sigilgraph.org"));
        let a = LedgerEntry::new_at(1_000, "http://gpu.example:11434", &out, &f).to_json_line();
        let b = LedgerEntry::new_at(2_000, "https://api.example", "{\"q\":\"hi \\\"x\\\"\"}", &[]).to_json_line();
        let junk = "not json at all\n";
        let s = ledger_summary(&format!("{a}{junk}{b}"), 5);
        assert_eq!(s.sends, 2);
        assert_eq!((s.first_ts, s.last_ts), (1_000, 2_000));
        assert_eq!(s.destinations.get("http://gpu.example:11434"), Some(&1));
        assert_eq!(s.hidden.get("HEX"), Some(&1));
        assert_eq!(s.hidden.get("EMAIL"), Some(&1));
        assert_eq!(s.recent[0].1, "https://api.example", "newest first");
        let txt = render_summary(&s, 2_060);
        assert!(txt.contains("2 request(s)") && txt.contains("60s ago") && txt.contains("HEX in 1"), "{txt}");
        assert!(!txt.contains(HEX), "the view never shows original values");
        assert_eq!(render_summary(&LedgerSummary::default(), 0), "🕶 Nothing has ever left this machine for an AI model.");
    }

    #[test]
    fn utf8_text_is_safe() {
        let t = "Hvorfor fejlede betalingen til æøå — kontakt viktor@sigilgraph.org 🙂";
        let (out, _, m) = send(t);
        assert_eq!(out, "Hvorfor fejlede betalingen til æøå — kontakt [EMAIL_1] 🙂");
        assert_eq!(rehydrate(&out, &m), t);
    }
}
