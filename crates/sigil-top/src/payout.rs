//! [P] — where your mined coins go (10.8.0, Viktor 2026-11-25: "a tui update shortcut to insert or
//! configure wallet address for mining. in windows i expect users to be unaware of the environment variable").
//!
//! Before this there was no way to mine into another wallet from the screen. `SIGIL_MINE_WALLET` existed, but
//! since 2026-10-02 sigil-top makes a wallet of its own on first run and that wallet outranks the variable, so
//! setting it did nothing on almost every install.
//!
//! The address is kept in one small file, `<home>/.flux/sigil-mine-payout`, written ONLY by this screen,
//! `sigil-top wallet mine-to` and the first-run question. It is deliberately not the older
//! `sigil-mine-wallet` file: the [W] page can write that one through the loopback API, and a web page must
//! never be able to move somebody's mining rewards. When the file holds a valid address it decides where
//! rewards go; nothing else changes — [W], sending and the private balance stay on this computer's own
//! wallet, because the key of a payout address is somewhere else.
//!
//! An address and a secret key look the same (64 hex characters), and a miner publishes its address with
//! every share. So a paste is checked before anything is saved:
//!   1. shape — an address, a `sigil:` code or a wallet link; words, coin links, Quillon addresses and
//!      `0x…` keys are refused in plain words ([`parse_address`]);
//!   2. it must be a public key at all (a point on the curve) — about half of all typos and half of all
//!      pasted secret keys stop here;
//!   3. the paste is read AS a key, and the chain is asked about the address that key would open. If that
//!      wallet exists, the paste is somebody's secret key. Only the derived public address leaves this
//!      computer; the paste itself is looked up only after it passed.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::console_glyphs::sa;
use crate::heroes::card_block;
use crate::mining_ui::fmt_glyphs;
use crate::tabs_ui::dim;
use crate::{App, C_BG, C_DIM, C_GOLD, C_NEON_CYAN, C_NEON_GOLD, C_NEON_GREEN, C_RED, C_VBRIGHT};

/// Longest text the box takes — a full payment code is ~210 characters.
const INPUT_MAX: usize = 400;
/// `Y` is ignored this long after the confirm question appears: a paste that ends in a newline and more
/// text must never answer it.
const CONFIRM_GUARD: Duration = Duration::from_millis(400);

// ── where it is kept ─────────────────────────────────────────────────────────────────────

pub(crate) fn path_in(home: &str) -> String {
    format!("{home}/.flux/sigil-mine-payout")
}

/// The payout address somebody chose on this install, if any (a damaged file counts as none).
pub(crate) fn load_in(home: &str) -> Option<String> {
    let t = std::fs::read_to_string(path_in(home)).ok()?;
    let a = t.trim().to_ascii_lowercase();
    parse_address(&a).ok()
}

/// Save the payout address: written beside, then renamed into place, so a crash never leaves half an address.
pub(crate) fn save_in(home: &str, addr: &str) -> Result<(), String> {
    let addr = parse_address(addr)?;
    std::fs::create_dir_all(format!("{home}/.flux")).map_err(|e| format!("cannot create {home}/.flux: {e}"))?;
    let (p, tmp) = (path_in(home), format!("{}.new", path_in(home)));
    std::fs::write(&tmp, format!("{addr}\n")).map_err(|e| format!("cannot write {tmp}: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("cannot move {tmp} into place: {e}"))?;
    match load_in(home) {
        Some(got) if got == addr => Ok(()),
        _ => Err(format!("{p} does not read back as the address just written")),
    }
}

/// Back to this computer's own wallet. `Ok(false)` = there was nothing to undo.
pub(crate) fn clear_in(home: &str) -> Result<bool, String> {
    match std::fs::remove_file(path_in(home)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", path_in(home))),
    }
}

/// The chosen payout address of this install.
pub(crate) fn chosen() -> Option<String> {
    load_in(&crate::flux_home())
}

/// The wallet mining rewards are credited to: the address chosen with [P], else this computer's own wallet.
/// Not the same thing as [`crate::miner_wallet`], which is the wallet this computer holds the KEY of.
pub(crate) fn mining_wallet() -> String {
    chosen().unwrap_or_else(crate::miner_wallet)
}

// ── reading what somebody pasted ─────────────────────────────────────────────────────────

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Is this 64-hex text a usable public key? A random string is one only about half the time.
fn is_wallet_key(addr: &str) -> bool {
    crate::hex_to_32(addr).is_some_and(|b| sigil_oauth::is_wallet_key(&b))
}

/// True when the text reads like a recovery phrase (several words that are not hex).
pub(crate) fn looks_like_words(t: &str) -> bool {
    let w: Vec<&str> = t.split_whitespace().collect();
    w.len() >= 2 && w.iter().filter(|x| !x.bytes().all(|b| b.is_ascii_hexdigit())).count() * 2 >= w.len()
}

/// The address in whatever was pasted — a bare address, a `sigil:` code from Receive, or a wallet link with
/// `?addr=` — or a plain sentence saying what was pasted instead. Lower-case; never sends anything anywhere.
pub(crate) fn parse_address(raw: &str) -> Result<String, String> {
    // Copying from a chat or a terminal brings quotes, invisible spaces and line breaks along.
    let t: String = raw.chars().filter(|c| !matches!(c, '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}')).collect();
    let t = t.trim().trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>' | '“' | '”')).trim();
    if t.is_empty() {
        return Err("Paste or type a wallet address first.".into());
    }
    let low = t.to_ascii_lowercase();
    if low.starts_with("sigil:coin") || low.starts_with("sigil://coin") || low.contains("claim.html#") {
        return Err("That is a coin link: it holds the coin's secret key. It is not a wallet address, and you should not share it.".into());
    }
    if low.starts_with("sigil1s:") {
        return Err("That code has no public address in it. Mining pays to a public address: copy the 64-character address from Receive.".into());
    }
    let cand = if let Some(rest) = low.strip_prefix("sigil:") {
        rest.trim_start_matches('/').split('?').next().unwrap_or("").trim().to_string()
    } else if low.starts_with("http://") || low.starts_with("https://") || low.starts_with("localhost") {
        match low.split(['?', '&', '#']).find_map(|p| p.strip_prefix("addr=").or_else(|| p.strip_prefix("wallet="))) {
            Some(a) => a.trim().to_string(),
            None => return Err("That link has no wallet address in it. Copy the 64-character address from Receive in your wallet.".into()),
        }
    } else if low.starts_with("qnk") {
        return Err("That is a Quillon (QUG) address. SIGIL is another network: its addresses have no qnk in front. Copy yours from a SIGIL wallet.".into());
    } else if looks_like_words(t) {
        return Err("Those look like recovery words. They are secret: never paste them here. Mining only needs the address.".into());
    } else if low.starts_with("0x") {
        return Err(if hex64(&low[2..]) {
            "Starts with 0x and has 64 characters: that is usually a secret key, not an address. A SIGIL address has no 0x.".into()
        } else {
            "That is an Ethereum-style address. A SIGIL address is 64 characters (0-9, a-f) without 0x.".to_string()
        });
    } else {
        // An address wrapped over two terminal lines arrives with a break in the middle.
        low.split_whitespace().collect::<String>()
    };
    if !hex64(&cand) {
        let bad = cand.chars().find(|c| !c.is_ascii_hexdigit());
        return Err(match bad {
            Some(c) => format!("A SIGIL address has only 0-9 and a-f in it; this has \"{c}\"."),
            None => format!("A SIGIL address is exactly 64 characters; this is {}.", cand.len()),
        });
    }
    if !is_wallet_key(&cand) {
        return Err("This is not a wallet address. A character is probably wrong, or it is a secret key. Copy it again from Receive in your wallet.".into());
    }
    Ok(cand)
}

// ── asking the chain ─────────────────────────────────────────────────────────────────────

/// What the network knows about one address. `None` = it did not answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Seen {
    pub balance: Option<u128>,
    pub registered: Option<bool>,
}

impl Seen {
    /// In use on the chain? `None` while either answer is missing and neither already says yes.
    pub(crate) fn in_use(&self) -> Option<bool> {
        if self.balance.is_some_and(|b| b > 0) || self.registered == Some(true) {
            return Some(true);
        }
        (self.balance.is_some() && self.registered.is_some()).then_some(false)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Check {
    /// The paste is the secret key of this existing wallet.
    SecretKeyOf(String),
    /// A wallet the chain knows; the text says what it knows.
    Known(String),
    /// Nothing has ever happened on this address.
    New,
    /// The network did not answer, so nothing could be checked (and the paste was not sent).
    NoAnswer,
}

/// The check, with the chain behind a closure so the tests can see exactly what would be sent.
pub(crate) fn check_with(addr: &str, own_secret: Option<[u8; 32]>, mut ask: impl FnMut(&str) -> Seen) -> Check {
    let Some(as_key) = crate::hex_to_32(addr) else { return Check::NoAnswer };
    let opens = sigil_oauth::Keypair::from_seed(&as_key).pubkey_hex();
    if own_secret == Some(as_key) {
        return Check::SecretKeyOf(opens); // this computer's own key: no question needed
    }
    match ask(&opens).in_use() {
        Some(true) => return Check::SecretKeyOf(opens),
        Some(false) => {}
        None => return Check::NoAnswer,
    }
    let s = ask(addr);
    match s.in_use() {
        Some(true) => {
            let mut what = Vec::new();
            if let Some(b) = s.balance {
                what.push(format!("public balance {} SIGIL", fmt_glyphs(b, 4)));
            }
            if s.registered == Some(true) {
                what.push("can be paid privately".to_string());
            }
            Check::Known(what.join(", "))
        }
        Some(false) => Check::New,
        None => Check::NoAnswer,
    }
}

/// One look-up over HTTP. Three tries: the public gateway drops some new connections.
fn look_up(client: &reqwest::blocking::Client, base: &str, addr: &str) -> Seen {
    let get = |path: String| -> Option<serde_json::Value> {
        for i in 0..3u64 {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(300 * i));
            }
            if let Ok(v) = client.get(format!("{}{path}", base.trim_end_matches('/'))).send().and_then(|r| r.json()) {
                return Some(v);
            }
        }
        None
    };
    let balance = get(format!("/v1/balance?wallet={addr}"))
        .and_then(|v| v.pointer("/data/balance").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()));
    let registered = get(format!("/v1/shielded/address?wallet={addr}")).and_then(|v| parse_registered(&v));
    Seen { balance, registered }
}

fn parse_registered(v: &serde_json::Value) -> Option<bool> {
    match v.get("ok").and_then(|x| x.as_bool()) {
        Some(true) => Some(true),
        Some(false) if v.get("error").and_then(|e| e.as_str()).is_some_and(|e| e.contains("not registered")) => Some(false),
        _ => None,
    }
}

pub(crate) fn check(addr: &str) -> Check {
    let Ok(client) = reqwest::blocking::Client::builder().timeout(Duration::from_secs(6)).build() else {
        return Check::NoAnswer;
    };
    let base = crate::top_core::money_node_url();
    check_with(addr, crate::miner_seed(), |a| look_up(&client, &base, a))
}

// ── words for people ─────────────────────────────────────────────────────────────────────

/// `4f2cabcd…9ab1ef` — enough to recognise, short enough for a status line.
pub(crate) fn short(a: &str) -> String {
    if a.len() >= 14 && a.is_ascii() { format!("{}…{}", &a[..8], &a[a.len() - 6..]) } else { a.to_string() }
}

/// 64 characters as two rows of four groups of eight, the way people compare them.
pub(crate) fn rows_of(addr: &str) -> [String; 2] {
    let g: Vec<String> = addr.as_bytes().chunks(8).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    let half = g.len().div_ceil(2);
    [g[..half].join(" "), g[half..].join(" ")]
}

/// Whose wallet the address is, as the screen says it.
pub(crate) fn owner_text(custom: bool) -> &'static str {
    if custom { "the address you chose" } else { "this computer's own wallet" }
}

fn secret_key_text(opens: &str) -> String {
    format!("STOP: that is the SECRET KEY of wallet {}, not its address. It was not saved or sent. To mine into that wallet, paste its address.", short(opens))
}

fn paste_hint() -> &'static str {
    if cfg!(windows) { "paste: right-click or Ctrl+V" } else if cfg!(target_os = "macos") { "paste: Cmd+V" } else { "paste: right-click or Ctrl+Shift+V" }
}

// ── the [P] screen ───────────────────────────────────────────────────────────────────────

pub(crate) enum Stage {
    Edit,
    Checking { addr: String, rx: mpsc::Receiver<Check>, since: Instant },
    Confirm { addr: String, check: Check, since: Instant },
}

pub(crate) struct PayoutUi {
    pub stage: Stage,
    pub input: String,
    /// A line under the box: what was wrong, or what to do.
    pub note: Option<String>,
}

impl PayoutUi {
    pub(crate) fn new() -> Self {
        PayoutUi { stage: Stage::Edit, input: String::new(), note: None }
    }
}

/// What the screen asks its owner (the App) to do.
#[derive(Debug, PartialEq)]
pub(crate) enum Act {
    None,
    Close,
    /// Save this address and mine to it.
    Use(String),
    /// Mine to this computer's own wallet again.
    UseOwn,
    /// Ask the chain about this address.
    Check(String),
    /// Put the clipboard's text into the box (Windows consoles that hand Ctrl+V to the program).
    Paste,
}

impl PayoutUi {
    /// One key. PURE apart from its own state: the App carries out the returned [`Act`].
    pub(crate) fn key(&mut self, k: KeyEvent, now: Instant) -> Act {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT);
        match &self.stage {
            Stage::Edit => match k.code {
                KeyCode::Esc => return Act::Close,
                KeyCode::Enter => return self.submit(),
                KeyCode::Backspace => { self.input.pop(); self.note = None; }
                KeyCode::Delete => { self.input.clear(); self.note = None; }
                KeyCode::Char('u') | KeyCode::Char('U') if ctrl => { self.input.clear(); self.note = None; }
                KeyCode::Char('v') | KeyCode::Char('V') if ctrl => return Act::Paste,
                KeyCode::Char('c') | KeyCode::Char('C') if ctrl => return Act::Close,
                KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
                _ => {}
            },
            Stage::Checking { .. } => {
                if matches!(k.code, KeyCode::Esc) {
                    self.stage = Stage::Edit;
                    self.note = Some("Stopped. Nothing was saved.".into());
                }
            }
            Stage::Confirm { addr, since, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl && now.saturating_duration_since(*since) >= CONFIRM_GUARD => {
                    return Act::Use(addr.clone());
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Backspace => {
                    self.stage = Stage::Edit;
                    self.note = Some("Nothing was saved. Paste another address, or Esc to close.".into());
                }
                _ => {}
            },
        }
        Act::None
    }

    /// Typed or pasted text goes into the box (control characters and anything past the limit do not).
    pub(crate) fn insert(&mut self, text: &str) {
        for c in text.chars() {
            if self.input.chars().count() >= INPUT_MAX {
                self.note = Some("That is too long for an address. Press Delete to clear the box.".into());
                return;
            }
            if c.is_control() {
                // a wrapped address arrives with a line break in it: keep the two halves apart, parse joins them
                if matches!(c, '\r' | '\n' | '\t') && !self.input.is_empty() { self.input.push(' '); }
            } else {
                self.input.push(c);
            }
        }
        self.note = None;
    }

    fn submit(&mut self) -> Act {
        if matches!(self.input.trim().to_ascii_lowercase().as_str(), "own" | "mine" | "reset") {
            return Act::UseOwn;
        }
        match parse_address(&self.input) {
            Ok(addr) => Act::Check(addr),
            Err(why) => {
                // Never leave somebody's recovery words on the screen.
                if looks_like_words(&self.input) { self.input.clear(); }
                self.note = Some(why);
                Act::None
            }
        }
    }

    /// The chain's answer arrived (or the check is still running).
    pub(crate) fn poll(&mut self, now: Instant) {
        let Stage::Checking { addr, rx, since } = &self.stage else { return };
        let got = match rx.try_recv() {
            Ok(c) => c,
            Err(mpsc::TryRecvError::Empty) if now.saturating_duration_since(*since) < Duration::from_secs(45) => return,
            Err(_) => Check::NoAnswer,
        };
        let addr = addr.clone();
        match got {
            Check::SecretKeyOf(opens) => {
                self.input.clear();
                self.note = Some(secret_key_text(&opens));
                self.stage = Stage::Edit;
            }
            Check::NoAnswer => {
                self.note = Some("The network did not answer, so the address could not be checked. Nothing was saved. Press Enter to try again.".into());
                self.stage = Stage::Edit;
            }
            check => self.stage = Stage::Confirm { addr, check, since: now },
        }
    }
}

/// [P]: open the screen.
pub(crate) fn open(app: &mut App) {
    app.payout_ui = Some(PayoutUi::new());
}

/// One key while the screen is open. Returns true when it was an Enter: the rest of that paste is dropped.
pub(crate) fn on_key(app: &mut App, k: KeyEvent) -> bool {
    let Some(ui) = app.payout_ui.as_mut() else { return false };
    match ui.key(k, Instant::now()) {
        Act::None => {}
        Act::Close => { app.payout_ui = None; }
        Act::Paste => match clipboard_text() {
            Some(t) => ui.insert(&t),
            None => ui.note = Some(format!("Could not read the clipboard here ({}).", paste_hint())),
        },
        Act::Check(addr) => {
            if chosen().is_none() && addr == crate::miner_wallet() {
                ui.note = Some("That is this computer's own wallet: your coins already go there.".into());
            } else {
                let (tx, rx) = mpsc::channel();
                let a = addr.clone();
                std::thread::spawn(move || { let _ = tx.send(check(&a)); });
                ui.note = None;
                ui.stage = Stage::Checking { addr, rx, since: Instant::now() };
            }
        }
        Act::Use(addr) => {
            // Pasting this computer's own address means "go back to it".
            let res = if addr == crate::miner_wallet() { clear_in(&crate::flux_home()).map(|_| ()) } else { save_in(&crate::flux_home(), &addr) };
            finish(app, res);
        }
        Act::UseOwn => {
            let res = clear_in(&crate::flux_home()).map(|_| ());
            finish(app, res);
        }
    }
    k.code == KeyCode::Enter
}

/// Saved: tell the user, and move a running miner over at once.
fn finish(app: &mut App, res: Result<(), String>) {
    match res {
        Ok(()) => {
            app.payout_ui = None;
            let (w, custom) = (mining_wallet(), chosen().is_some());
            let moved = app.repoint_mining();
            let tail = if moved { "Mining continues on it now." } else { "Press M to start mining." };
            app.toast = format!("✓ Your mined coins go to {} ({}). {tail}", short(&w), owner_text(custom));
            app.toast_sticky = true;
            crate::boot_trace(&format!("payout address set to {} (custom={custom}, miner moved={moved})", short(&w)));
        }
        Err(e) => {
            if let Some(ui) = app.payout_ui.as_mut() {
                ui.stage = Stage::Edit;
                ui.note = Some(format!("Could not save it: {e}"));
            }
        }
    }
}

/// Draw the screen on top of everything, when it is open.
pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(ui) = app.payout_ui.as_ref() else { return };
    let custom = chosen();
    let now_addr = custom.clone().unwrap_or_else(crate::miner_wallet);
    let w = 76u16.min(area.width.saturating_sub(2)).max(20.min(area.width));
    let lines = lines_for(ui, &now_addr, custom.is_some(), w.saturating_sub(4) as usize);
    let h = (lines.len() as u16 + 2).min(area.height);
    let modal = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 2, width: w, height: h };
    f.render_widget(Clear, modal);
    let card = card_block(" ⛏ WHERE YOUR MINED COINS GO", C_NEON_GOLD)
        .border_style(Style::default().fg(C_NEON_GOLD))
        .style(Style::default().bg(C_BG));
    f.render_widget(Paragraph::new(lines).block(card), modal);
}

fn bold(t: impl Into<String>, c: Color) -> Span<'static> {
    Span::styled(sa(t.into()), Style::default().fg(c).add_modifier(Modifier::BOLD))
}

fn wrapped(t: &str, width: usize, c: Color) -> Vec<Line<'static>> {
    let mut rows: Vec<String> = vec![String::new()];
    for word in t.split_whitespace() {
        let last = rows.last_mut().expect("never empty");
        if !last.is_empty() && last.chars().count() + 1 + word.chars().count() > width.max(8) {
            rows.push(word.to_string());
        } else {
            if !last.is_empty() { last.push(' '); }
            last.push_str(word);
        }
    }
    rows.into_iter().map(|r| Line::from(Span::styled(sa(r), Style::default().fg(c)))).collect()
}

/// The text of the screen (pure, so the tests read what a user reads).
fn lines_for(ui: &PayoutUi, now_addr: &str, custom: bool, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let addr_rows = |a: &str, c: Color| -> Vec<Line<'static>> {
        rows_of(a).into_iter().map(|r| Line::from(vec![Span::raw("   "), bold(r, c)])).collect()
    };
    match &ui.stage {
        Stage::Edit | Stage::Checking { .. } => {
            out.push(Line::from(vec![dim("Now: "), bold(owner_text(custom), C_VBRIGHT)]));
            out.extend(addr_rows(now_addr, C_NEON_CYAN));
            out.push(Line::from(""));
            out.extend(wrapped(&format!("Mine into another wallet (phone app, sigilgraph.org): open Receive there, copy the address, put it here ({}).", paste_hint()), width, C_DIM));
            out.push(Line::from(""));
            let n = ui.input.chars().count();
            let secret = looks_like_words(&ui.input);
            let room = width.saturating_sub(12).max(8);
            let shown: String = if secret { "•".repeat(n.min(room)) } else { ui.input.chars().skip(n.saturating_sub(room)).collect() };
            out.push(Line::from(vec![
                bold(" > ", C_NEON_GOLD),
                Span::styled(sa(shown), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                bold("█", C_NEON_GOLD),
                dim(&if n == 0 { String::new() } else { format!("  {n}") }),
            ]));
            out.push(Line::from(""));
            if matches!(ui.stage, Stage::Checking { .. }) {
                out.extend(wrapped("Asking the network about this address…", width, C_GOLD));
            } else if let Some(n) = &ui.note {
                out.extend(wrapped(n, width, if n.starts_with("STOP") { C_RED } else { C_GOLD }));
            } else {
                out.extend(wrapped("Only the address. Never your recovery words or secret key.", width, C_DIM));
            }
            out.push(Line::from(""));
            out.push(Line::from(vec![bold("Enter", C_NEON_GOLD), dim(" check it   "), bold("Delete", C_NEON_GOLD), dim(" clear   "), bold("Esc", C_NEON_GOLD), dim(" close")]));
            if custom {
                out.push(Line::from(vec![dim("type "), bold("own", C_NEON_GOLD), dim(" + Enter = back to this computer's own wallet")]));
            }
        }
        Stage::Confirm { addr, check, .. } => {
            out.push(Line::from(vec![dim("Send your mined coins to:")]));
            out.extend(addr_rows(addr, C_NEON_GREEN));
            out.push(Line::from(""));
            match check {
                Check::Known(what) => out.extend(wrapped(&format!("✓ This wallet is on the chain: {what}."), width, C_NEON_GREEN)),
                _ => out.extend(wrapped("⚠ Nothing has ever happened on this address. That is normal for a new wallet, but a wrong character looks the same. Compare it.", width, C_GOLD)),
            }
            out.push(Line::from(""));
            out.extend(wrapped("Compare it with your wallet: coins paid to a wrong address cannot be brought back. [W] keeps showing this computer's own wallet; see these coins where that wallet lives.", width, C_DIM));
            out.push(Line::from(""));
            out.push(Line::from(vec![bold("Y", C_NEON_GOLD), dim(" yes, mine to it   "), bold("N", C_NEON_GOLD), dim(" no, change it")]));
        }
    }
    out
}

// ── the same thing without the screen ────────────────────────────────────────────────────

/// `sigil-top wallet mine-to [<address> | own]` and the first-run question. Returns true when saved.
pub(crate) fn run_cli(arg: Option<&str>, ask: impl Fn(&str) -> String) -> bool {
    let home = crate::flux_home();
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        let custom = load_in(&home);
        println!("\n  mined coins go to  {}  ({})", custom.clone().unwrap_or_else(crate::miner_wallet), owner_text(custom.is_some()));
        println!("  change it:         sigil-top wallet mine-to <address>   (or press P in the dashboard)");
        println!("  your own wallet:   sigil-top wallet mine-to own\n");
        return false;
    };
    if arg.eq_ignore_ascii_case("own") {
        return match clear_in(&home) {
            Ok(_) => { println!("  ✓ mined coins go to this computer's own wallet {}", crate::miner_wallet()); true }
            Err(e) => { eprintln!("  ✗ {e}"); false }
        };
    }
    let addr = match parse_address(arg) {
        Ok(a) => a,
        Err(e) => { eprintln!("  ✗ {e}"); return false; }
    };
    let new = match check(&addr) {
        Check::SecretKeyOf(opens) => { eprintln!("  ✗ {}", secret_key_text(&opens)); return false; }
        Check::NoAnswer => { eprintln!("  ✗ the network did not answer, so the address could not be checked. Nothing was saved."); return false; }
        Check::Known(what) => { println!("  ✓ this wallet is on the chain: {what}"); false }
        Check::New => { println!("  ⚠ nothing has ever happened on this address — normal for a new wallet, but a typo looks the same."); true }
    };
    let [a, b] = rows_of(&addr);
    println!("\n     {a}\n     {b}\n");
    if new && ask("  Mine to it? Type yes: ") != "yes" {
        eprintln!("  not saved — nothing changed");
        return false;
    }
    match save_in(&home, &addr) {
        Ok(()) => { println!("  ✓ mined coins go to {addr}\n    (a running dashboard picks it up when mining is restarted: press M twice)"); true }
        Err(e) => { eprintln!("  ✗ {e}"); false }
    }
}

// ── clipboard ────────────────────────────────────────────────────────────────────────────

/// The classic Windows console hands Ctrl+V to the program instead of pasting, so read the clipboard here.
#[cfg(windows)]
fn clipboard_text() -> Option<String> {
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> isize;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(mem: isize) -> *const u16;
        fn GlobalUnlock(mem: isize) -> i32;
        fn GlobalSize(mem: isize) -> usize;
    }
    const CF_UNICODETEXT: u32 = 13;
    // SAFETY: the handle belongs to the clipboard while it is open; it is locked before reading, the read is
    // bounded by GlobalSize (and INPUT_MAX), and everything is unlocked and closed on every path.
    unsafe {
        if OpenClipboard(0) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(CF_UNICODETEXT);
        if h != 0 {
            let p = GlobalLock(h);
            if !p.is_null() {
                let max = (GlobalSize(h) / 2).min(INPUT_MAX + 1);
                let mut n = 0;
                while n < max && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        out
    }
}

/// Elsewhere the terminal pastes by itself (the text arrives as typing).
#[cfg(not(windows))]
fn clipboard_text() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"; // public key of seed 07…07
    const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    fn tmp_home(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("sigil-payout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }
    fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }
    fn type_in(ui: &mut PayoutUi, t: &str) { for c in t.chars() { ui.key(key(KeyCode::Char(c)), Instant::now()); } }
    fn text(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn every_way_people_copy_an_address_is_understood() {
        let e = "ab".repeat(32);
        for raw in [
            ADDR.to_string(),
            ADDR.to_uppercase(),
            format!("  {ADDR}\r\n"),
            format!("\"{ADDR}\""),
            format!("\u{feff}{ADDR}\u{200b}"),
            format!("sigil:{ADDR}"),
            format!("sigil://{ADDR}"),
            format!("sigil:{ADDR}?s={e}&e={e}"),
            format!("SIGIL:{}?amount=5", ADDR.to_uppercase()),
            format!("https://sigilgraph.org/sigil-wallet-tron-embedded.html?addr={ADDR}"),
            format!("http://localhost:9800/?addr={ADDR}#stats"),
            format!("{} {}", &ADDR[..32], &ADDR[32..]), // wrapped over two lines
            format!("{}\n{}", &ADDR[..40], &ADDR[40..]),
        ] {
            assert_eq!(parse_address(&raw).as_deref(), Ok(ADDR), "{raw:?}");
        }
    }

    #[test]
    fn what_is_not_an_address_is_refused_in_plain_words() {
        for (raw, want) in [
            ("", "Paste or type"),
            ("abandon ability able about above absent absorb abstract absurd abuse access accident", "recovery words"),
            ("bad cafe abandon ability able about", "recovery words"),
            ("sigil:coin?v=2&k=0707&n=sigil-g3", "coin link"),
            ("https://sigilgraph.org/claim.html#v=2&k=07", "coin link"),
            ("sigil1s:aa:bb", "no public address"),
            ("qnk22ac7dd63164c4efe5311364a9ca6bff4fbf8fb92fe73f61c53a1f8bfaba56b0", "Quillon"),
            ("0x7e9C000000000000000000000000000000000000", "Ethereum"),
            ("https://sigilgraph.org/", "no wallet address"),
            ("ea4a6c63", "exactly 64 characters; this is 8"),
            ("ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22g", "has \"g\""),
        ] {
            let got = parse_address(raw).expect_err(raw);
            assert!(got.contains(want), "{raw:?} → {got}");
        }
        let eth_key = format!("0x{SEED}");
        assert!(parse_address(&eth_key).unwrap_err().contains("secret key"));
    }

    /// One wrong character usually leaves the curve: count how many of the 960 one-character typos of this
    /// address are refused before any question is asked. (About half, by the mathematics of the curve.)
    #[test]
    fn about_half_of_all_single_typos_are_caught_without_the_network() {
        let (mut refused, mut all) = (0, 0);
        for i in 0..64 {
            for c in "0123456789abcdef".chars() {
                if ADDR.as_bytes()[i] as char == c { continue; }
                let mut s: Vec<char> = ADDR.chars().collect();
                s[i] = c;
                all += 1;
                refused += parse_address(&s.into_iter().collect::<String>()).is_err() as usize;
            }
        }
        assert_eq!(all, 960);
        assert!((350..=610).contains(&refused), "refused {refused} of {all}");
    }

    /// The safety property: a secret key is recognised and NEVER asked about as an address.
    #[test]
    fn a_secret_key_is_recognised_and_never_sent() {
        // a key that is also a valid curve point, so only the chain question can tell
        let mut secret = [9u8; 32];
        while !sigil_oauth::is_wallet_key(&secret) { secret[0] = secret[0].wrapping_add(1); }
        let pasted = hex::encode(secret);
        let opens = sigil_oauth::Keypair::from_seed(&secret).pubkey_hex();
        assert_eq!(parse_address(&pasted).as_deref(), Ok(pasted.as_str()), "shape alone cannot tell");
        let mut asked = Vec::new();
        let got = check_with(&pasted, None, |a| {
            asked.push(a.to_string());
            Seen { balance: Some(0), registered: Some(a == opens) }
        });
        assert_eq!(got, Check::SecretKeyOf(opens.clone()));
        assert_eq!(asked, vec![opens.clone()], "only the derived public address may leave this computer");
        // this computer's own key: refused without any question
        let got = check_with(&pasted, Some(secret), |_| panic!("must not ask"));
        assert_eq!(got, Check::SecretKeyOf(opens));
        // no answer about the derived address → the paste is not sent either
        let mut n = 0;
        assert_eq!(check_with(&pasted, None, |_| { n += 1; Seen::default() }), Check::NoAnswer);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_real_address_is_known_or_new() {
        let none = Seen { balance: Some(0), registered: Some(false) };
        let me = Seen { balance: Some(123_400_000_000), registered: Some(true) };
        let got = check_with(ADDR, None, |a| if a == ADDR { me.clone() } else { none.clone() });
        let Check::Known(t) = got else { panic!("{got:?}") };
        assert!(t.contains("12.34") && t.contains("privately"), "{t}");
        assert_eq!(check_with(ADDR, None, |_| none.clone()), Check::New);
        // half an answer is not "never used"
        let got = check_with(ADDR, None, |a| if a == ADDR { Seen { balance: Some(0), registered: None } } else { none.clone() });
        assert_eq!(got, Check::NoAnswer);
    }

    #[test]
    fn the_registry_answer_is_read_exactly() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(parse_registered(&j(r#"{"ok":true,"pk_shield":"aa"}"#)), Some(true));
        assert_eq!(parse_registered(&j(r#"{"error":"wallet has not registered a shielded address","ok":false,"registration":null}"#)), Some(false));
        assert_eq!(parse_registered(&j(r#"{"ok":false,"error":"busy"}"#)), None, "an error is not \"unregistered\"");
        assert_eq!(parse_registered(&j("[]")), None);
    }

    #[test]
    fn saved_read_back_and_undone() {
        let h = tmp_home("save");
        assert_eq!(load_in(&h), None);
        assert_eq!(clear_in(&h), Ok(false));
        save_in(&h, &ADDR.to_uppercase()).unwrap();
        assert_eq!(load_in(&h).as_deref(), Some(ADDR));
        assert!(save_in(&h, "nonsense").is_err());
        assert_eq!(load_in(&h).as_deref(), Some(ADDR), "a refused save leaves the old address");
        std::fs::write(path_in(&h), "half an addre").unwrap();
        assert_eq!(load_in(&h), None, "a damaged file falls back to this computer's own wallet");
        assert_eq!(clear_in(&h), Ok(true));
        assert!(!std::path::Path::new(&format!("{}.new", path_in(&h))).exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn typing_enter_and_the_confirm_question() {
        let mut ui = PayoutUi::new();
        assert_eq!(ui.key(key(KeyCode::Enter), Instant::now()), Act::None);
        assert!(ui.note.as_deref().unwrap().contains("Paste or type"));
        type_in(&mut ui, ADDR);
        assert_eq!(ui.key(key(KeyCode::Enter), Instant::now()), Act::Check(ADDR.into()));

        let t0 = Instant::now();
        ui.stage = Stage::Confirm { addr: ADDR.into(), check: Check::New, since: t0 };
        assert_eq!(ui.key(key(KeyCode::Enter), t0 + Duration::from_secs(5)), Act::None, "Enter never confirms");
        assert_eq!(ui.key(key(KeyCode::Char('y')), t0 + Duration::from_millis(50)), Act::None, "a pasted y never confirms");
        assert_eq!(ui.key(key(KeyCode::Char('y')), t0 + Duration::from_secs(1)), Act::Use(ADDR.into()));
        assert_eq!(ui.key(key(KeyCode::Char('n')), t0 + Duration::from_secs(1)), Act::None);
        assert!(matches!(ui.stage, Stage::Edit));
        assert_eq!(ui.key(key(KeyCode::Esc), t0), Act::Close);
    }

    #[test]
    fn letters_that_are_shortcuts_are_text_here() {
        let mut ui = PayoutUi::new();
        type_in(&mut ui, "qQfFmMyYuU");
        assert_eq!(ui.input, "qQfFmMyYuU");
        assert_eq!(ui.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL), Instant::now()), Act::Paste);
        ui.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), Instant::now());
        assert_eq!(ui.input, "");
        type_in(&mut ui, "Own");
        assert_eq!(ui.key(key(KeyCode::Enter), Instant::now()), Act::UseOwn);
    }

    #[test]
    fn recovery_words_are_masked_and_wiped() {
        let mut ui = PayoutUi::new();
        type_in(&mut ui, "abandon ability able about above absent");
        let seen = text(&lines_for(&ui, ADDR, false, 72));
        assert!(!seen.contains("abandon") && !seen.contains("absent"), "{seen}");
        assert_eq!(ui.key(key(KeyCode::Enter), Instant::now()), Act::None);
        assert_eq!(ui.input, "", "the words do not stay in the box");
        assert!(ui.note.as_deref().unwrap().contains("secret"));
    }

    #[test]
    fn a_long_paste_is_bounded_and_a_wrapped_one_joined() {
        let mut ui = PayoutUi::new();
        ui.insert(&"a".repeat(5000));
        assert_eq!(ui.input.chars().count(), INPUT_MAX);
        let mut ui = PayoutUi::new();
        ui.insert(&format!("{}\r\n{}\r\n", &ADDR[..40], &ADDR[40..]));
        assert_eq!(ui.key(key(KeyCode::Enter), Instant::now()), Act::Check(ADDR.into()));
    }

    #[test]
    fn the_answer_moves_the_screen_on() {
        let t0 = Instant::now();
        let run = |c: Check| {
            let (tx, rx) = mpsc::channel();
            let mut ui = PayoutUi { stage: Stage::Checking { addr: ADDR.into(), rx, since: t0 }, input: ADDR.into(), note: None };
            ui.poll(t0);
            assert!(matches!(ui.stage, Stage::Checking { .. }), "no answer yet");
            tx.send(c).unwrap();
            ui.poll(t0);
            ui
        };
        assert!(matches!(run(Check::New).stage, Stage::Confirm { .. }));
        assert!(matches!(run(Check::Known("x".into())).stage, Stage::Confirm { .. }));
        let ui = run(Check::SecretKeyOf(ADDR.into()));
        assert!(matches!(ui.stage, Stage::Edit) && ui.input.is_empty());
        assert!(ui.note.as_deref().unwrap().starts_with("STOP"));
        let ui = run(Check::NoAnswer);
        assert!(matches!(ui.stage, Stage::Edit) && ui.input == ADDR, "kept, so Enter tries again");
        // a checker that died counts as no answer
        let (tx, rx) = mpsc::channel::<Check>();
        drop(tx);
        let mut ui = PayoutUi { stage: Stage::Checking { addr: ADDR.into(), rx, since: t0 }, input: ADDR.into(), note: None };
        ui.poll(t0);
        assert!(matches!(ui.stage, Stage::Edit));
    }

    #[test]
    fn the_screen_shows_the_whole_address_in_groups() {
        let [a, b] = rows_of(ADDR);
        assert_eq!(a, "ea4a6c63 e29c520a bef5507b 132ec5f9");
        assert_eq!(b, "954776ae bebe7b92 421eea69 1446d22c");
        let ui = PayoutUi { stage: Stage::Confirm { addr: ADDR.into(), check: Check::New, since: Instant::now() }, input: String::new(), note: None };
        let t = text(&lines_for(&ui, ADDR, false, 72));
        assert!(t.contains(&a) && t.contains(&b) && t.replace('\n', " ").contains("cannot be brought back"), "{t}");
        // fits a 24-row terminal, and every row fits the box
        for (w, ui) in [(72, &ui), (72, &PayoutUi::new())] {
            let l = lines_for(ui, ADDR, true, w);
            assert!(l.len() + 2 <= 24, "{} rows", l.len());
            assert!(l.iter().all(|x| x.width() <= w), "{}", text(&l));
        }
    }
}
