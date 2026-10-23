//! ai_tools — the SIGIL wallet MCP surface as tools the [A]I tab's model can actually USE.
//!
//! Before this module the model could only talk; asked to "load the wallet MCP" it (rightly)
//! refused to run a pasted `curl | bash`. The tools are now built in: sigil-top itself runs
//! them against the node, so nothing is installed from a chat message.
//!
//! Three tiers, and the tier is the safety model:
//! * READ (balance, height/supply, pools, shielded pool, mining, nation) — run at once,
//!   the JSON goes back to the model, a one-line receipt goes in the transcript.
//! * PROPOSE (private send) — validated, shown to the user with the FULL address; never
//!   executed here. SIGIL sends are shielded and are signed in the wallet [W].
//! * ACTION (create wallet, start/stop mining) — the loop STOPS and the user must type
//!   `yes`. The model never sees a seed: a created wallet's seed goes to a 0600 file.
//!
//! Guards that do not trust the model (measured 2026-10-02 on qwen3:8b: it shortened a
//! 64-hex address to 60, and invented "bob's wallet address" + 100 SIGIL): every address is
//! checked for shape here, and a malformed one is returned to the model as an error telling
//! it to ask the user — it is never "fixed up".

use crate::flux_moe::{self, StreamEv};
use serde_json::{json, Value};
use std::sync::mpsc::Sender;

const GLYPHS_PER_SIGIL: f64 = 1e10;
const MAX_STEPS: usize = 4;

fn tool(name: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":desc,
        "parameters":{"type":"object","properties":props,"required":req}}})
}

/// The tool list sent to the model.
pub(crate) fn tool_defs() -> Value {
    let w = json!({"type":"string","description":"64-hex SIGIL wallet id. Omit to use this node's own wallet."});
    json!([
        tool("sigil_balance", "Live transparent balance of a SIGIL wallet, in SIGIL.", json!({"wallet": w}), &[]),
        tool("sigil_network_status", "Live SIGIL chain height, native supply and node health.", json!({}), &[]),
        tool("sigil_pools", "Live DEX liquidity pools on SIGIL.", json!({}), &[]),
        tool("sigil_shielded_status", "Live shielded pool: note count (the real privacy number), nullifiers, capacity.", json!({}), &[]),
        tool("sigil_mining_network", "Live mining network: live miners, network hashrate, blocks and shares accepted.", json!({}), &[]),
        tool("sigil_nation_status", "Live SIGIL Nation: welfare treasury, stipend, claim interval.", json!({}), &[]),
        tool("sigil_my_wallet", "This node's own wallet address and whether this node is mining.", json!({}), &[]),
        tool("sigil_propose_private_send",
            "PROPOSE a private SIGIL payment. Does not move money — the user signs it in the wallet. \
             Only call when the user gave BOTH a full recipient address and an amount; otherwise ask.",
            json!({"to":{"type":"string","description":"64-hex wallet or sigil1s:… shielded address, exactly as the user wrote it"},
                   "amount":{"type":"number","description":"amount in SIGIL, as the user said it"},
                   "memo":{"type":"string","description":"optional private memo"}}), &["to","amount"]),
        tool("sigil_create_wallet", "ACTION: create a new SIGIL wallet on this node (the user confirms; the seed is saved to a private file, never shown to you).", json!({}), &[]),
        tool("sigil_start_mining", "ACTION: start mining SIGIL on this node to this node's wallet (the user confirms).", json!({}), &[]),
        tool("sigil_stop_mining", "ACTION: stop mining on this node (the user confirms).", json!({}), &[]),
    ])
}

pub(crate) const TOOLS_SYSTEM: &str = "\n\nTOOLS: you can call SIGIL wallet tools. Use one whenever the user needs a LIVE number \
(balance, height, supply, pools, mining, privacy pool). Never invent a number. If the user's request is missing a \
wallet address or an amount, ASK — never guess, never use a placeholder. Copy addresses exactly, every character. \
Actions (create wallet, start/stop mining) are confirmed by the user, so just call them when asked. Never ask for, \
accept or repeat a seed or secret key.";

// ── Scopes: least privilege for the AI, the GrapheneOS way ─────────────────
//
// GrapheneOS's strength is that an app only gets the scopes the user grants. Here
// the "app" is the model: ON this device it may use every tool (nothing leaves);
// OFF this device it gets only network-wide facts by default. Tool RESULTS go back
// to an off-box model with identifiers hidden — but numbers are not identifiers, so
// a remote model calling sigil_balance would learn the user's wealth. Wallet,
// payment and action tools must be granted: SIGIL_AI_REMOTE_SCOPES=wallet,payments,actions.

/// What a tool exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Chain-wide public facts: height, supply, pools, pool size, mining network, nation.
    Network,
    /// The user's own wallet: address, balance.
    Wallet,
    /// Proposing a payment (who the user pays, how much).
    Payments,
    /// Changing this node: create a wallet, start/stop mining.
    Actions,
}

pub(crate) fn scope_of(tool: &str) -> Scope {
    match tool {
        "sigil_network_status" | "sigil_pools" | "sigil_shielded_status" | "sigil_mining_network" | "sigil_nation_status" => Scope::Network,
        "sigil_propose_private_send" => Scope::Payments,
        t if is_action(t) => Scope::Actions,
        // sigil_balance, sigil_my_wallet — and anything unknown defaults to the
        // personal scope (deny-by-default for a tool nobody classified)
        _ => Scope::Wallet,
    }
}

/// Pure: the scopes granted for a model on (`remote == false`) or off this device.
pub(crate) fn granted_scopes(remote: bool, remote_scopes_env: Option<&str>) -> Vec<Scope> {
    if !remote {
        return vec![Scope::Network, Scope::Wallet, Scope::Payments, Scope::Actions];
    }
    let mut g = vec![Scope::Network];
    for part in remote_scopes_env.unwrap_or("").split(',').map(|x| x.trim().to_ascii_lowercase()) {
        let sc = match part.as_str() {
            "wallet" => Some(Scope::Wallet),
            "payments" | "payment" => Some(Scope::Payments),
            "actions" | "action" => Some(Scope::Actions),
            _ => None,
        };
        if let Some(sc) = sc { if !g.contains(&sc) { g.push(sc); } }
    }
    g
}

/// The tool list offered to the model, filtered to what is granted.
pub(crate) fn tool_defs_for(granted: &[Scope]) -> Value {
    let all = tool_defs();
    let kept: Vec<Value> = all.as_array().cloned().unwrap_or_default().into_iter()
        .filter(|t| t.pointer("/function/name").and_then(|n| n.as_str()).map(|n| granted.contains(&scope_of(n))).unwrap_or(false))
        .collect();
    Value::Array(kept)
}

pub(crate) fn scope_label(s: Scope) -> &'static str {
    match s { Scope::Network => "network", Scope::Wallet => "wallet", Scope::Payments => "payments", Scope::Actions => "actions" }
}

pub(crate) fn is_action(name: &str) -> bool {
    matches!(name, "sigil_create_wallet" | "sigil_start_mining" | "sigil_stop_mining")
}

/// Pure: a 64-hex wallet id (the shape the node keys balances by).
pub(crate) fn is_wallet(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) }

/// Pure: `sigil1s:<64hex>:<64hex>` shielded address.
pub(crate) fn is_shielded_addr(s: &str) -> bool {
    let mut it = s.strip_prefix("sigil1s:").map(|r| r.split(':'));
    match it.as_mut() {
        Some(p) => matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), None) if is_wallet(a) && is_wallet(b)),
        None => false,
    }
}

fn short(a: &str) -> String { if a.len() > 16 { format!("{}…{}", &a[..8], &a[a.len() - 4..]) } else { a.to_string() } }

/// The mining wallet's PUBLIC balance, polled every 30 s on a background thread for the dashboard
/// card (2026-10-19: that line read 0.00 for everyone — nothing ever filled it). Same node as [W]'s
/// proxy. On g3 block rewards are public, so this is the balance a miner earns into.
static POLLED_BAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static POLLED_BAL_KNOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static POLLER: std::sync::Once = std::sync::Once::new();

pub(crate) fn start_balance_poller() {
    use std::sync::atomic::Ordering;
    POLLER.call_once(|| {
        std::thread::spawn(|| loop {
            // 2026-10-23: your own node when it is caught up, the network otherwise.
            let base = crate::top_core::wallet_read_url("http://sigilgraph.quillon.xyz:18181");
            let wallet = crate::miner_wallet();
            if is_wallet(&wallet) {
                if let Ok(j) = get(base.trim_end_matches('/'), &format!("/v1/balance?wallet={wallet}")) {
                    if let Some(g) = j.pointer("/data/balance").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()) {
                        POLLED_BAL.store(g, Ordering::Relaxed);
                        POLLED_BAL_KNOWN.store(true, Ordering::Relaxed);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        // 2026-10-23: and LISTEN — the node pushes this wallet's balance the instant it moves.
        std::thread::spawn(balance_stream);
    });
}

/// Hold `/v1/events?kinds=balance&wallet=<mining wallet>` open and store each pushed balance in [`POLLED_BAL`].
/// Reconnects with a growing pause (2 s … 60 s), and for a new wallet when the mining wallet changes (checked at most
/// every 15 s — reading it opens a file and derives a key; the node sends a keep-alive line every 15 s).
fn balance_stream() {
    use std::io::BufRead;
    use std::sync::atomic::Ordering;
    let Ok(client) = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(None::<std::time::Duration>)
        .build()
    else {
        return;
    };
    let mut pause = 2u64;
    loop {
        let base = crate::top_core::wallet_read_url("http://sigilgraph.quillon.xyz:18181");
        let wallet = crate::miner_wallet();
        if is_wallet(&wallet) {
            let url = format!("{}/v1/events?kinds=balance&wallet={wallet}", base.trim_end_matches('/'));
            if let Ok(resp) = client.get(&url).header("Accept", "text/event-stream").send() {
                if resp.status().is_success() {
                    pause = 2;
                    let mut checked = std::time::Instant::now();
                    for line in std::io::BufReader::new(resp).lines() {
                        let Ok(line) = line else { break };
                        if let Some(g) = line.strip_prefix("data:").and_then(|d| pushed_balance(d.trim(), &wallet)) {
                            POLLED_BAL.store(g, Ordering::Relaxed);
                            POLLED_BAL_KNOWN.store(true, Ordering::Relaxed);
                        }
                        if checked.elapsed() >= std::time::Duration::from_secs(15) {
                            checked = std::time::Instant::now();
                            if crate::miner_wallet() != wallet
                                || crate::top_core::wallet_read_url("http://sigilgraph.quillon.xyz:18181") != base
                            {
                                break; // another wallet, or your node caught up / fell behind: reconnect there
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(pause));
        pause = (pause * 2).min(60);
    }
}

/// The native balance in one pushed `balance` event for `wallet`, if that is what it is.
fn pushed_balance(data: &str, wallet: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(data).ok()?;
    let native = "0".repeat(64);
    (v["kind"].as_str() == Some("balance")
        && v["wallet"].as_str().map(|w| w.eq_ignore_ascii_case(wallet)) == Some(true)
        && v["token"].as_str() == Some(native.as_str()))
    .then(|| v["balance"].as_str()?.parse::<u64>().ok())
    .flatten()
}

#[cfg(test)]
mod balance_push_tests {
    use super::pushed_balance;

    #[test]
    fn a_pushed_balance_for_our_wallet_is_taken_and_nothing_else_is() {
        let me = "ab".repeat(32);
        let ev = |kind: &str, w: &str, token: &str, bal: &str| {
            serde_json::json!({"seq": 3, "kind": kind, "wallet": w, "token": token, "balance": bal, "height": 9, "ts_ms": 0}).to_string()
        };
        let native = "0".repeat(64);
        assert_eq!(pushed_balance(&ev("balance", &me, &native, "1234500000"), &me), Some(1_234_500_000));
        assert_eq!(pushed_balance(&ev("balance", &me.to_uppercase(), &native, "7"), &me), Some(7), "hex case does not matter");
        assert_eq!(pushed_balance(&ev("balance", &"cd".repeat(32), &native, "7"), &me), None, "another wallet");
        assert_eq!(pushed_balance(&ev("balance", &me, &"11".repeat(32), "7"), &me), None, "another token");
        assert_eq!(pushed_balance(&ev("note", &me, &native, "7"), &me), None, "another kind");
        assert_eq!(pushed_balance("not json", &me), None);
    }
}

/// `None` until the node has answered once — the card then shows "—", never a false 0.00.
pub(crate) fn polled_balance() -> Option<u128> {
    use std::sync::atomic::Ordering;
    POLLED_BAL_KNOWN.load(Ordering::Relaxed).then(|| POLLED_BAL.load(Ordering::Relaxed) as u128)
}

fn get(base: &str, path: &str) -> Result<Value, String> {
    let url = format!("{base}{path}");
    let r = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(15)).build()
        .map_err(|e| e.to_string())?.get(&url).send().map_err(|e| format!("node unreachable: {e}"))?;
    let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !ct.contains("json") { return Err(format!("{path} answered {ct}, not JSON — route missing on this node")); }
    r.json::<Value>().map_err(|e| format!("bad JSON from {path}: {e}"))
}

fn glyphs_to_sigil(v: &Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()).map(|g| g / GLYPHS_PER_SIGIL)
}

/// This node's own wallet, if it has a real key (seed-derived) — never the hostname fallback.
fn own_wallet() -> Option<String> { crate::miner_keypair().map(|kp| kp.pubkey_hex()) }

/// Run a READ or PROPOSE tool. Returns (result JSON for the model, receipt line for the user).
pub(crate) fn run_read(name: &str, args: &Value, base: &str) -> (Value, String) {
    let err = |m: String| (json!({"error": m}), format!("🔧 {name} → ⚠ {m}"));
    match name {
        "sigil_balance" => {
            let arg = args.get("wallet").and_then(|w| w.as_str()).unwrap_or("").trim().to_string();
            let wallet = if is_wallet(&arg) { arg } else if arg.is_empty() || arg.contains(' ') || arg.contains('_') {
                match own_wallet() { Some(w) => w, None => return err("no wallet given and this node has none — ask the user for their 64-hex address".into()) }
            } else { return err(format!("'{arg}' is not a 64-hex wallet ({} chars) — ask the user to paste the full address", arg.len())) };
            match get(base, &format!("/v1/balance?wallet={wallet}")) {
                Ok(j) => {
                    let bal = j.pointer("/data/balance").and_then(glyphs_to_sigil).unwrap_or(0.0);
                    (json!({"wallet": wallet, "balance_sigil": bal, "note": "transparent balance; shielded notes are private and not shown here"}),
                     format!("🔧 balance {} → {bal} SIGIL", short(&wallet)))
                }
                Err(e) => err(e),
            }
        }
        "sigil_network_status" => match (get(base, "/v1/supply"), get(base, "/v1/mining/challenge")) {
            (Ok(s), Ok(c)) => {
                let sup = s.pointer("/data/native_supply").and_then(glyphs_to_sigil).unwrap_or(0.0);
                let h = c.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                let pct = s.pointer("/data/minted_pct").and_then(|p| p.as_f64()).unwrap_or(0.0);
                (json!({"height": h, "native_supply_sigil": sup, "max_supply_sigil": 21_000_000, "minted_pct": pct}),
                 format!("🔧 network → height {h} · supply {sup:.0} SIGIL ({pct:.3}%)"))
            }
            (Err(e), _) | (_, Err(e)) => err(e),
        },
        "sigil_pools" => match get(base, "/v1/pools") {
            Ok(j) => { let n = j.get("pools").and_then(|p| p.as_array()).map(|a| a.len()).unwrap_or(0);
                       (j, format!("🔧 pools → {n} pool(s)")) }
            Err(e) => err(e),
        },
        "sigil_shielded_status" => match get(base, "/v1/shielded/anchor") {
            Ok(j) => { let n = j.get("notes").and_then(|x| x.as_u64()).unwrap_or(0);
                       let r = json!({"notes": n, "nullifiers": j.get("nullifiers"), "capacity": j.get("capacity"), "epoch": j.get("epoch"), "registered": j.get("registered")});
                       (r, format!("🔧 shielded pool → {n} notes")) }
            Err(e) => err(e),
        },
        "sigil_mining_network" => match get(base, "/v1/mining/miners") {
            Ok(j) => { let d = j.get("data").cloned().unwrap_or(json!({}));
                       let m = d.get("live_miners").and_then(|x| x.as_u64()).unwrap_or(0);
                       let hps = d.get("net_hps").and_then(|x| x.as_f64()).unwrap_or(0.0);
                       (json!({"live_miners": m, "net_mhps": hps / 1e6, "blocks_accepted": d.get("blocks_accepted"), "height": d.get("height")}),
                        format!("🔧 mining network → {m} miners · {:.1} MH/s", hps / 1e6)) }
            Err(e) => err(e),
        },
        "sigil_nation_status" => match get(base, "/v1/nation/status") {
            Ok(j) => { let d = j.get("data").cloned().unwrap_or(json!({}));
                       let t = d.get("treasury_glyphs").and_then(glyphs_to_sigil).unwrap_or(0.0);
                       (json!({"active": d.get("active"), "treasury_sigil": t, "payout_asset": d.get("payout_asset"),
                               "claim_interval_blocks": d.get("claim_interval_blocks")}),
                        format!("🔧 nation → treasury {t:.2} SIGIL")) }
            Err(e) => err(e),
        },
        "sigil_my_wallet" => match own_wallet() {
            Some(w) => (json!({"wallet": w}), format!("🔧 my wallet → {}", short(&w))),
            None => (json!({"wallet": null, "hint": "no wallet on this node yet — offer sigil_create_wallet"}), "🔧 my wallet → none yet".into()),
        },
        "sigil_propose_private_send" => {
            let to = args.get("to").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
            let amt = args.get("amount").and_then(|a| a.as_f64().or_else(|| a.as_str().and_then(|s| s.parse().ok())));
            if !(is_wallet(&to) || is_shielded_addr(&to)) {
                return err(format!("recipient '{}' is not a valid address ({} chars; need 64 hex or sigil1s:…) — ask the user to paste it", short(&to), to.len()));
            }
            let Some(amt) = amt.filter(|a| *a > 0.0 && a.is_finite()) else { return err("no positive amount — ask the user how much".into()) };
            (json!({"status": "shown to the user", "next": "the user signs it in the wallet [W]; nothing has been sent"}),
             format!("🔧 PROPOSED private send · {amt} SIGIL → {to}  (not sent — sign it in the wallet [W])"))
        }
        other => err(format!("unknown tool {other}")),
    }
}

/// One-line description of an action, shown when asking the user to confirm.
pub(crate) fn action_summary(name: &str) -> String {
    match name {
        "sigil_create_wallet" => "create a NEW SIGIL wallet on this node".into(),
        "sigil_start_mining" => "START mining SIGIL on this node".into(),
        "sigil_stop_mining" => "STOP mining on this node".into(),
        o => o.to_string(),
    }
}

/// Where a wallet created from the AI tab keeps its seed (0600, inside the sigil-top home).
pub(crate) fn wallet_seed_path() -> String { format!("{}/.flux/sigil-wallet-seed", crate::flux_home()) }

/// Create a wallet: random seed → 0600 file → `SIGIL_MINE_SEED` for this process → the
/// chosen mining wallet. Refuses if a wallet already exists, so it can never overwrite one.
pub(crate) fn create_wallet() -> Result<(String, String), String> {
    if let Some(w) = own_wallet() { return Err(format!("this node already has a wallet ({}) — not creating a second one", short(&w))); }
    let path = wallet_seed_path();
    if std::path::Path::new(&path).exists() { return Err(format!("{path} already exists — not overwriting it")); }
    // 10.4.22: a new wallet is a 12-word phrase (kept beside the key), so the SAME words also open it
    // on sigilgraph.org and in the phone app. Before, this was a bare random key nobody could type.
    #[cfg(feature = "shield-register")]
    {
        let got = crate::wallet_phrase::install_in(&crate::flux_home(), &crate::wallet_phrase::generate_phrase())?;
        let seed_hex = std::fs::read_to_string(&path).map_err(|e| format!("cannot read back {path}: {e}"))?;
        std::env::set_var("SIGIL_MINE_SEED", seed_hex.trim());
        crate::set_mine_wallet(&got.address);
        return Ok((got.address, path));
    }
    #[allow(unreachable_code)]
    let seed = sigil_oauth::random_seed();
    let hex_seed: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(format!("{}/.flux", crate::flux_home()));
    {
        use std::io::Write;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; o.mode(0o600); }
        let mut f = o.open(&path).map_err(|e| format!("cannot write {path}: {e}"))?;
        f.write_all(hex_seed.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::env::set_var("SIGIL_MINE_SEED", &hex_seed);
    let addr = sigil_oauth::Keypair::from_seed(&seed).pubkey_hex();
    crate::set_mine_wallet(&addr);
    Ok((addr, path))
}

/// The agent loop: stream a turn WITH tools; run read tools and feed results back (≤ MAX_STEPS);
/// stop at an action and ask the user. Any failure to start falls back to the plain stream.
pub(crate) fn chat_agent(model: &str, history: &[(String, String)], user: &str, extra_system: &str,
                         base: &str, tx: &Sender<StreamEv>) {
    let route = flux_moe::current_ai_route();
    let mode = flux_moe::rewrite_mode();
    let rewriter = if matches!(route, flux_moe::AiRoute::Local) || mode == flux_moe::RewriteMode::Off { None } else { flux_moe::find_rewriter() };
    chat_agent_at(model, history, user, extra_system, base, &flux_moe::ollama_base(), &route,
                  Some(&flux_moe::veil_ledger_path()), rewriter.as_ref(), mode, tx)
}

/// [`chat_agent`] with the model endpoint, route and ledger injected (testable).
///
/// SIGIL Veil applies whenever `route` is not Local: this loop is the AI tab's REAL
/// path, and it used to build its own request and post it straight to the endpoint —
/// bypassing the gate, the Tor route and the ledger that `chat`/`chat_stream` had
/// (found 2026-10-03 while wiring L4; never released). Now every step is gated:
/// the first request through `veil_outbound`, the model's tool-call arguments are
/// restored before a tool runs, tool RESULTS are pseudonymised before they go back,
/// streamed answers are restored for the screen, and every step is ledgered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_agent_at(model: &str, history: &[(String, String)], user: &str, extra_system: &str,
                            base: &str, ollama_base: &str, route: &flux_moe::AiRoute,
                            ledger: Option<&std::path::Path>, rewriter: Option<&flux_moe::Rewriter>,
                            mode: flux_moe::RewriteMode, tx: &Sender<StreamEv>) {
    use std::io::BufRead;
    if let flux_moe::AiRoute::Refuse(msg) = route { let _ = tx.send(StreamEv::Whole(msg.clone())); return; }
    let system = format!("{}{}{}", flux_moe::system_prompt(), extra_system, TOOLS_SYSTEM);
    let remote = !matches!(route, flux_moe::AiRoute::Local);
    // L4: an off-box model gets ONE de-identified question rewritten on this device —
    // no history, not the user's own wording. Shown in the transcript, then gated as usual.
    let rewritten = if remote && mode != flux_moe::RewriteMode::Off {
        rewriter.and_then(|rw| flux_moe::rewrite_with(rw, history, user))
    } else { None };
    if remote && mode == flux_moe::RewriteMode::Require && rewritten.is_none() {
        let _ = tx.send(StreamEv::Whole("🕶 Not sent: SIGIL_AI_REWRITE=require and no on-device model could rewrite the question \
            (none installed on this device, or it failed). Install a small local model (e.g. qwen3:1.7b) or relax the setting.".into()));
        return;
    }
    let mut messages = vec![json!({"role":"system","content":system})];
    match &rewritten {
        Some(q) => {
            let _ = tx.send(StreamEv::ToolRan(format!("🕶 rewritten on this device before it left — the model is asked only: {q}")));
            messages.push(json!({"role":"user","content":q}));
        }
        None => {
            if remote && mode == flux_moe::RewriteMode::Auto {
                let _ = tx.send(StreamEv::ToolRan("🕶 no on-device rewriter — sending your words with identifiers hidden (install qwen3:1.7b locally to send an anonymous rewrite instead)".into()));
            }
            for (role, content) in history { messages.push(json!({"role": role, "content": content})); }
            messages.push(json!({"role":"user","content":user}));
        }
    }
    let dest = format!("{}{}", flux_moe::route_label(ollama_base, route), if rewritten.is_some() { " [rewritten on-device]" } else { "" });
    let mut veil = if remote {
        match flux_moe::veil_outbound(&mut messages, &dest, ledger) {
            Ok(m) => Some(m),
            Err(e) => { let _ = tx.send(StreamEv::Whole(e)); return; }
        }
    } else { None };
    let http = match flux_moe::chat_client(route) { Ok(c) => c, Err(e) => { let _ = tx.send(StreamEv::Whole(e)); return; } };
    let url = format!("{}/api/chat", ollama_base);
    let granted = granted_scopes(remote, std::env::var("SIGIL_AI_REMOTE_SCOPES").ok().as_deref());
    let tools = tool_defs_for(&granted);
    for step in 0..MAX_STEPS {
        let body = json!({"model": model, "messages": messages, "stream": true, "tools": tools});
        let resp = match http.post(&url).json(&body).send() {
            Ok(r) if r.status().is_success() => r,
            _ if step == 0 => return flux_moe::chat_stream(model, history, user, extra_system, tx),
            Ok(r) => { let _ = tx.send(StreamEv::Whole(format!("⚠ model error {} after a tool call", r.status()))); return; }
            Err(e) => { let _ = tx.send(StreamEv::Whole(format!("⚠ {e}"))); return; }
        };
        let mut calls: Vec<Value> = Vec::new();
        // `text` stays in the model's own (placeholder) form — it goes back into
        // `messages`; only what reaches the SCREEN is restored.
        let mut text = String::new();
        let mut done_ev = None;
        let mut screen = sigil_veil::StreamRehydrator::new();
        for line in std::io::BufReader::new(resp).lines() {
            let Ok(line) = line else { break };
            for ev in flux_moe::parse_stream_line(&line) {
                match ev {
                    StreamEv::ToolCalls(c) => calls.extend(c),
                    StreamEv::Done { .. } => done_ev = Some(ev),
                    StreamEv::Token(t) => {
                        text.push_str(&t);
                        let shown = match &veil { Some(m) => screen.push(&t, m), None => t };
                        if !shown.is_empty() && tx.send(StreamEv::Token(shown)).is_err() { return; }
                    }
                    StreamEv::Thinking(t) => {
                        let shown = match &veil { Some(m) => sigil_veil::rehydrate(&t, m), None => t };
                        if tx.send(StreamEv::Thinking(shown)).is_err() { return; }
                    }
                    other => { if tx.send(other).is_err() { return; } }
                }
            }
            if done_ev.is_some() { break; }
        }
        if let Some(m) = &veil {
            let tail = screen.finish(m);
            if !tail.is_empty() && tx.send(StreamEv::Token(tail)).is_err() { return; }
        }
        if calls.is_empty() {
            let _ = tx.send(done_ev.unwrap_or(StreamEv::Done { eval_count: 0, eval_ns: 0, prompt_count: 0, reason: "interrupted".into() }));
            return;
        }
        messages.push(json!({"role":"assistant","content": text, "tool_calls": calls}));
        let mut step_findings: Vec<(sigil_veil::Kind, String)> = Vec::new();
        for c in &calls {
            let name = c.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let args = c.pointer("/function/arguments").cloned().unwrap_or(json!({}));
            let mut args = match args { Value::String(s) => serde_json::from_str(&s).unwrap_or(json!({})), v => v };
            // Model output is DATA: a tool it was not given is refused, not run — even
            // if it names one anyway (prompt injection, hallucination, a hostile provider).
            if !granted.contains(&scope_of(&name)) {
                let why = format!("not permitted: the '{}' scope is not granted to a model off this device", scope_label(scope_of(&name)));
                if tx.send(StreamEv::ToolRan(format!("🔒 {name} refused — {why}"))).is_err() { return; }
                messages.push(json!({"role":"tool","tool_name": name, "content": json!({"error": why}).to_string()}));
                continue;
            }
            // the tool acts on the REAL address, never on "[QNK_1]"
            if let Some(m) = &veil { flux_moe::rehydrate_json(&mut args, m); }
            if is_action(&name) {
                let _ = tx.send(StreamEv::Confirm { summary: action_summary(&name), name, args });
                return;
            }
            let (result, receipt) = run_read(&name, &args, base);
            if tx.send(StreamEv::ToolRan(receipt)).is_err() { return; }
            let mut s = result.to_string();
            s.truncate(2000);
            // ...and the result goes back to an off-box model with its identifiers hidden
            if let Some(m) = veil.as_mut() {
                let (hidden, f) = sigil_veil::pseudonymize(&s, m);
                step_findings.extend(f);
                s = hidden;
            }
            messages.push(json!({"role":"tool","tool_name": name, "content": s}));
        }
        if remote { flux_moe::veil_ledger_step(&messages, &dest, &step_findings, ledger); }
    }
    let _ = tx.send(StreamEv::Whole("⚠ stopped after 4 tool steps without a final answer — ask more narrowly.".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_default_to_least_privilege_off_device() {
        assert_eq!(granted_scopes(false, None).len(), 4, "on-device: everything — nothing leaves");
        assert_eq!(granted_scopes(true, None), vec![Scope::Network]);
        assert_eq!(granted_scopes(true, Some("wallet, Payments ,bogus")), vec![Scope::Network, Scope::Wallet, Scope::Payments]);
        let names = |v: Value| v.as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let remote = names(tool_defs_for(&granted_scopes(true, None)));
        assert!(remote.contains(&"sigil_network_status".to_string()));
        for personal in ["sigil_balance", "sigil_my_wallet", "sigil_propose_private_send", "sigil_create_wallet", "sigil_start_mining"] {
            assert!(!remote.contains(&personal.to_string()), "{personal} offered to an off-device model");
        }
        assert_eq!(names(tool_defs_for(&granted_scopes(false, None))).len(), tool_defs().as_array().unwrap().len());
        assert_eq!(scope_of("some_new_tool_nobody_classified"), Scope::Wallet, "unknown tools are personal (deny by default)");
    }

    /// Minimal HTTP server: answers each accepted request with the next reply and
    /// returns the request bodies it saw.
    fn fake_http(replies: Vec<(&'static str, String)>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (ctype, reply) in replies {
                let Ok((c, _)) = listener.accept() else { break };
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" { break; }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap_or(0); }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                bodies.push(String::from_utf8_lossy(&body).to_string());
                let mut c = c;
                let _ = c.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", reply.len(), reply).as_bytes());
            }
            bodies
        });
        (port, h)
    }

    #[test]
    fn off_box_model_receives_only_the_on_device_rewrite() {
        let _env = crate::flux_moe::tests::ai_model_env();
        let personal = "I'm Viktor, I live in Frederikshavn and my mum says my wallet send fails with HTTP 413 — why?";
        let history = vec![("user".to_string(), "earlier I told you my address is Rådhusgade 3".to_string()),
                           ("assistant".to_string(), "noted".to_string())];
        let (rw_port, rw) = fake_http(vec![("application/json",
            json!({"message":{"content":"Why would a cryptocurrency wallet's send request fail with HTTP 413?"},"done":true}).to_string())]);
        let (m_port, model_srv) = fake_http(vec![("application/x-ndjson",
            format!("{}\n{}\n", json!({"message":{"content":"Payload too large: the body exceeds a proxy limit."}}), json!({"done":true})))]);
        let dir = std::env::temp_dir().join(format!("veil-rw-{}", std::process::id()));
        let ledger = dir.join("veil-egress.jsonl");
        let rewriter = flux_moe::Rewriter { base: format!("http://127.0.0.1:{rw_port}"), model: "qwen3:1.7b".into() };
        let (tx, rx) = std::sync::mpsc::channel();
        chat_agent_at("m", &history, personal, "", "http://127.0.0.1:1", &format!("http://127.0.0.1:{m_port}"),
                      &flux_moe::AiRoute::Direct, Some(&ledger), Some(&rewriter), flux_moe::RewriteMode::Auto, &tx);
        drop(tx);
        let events: Vec<StreamEv> = rx.into_iter().collect();
        let to_rewriter = rw.join().unwrap();
        let to_model = model_srv.join().unwrap();
        assert!(to_rewriter[0].contains("Frederikshavn") && to_rewriter[0].contains("Rådhusgade"), "the ON-DEVICE rewriter gets the context");
        let wire = &to_model[0];
        assert!(wire.contains("Why would a cryptocurrency wallet"), "{wire}");
        for leak in ["Viktor", "Frederikshavn", "Rådhusgade", "mum", "earlier I told you"] {
            assert!(!wire.contains(leak), "{leak} reached the off-box model: {wire}");
        }
        assert!(events.iter().any(|e| matches!(e, StreamEv::ToolRan(t) if t.contains("rewritten on this device"))), "the user sees what was asked");
        assert!(std::fs::read_to_string(&ledger).unwrap().contains("[rewritten on-device]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_off_box_model_calling_an_ungranted_tool_is_refused_not_run() {
        let _env = crate::flux_moe::tests::ai_model_env();
        std::env::remove_var("SIGIL_AI_REMOTE_SCOPES");
        let (port, srv) = fake_http(vec![
            ("application/x-ndjson", format!("{}\n{}\n",
                json!({"message":{"content":"","tool_calls":[{"function":{"name":"sigil_balance","arguments":{}}}]}}), json!({"done":true}))),
            ("application/x-ndjson", format!("{}\n{}\n", json!({"message":{"content":"I can't see balances."}}), json!({"done":true}))),
        ]);
        let (tx, rx) = std::sync::mpsc::channel();
        chat_agent_at("m", &[], "what's my balance?", "", "http://127.0.0.1:1", &format!("http://127.0.0.1:{port}"),
                      &flux_moe::AiRoute::Direct, None, None, flux_moe::RewriteMode::Off, &tx);
        drop(tx);
        let events: Vec<StreamEv> = rx.into_iter().collect();
        let bodies = srv.join().unwrap();
        assert!(!bodies[0].contains("\"sigil_balance\""), "the tool was never even offered");
        assert!(events.iter().any(|e| matches!(e, StreamEv::ToolRan(t) if t.contains("refused") && t.contains("wallet"))));
        assert!(!events.iter().any(|e| matches!(e, StreamEv::ToolRan(t) if t.contains("balance") && t.contains("SIGIL"))), "and it never ran");
        assert!(bodies[1].contains("not permitted"), "the model is told why");
    }

    #[test]
    fn require_mode_without_a_rewriter_sends_nothing() {
        let _env = crate::flux_moe::tests::ai_model_env();
        let (m_port, model_srv) = fake_http(vec![]);
        let (tx, rx) = std::sync::mpsc::channel();
        chat_agent_at("m", &[], "anything", "", "http://127.0.0.1:1", &format!("http://127.0.0.1:{m_port}"),
                      &flux_moe::AiRoute::Direct, None, None, flux_moe::RewriteMode::Require, &tx);
        drop(tx);
        let events: Vec<StreamEv> = rx.into_iter().collect();
        assert!(events.iter().any(|e| matches!(e, StreamEv::Whole(t) if t.contains("Not sent"))));
        assert!(model_srv.join().unwrap().is_empty(), "the off-box model must never be contacted");
    }

    /// The AI tab's REAL path, end to end, against a fake OFF-BOX model server: the
    /// first request and the follow-up carrying the tool result must both leave
    /// without the wallet address; the tool must act on the real address; the
    /// screen must show the real address; both steps must be ledgered.
    #[test]
    fn agent_loop_to_an_off_box_model_goes_through_veil() {
        use std::io::{BufRead, BufReader, Read, Write};
        const W: &str = "4973498a9865b291636faef205f728a49d98890f001e9e806479043f038ebf6c";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let replies = vec![
            format!("{}\n{}\n",
                json!({"message":{"content":"","tool_calls":[{"function":{"name":"sigil_balance","arguments":{"wallet":"[HEX_1]"}}}]}}),
                json!({"done":true})),
            format!("{}\n{}\n{}\n",
                json!({"message":{"content":"Balance of [HE"}}), json!({"message":{"content":"X_1] is unknown."}}), json!({"done":true,"done_reason":"stop"})),
        ];
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for reply in replies {
                let (c, _) = listener.accept().unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() { break; }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap(); }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                bodies.push(String::from_utf8(body).unwrap());
                let mut c = c;
                let _ = c.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", reply.len(), reply).as_bytes());
            }
            bodies
        });
        let dir = std::env::temp_dir().join(format!("veil-agent-{}", std::process::id()));
        let ledger = dir.join("veil-egress.jsonl");
        let (tx, rx) = std::sync::mpsc::channel();
        let _env = crate::flux_moe::tests::ai_model_env();
        std::env::set_var("SIGIL_AI_REMOTE_SCOPES", "wallet");
        chat_agent_at("m", &[], &format!("what is the balance of {W}?"), "", "http://127.0.0.1:1",
                      &format!("http://127.0.0.1:{port}"), &flux_moe::AiRoute::Direct, Some(&ledger),
                      None, flux_moe::RewriteMode::Off, &tx);
        drop(tx);
        let mut shown = String::new();
        let mut receipts = Vec::new();
        for ev in rx {
            match ev {
                StreamEv::Token(t) => shown.push_str(&t),
                StreamEv::ToolRan(r) => receipts.push(r),
                _ => {}
            }
        }
        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("[HEX_1]") && !bodies[0].contains(W), "first request leaked: {}", bodies[0]);
        assert!(bodies[1].contains("\"role\":\"tool\"") && !bodies[1].contains(W), "tool result leaked: {}", bodies[1]);
        assert!(receipts.iter().any(|r| r.contains("balance") && !r.contains("no wallet given")),
            "the tool must have acted on the REAL wallet, not fallen back to the node's own: {receipts:?}");
        assert_eq!(shown, format!("Balance of {W} is unknown."), "the screen shows the real address");
        assert_eq!(std::fs::read_to_string(&ledger).unwrap().lines().count(), 2, "both steps ledgered");
        std::env::remove_var("SIGIL_AI_REMOTE_SCOPES");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn address_guards_reject_what_qwen3_8b_actually_produced() {
        assert!(is_wallet(&"7c0e".repeat(16)));
        assert!(!is_wallet(&"7c0e".repeat(15)));            // the 60-char truncation, measured
        assert!(!is_wallet("bob's wallet address"));         // the invented recipient, measured
        assert!(is_shielded_addr(&format!("sigil1s:{}:{}", "ab".repeat(32), "cd".repeat(32))));
        assert!(!is_shielded_addr("sigil1s:abcd"));
    }

    #[test]
    fn a_bad_recipient_is_returned_as_an_error_never_proposed() {
        let (r, line) = run_read("sigil_propose_private_send", &json!({"to": "7c0e".repeat(15), "amount": 5}), "http://127.0.0.1:9");
        assert!(r.get("error").is_some(), "{r}");
        assert!(line.contains("⚠"));
        let (r, _) = run_read("sigil_propose_private_send", &json!({"to": "7c0e".repeat(16), "amount": 0}), "http://127.0.0.1:9");
        assert!(r.get("error").is_some());
        let (r, line) = run_read("sigil_propose_private_send", &json!({"to": "7c0e".repeat(16), "amount": 5}), "http://127.0.0.1:9");
        assert!(r.get("error").is_none() && line.contains(&"7c0e".repeat(16)), "full address must be shown: {line}");
    }

    #[test]
    fn actions_are_never_run_by_the_read_path() {
        for a in ["sigil_create_wallet", "sigil_start_mining", "sigil_stop_mining"] { assert!(is_action(a)); }
        assert!(!is_action("sigil_balance") && !is_action("sigil_propose_private_send"));
    }

    #[test]
    fn every_tool_definition_is_well_formed() {
        let d = tool_defs();
        let arr = d.as_array().unwrap();
        assert!(arr.len() >= 10);
        for t in arr { assert!(t.pointer("/function/name").and_then(|n| n.as_str()).is_some()); }
    }
}
