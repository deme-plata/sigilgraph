//! The two node-status renderers: render_full (the dashboard card) and
//! render_lite (the one-line loop/cron form). Extracted from main.rs.
//! `use super::*` reaches the color consts, NodeStatus, the fmt::* formatters,
//! stall/feed/session helpers — the heroes.rs/wallet_ui.rs module pattern.
use super::*;

pub(crate) fn render_full(st: &NodeStatus, online: bool, api: &str, source: &str) -> String {
    let mut o = String::new();
    // Live update signal from the flux release channel (one-shot; falls back to LATEST).
    let latest = fetch_latest().map(|r| r.version).unwrap_or_else(|_| LATEST.to_string());
    let build_net = build_network_id();
    let net = if st.network.is_empty() { build_net.as_str() } else { &st.network };
    let dot = if online { format!("{GREEN}●{RESET}") } else { format!("{RED}●{RESET}") };
    let state = if online { format!("{GREEN}LIVE{RESET}") } else { format!("{RED}OFFLINE{RESET}") };

    // ── brand header — clean Quillon-graph node look (⬡ mark, one status line) ──
    o.push_str(&format!("\n  {GOLD}◆{RESET} {VBRIGHT}{BOLD}SIGIL{RESET} {DIM}lightweight node{RESET} {VBRIGHT}v{VERSION}{RESET}    {dot} {state}    {DIM}net {net}{RESET}\n"));
    // SELF-DIAGNOSIS: when OFFLINE, show exactly WHY the feed fetch failed (DNS/TLS/connect/HTTP/parse)
    // so the user can read the real cause instead of a blind "offline". This is the dogfood endpoint.
    if !online {
        let e = last_feed_err();
        if !e.is_empty() { o.push_str(&format!("  {RED}why: {}{RESET}\n", e.chars().take(160).collect::<String>())); }
    }
    // ST-2: FROZEN banner — the chain has stopped advancing (peering loss / no qualifying PoW)
    let stall = stall_check(st.height, online);
    if stall.frozen {
        o.push_str(&format!("  {RED}{BOLD}■ FROZEN{RESET} {RED}height {} not advancing for {}s — node stalled (check peers / PoW){RESET}\n", st.height, stall.stalled_secs));
    }
    match read_session() {
        Some(id) => {
            let short = if id.len() > 18 { format!("{}…", &id[..18]) } else { id };
            o.push_str(&format!("    {DIM}resolving{RESET} {CYAN}flux://dashboard@sigilgraph{RESET}  {DIM}·{RESET} {GREEN}◉ {short}{RESET}\n"));
        }
        None => o.push_str(&format!("    {DIM}resolving{RESET} {CYAN}flux://dashboard@sigilgraph{RESET}  {DIM}· not logged in ·{RESET} {GOLD}[L]{DIM}ogin{RESET}\n")),
    }
    // update line — compact + truthful: gold when this binary is behind, green when current
    if version_gt(&latest, VERSION) {
        o.push_str(&format!("    {GOLD}{BOLD}⬆ update{RESET} {GREEN}v{VERSION} → v{latest}{RESET}  {DIM}·{RESET} {GOLD}[U]{DIM} hot-swap via flux://{RESET}\n"));
    } else if version_gt(VERSION, &latest) {
        o.push_str(&format!("    {RED}{BOLD}⚠ channel stale{RESET} {DIM}served v{latest} < binary v{VERSION} · press [U] for details{RESET}\n"));
    } else {
        o.push_str(&format!("    {GREEN}✓ up to date{RESET} {DIM}v{VERSION}  ·{RESET} {GOLD}[U]{DIM} re-check via flux://{RESET}\n"));
    }
    // Nerd Font probe: if these two show as real glyphs (a chain link + the Rust
    // gear), your terminal has a Nerd Font and we can light up the whole UI with
    // them. If they're boxes/?, we stay on the universal Unicode set below.
    o.push_str(&format!(
        "    {DIM}glyph test:{RESET}  {GOLD}\u{F0C1}{RESET} {DIM}chain{RESET}   {GOLD}\u{E7A8}{RESET} {DIM}rust{RESET}   {DIM}· boxes? install a Nerd Font{RESET}\n"
    ));
    o.push('\n');

    // NODE panel (section title embedded in the top border)
    o.push_str(&top_title("NODE"));
    let prod = if st.producer.is_empty() { "—".into() } else { st.producer.clone() };
    let ver = if st.version.is_empty() { "—".into() } else { st.version.clone() };
    let disp_height = st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(st.height);
    o.push_str(&row("height", &format!("{GOLD}{}{RESET}", disp_height)));
    // v0.42: explorer search readiness. The node now binds + serves money/chain
    // routes instantly on restart and builds its full-text index in the
    // background, so this briefly reads "indexing…" before "search ready".
    if !st.index_ready {
        o.push_str(&row("explorer", &format!("{GOLD}⟳ indexing…{RESET} {DIM}money/chain live; search warming up{RESET}")));
    }
    if st.blocks_per_sec > 0.0 {
        // v0.12: gauge the live backfill rate against the SIGIL-g0 sync target of
        // 8000 blk/s (one full second of mainnet block production). A catch-up sync
        // now shows how close it runs to line-rate, not just a bare number.
        const SYNC_TARGET_BPS: f64 = 8000.0;
        let frac = (st.blocks_per_sec / SYNC_TARGET_BPS).clamp(0.0, 1.0);
        let filled = (frac * 10.0).round() as usize;
        let bar: String = "▓".repeat(filled) + &"░".repeat(10 - filled);
        let bps_col = if st.blocks_per_sec >= SYNC_TARGET_BPS { GOLD }
            else if st.blocks_per_sec >= 1000.0 { GREEN } else { DIM };
        o.push_str(&row("blocks/s", &format!(
            "{bps_col}{:.0}{RESET} {DIM}/ 8000{RESET} {bps_col}{bar}{RESET} {DIM}{:.0}%{RESET}",
            st.blocks_per_sec, frac * 100.0)));
    }
    o.push_str(&row("peers", &format!("{}", st.peers)));
    o.push_str(&row("producer", &prod));
    o.push_str(&row("binary", &ver));
    o.push_str(&row("uptime", &fmt_uptime(st.uptime_secs)));

    // state roots — the consensus primitive Quillon lacked.
    // L4-A: when the node publishes a real tip, VERIFY it — don't just display it.
    if let Some(tip) = st.tip.as_ref() {
        let v = verify_tip(tip);
        let badge = if v.ok { format!("{GREEN}✓ VERIFIED{RESET}") } else { format!("{RED}✗ FAILED{RESET}") };
        o.push_str(&mid_title(&format!("4 STATE ROOTS  {badge}  (tip-proof · sigil-g0)")));
        o.push_str(&row("wallet", &short_hex(&tip.roots.wallet_state_root)));
        o.push_str(&row("dex", &short_hex(&tip.roots.dex_state_root)));
        o.push_str(&row("events", &short_hex(&tip.roots.event_log_root)));
        o.push_str(&row("contract", &short_hex(&tip.roots.contract_state_root)));

        o.push_str(&mid_title("TIP VERIFY  (verify-don't-trust · 0 bytes)"));
        if v.ok {
            o.push_str(&row("status", &format!("{GREEN}✓ REAL chain tip {} verified{RESET}", v.height)));
        } else {
            o.push_str(&row("status", &format!("{RED}✗ {}{RESET}", v.err.clone().unwrap_or_default())));
        }
        o.push_str(&row("verify time", &format!("{GOLD}{} µs{RESET} {DIM}· 0 blocks downloaded{RESET}", v.latency_us)));
        o.push_str(&row("fingerprint", &format!("{DIM}{}{RESET}", short_root(&v.fingerprint_hex))));
        if v.hash_is_fingerprint {
            o.push_str(&row("block hash", &format!("{GREEN}commits to these 4 roots{RESET}")));
        } else if !v.reported_hash.is_empty() {
            o.push_str(&row("block hash", &format!("{DIM}{} · commits to ⊃roots{RESET}", short_root(&v.reported_hash))));
        }
        o.push_str(&row("flavor", &format!("{DIM}v0 BLAKE3 · bit-rot-safe, not adversary-proof{RESET}")));
        o.push_str(&row("", &format!("{DIM}adversarial ⇒ K-sources (L4-C) + SQIsign/STARK{RESET}")));
    } else {
        o.push_str(&mid_title("4 STATE ROOTS  (committed per block)"));
        o.push_str(&row("wallet", &short_root(&st.wallet_root)));
        o.push_str(&row("dex", &short_root(&st.dex_root)));
        o.push_str(&row("events", &short_root(&st.event_root)));
        o.push_str(&row("contract", &short_root(&st.contract_root)));
    }

    // economics — 21 M hard cap. ONE-CHAIN P2a: the LEDGER's /supply is the truth
    // (SigilState.native_supply behind the commit chokepoint). The old value came
    // from the spine's display path and read "100% minted" — fiction. Feed value
    // only as fallback while the ledger is unreachable.
    o.push_str(&mid_title("ECONOMICS  (21 M hard cap)"));
    let ledger = ledger_verify::latest();
    let (sup_base, sup_src) = match &ledger {
        Some(li) => (li.supply_base, "ledger"),
        None => (st.native_supply, "feed"),
    };
    let frac = sup_base as f64 / MAX_SUPPLY_BASE as f64;
    o.push_str(&row("supply", &format!("{GOLD}{}{RESET} {DIM}/ 21,000,000 SIGIL · {sup_src}{RESET}", fmt_supply(sup_base))));
    o.push_str(&row("minted", &format!("{}  {GOLD}{:.4}%{RESET}", bar(frac, 30, GOLD), frac * 100.0)));
    if let Some(li) = &ledger {
        o.push_str(&row("chain", &format!("{DIM}mining ledger · height{RESET} {GOLD}{}{RESET}", li.height)));
        let v = match (&li.break_note, li.header_tip) {
            (Some(b), _) => format!("{}✗ {b}{RESET}", "\x1b[38;5;203m"),
            (None, 0) => format!("{DIM}headers not minted yet (pre-P1 node){RESET}"),
            (None, _) => format!("{GREEN}✓ headers #{}→#{} self-linked{RESET} {DIM}· {} walked{RESET}",
                li.verified_floor, li.header_tip, li.checked),
        };
        o.push_str(&row("verify", &v));
    }

    // flux-fold succinct sync capability
    o.push_str(&mid_title("SUCCINCT SYNC  (flux-fold · light node)"));
    o.push_str(&row("fold proof", &format!("{GREEN}2,568 B{RESET} {DIM}constant ∀ chain len{RESET}")));
    o.push_str(&row("whole-chain", &format!("{GREEN}1 check{RESET} {DIM}· 342 ms @ 100k blocks{RESET}")));
    // v0.26.0: EFFECTIVE sync throughput. The fold-proof verifies the WHOLE chain in a
    // constant ~342ms (DeepSeek's #1 lever for 1M blk/s) — so the effective verification
    // rate is chain_height/0.342s and GROWS with the chain. This is the real sync speed:
    // we do not download the 11M-block middle, we prove it.
    let fold_h = st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(st.height);
    let fold_bps = (fold_h as f64 / 0.342) as u64;
    o.push_str(&row("throughput", &format!("{GOLD}⚡ {} blk/s{RESET} {DIM}effective · whole chain proven, grows with length{RESET}", group(fold_bps))));
    // CATHEDRAL DAGKNIGHT (wired 2026-06-17) — module live, full vault ingest + tab in next slice
    o.push_str(&row("cathedral", "vaults:0 (first wire) div:0 ✓ 4-roots+tip-proof+fluxc"));
    o.push_str(&row("crypto", &format!("{DIM}Ajtai/SIS · post-quantum · no trusted setup{RESET}")));

    o.push_str(&bottom());
    if !online {
        o.push_str(&format!("  {DIM}feed + local node both unreachable ({api}) — showing SIGIL constants{RESET}\n"));
    } else if source == "feed" {
        o.push_str(&format!("  {GREEN}● synced from verified live feed{RESET} {DIM}· no local node required — verify on a potato{RESET}\n"));
    }
    // keybar footer — real keybindings UI
    o.push_str(&format!("  {GOLD}[M]{RESET}{DIM}ine{RESET}   {GREEN}[F]{RESET}{DIM}ull{RESET}  {GREEN}[V]{RESET}{DIM}erify{RESET}  {CYAN}[Y]{RESET}{DIM}esync{RESET}   {GOLD}[U]{RESET}{DIM}pdate{RESET}   {VBRIGHT}[L]{RESET}{DIM}ogin{RESET}   {CYAN}[T]{RESET}{DIM}stats{RESET}   {DIM}[Q]uit{RESET}\n"));
    o
}

/// Inner width between the │ borders. Every box line renders to `2 + 1 + BOX_W + 1`
/// = 68 visible columns; `display_width` keeps that exact so the right edge is flush.
const BOX_W: usize = 64;

pub(crate) fn row(label: &str, value: &str) -> String {
    // inner = 3-space indent + 12-wide label + value + pad → exactly BOX_W cols.
    let used = 3 + 12 + display_width(value);
    let pad = " ".repeat(BOX_W.saturating_sub(used).max(1));
    format!("  {VIOLET}│{RESET}   {DIM}{label:<12}{RESET}{value}{pad}{VIOLET}│{RESET}\n")
}
// ╭─ TITLE ─────────╮  — section title embedded in the top border (cyan accent)
pub(crate) fn top_title(title: &str) -> String {
    let fill = BOX_W.saturating_sub(3 + display_width(title)).max(1);
    format!("  {VIOLET}╭─ {CYAN}{BOLD}{title}{RESET} {VIOLET}{}╮{RESET}\n", "─".repeat(fill))
}
// ├─ TITLE ─────────┤  — section divider with the title in the rule, no double line
pub(crate) fn mid_title(title: &str) -> String {
    let fill = BOX_W.saturating_sub(3 + display_width(title)).max(1);
    format!("  {VIOLET}├─ {CYAN}{BOLD}{title}{RESET} {VIOLET}{}┤{RESET}\n", "─".repeat(fill))
}
pub(crate) fn bottom() -> String { format!("  {VIOLET}╰{}╯{RESET}\n", "─".repeat(BOX_W)) }

/// Display width of `s`, ignoring ANSI escape sequences. Most glyphs are one
/// column; emoji-presentation symbols (⬡ ⛏ ⬆ …) are two. Honest width here is what
/// keeps the box's right border flush instead of ragged — the recurring bug.

pub(crate) fn render_lite(st: &NodeStatus, online: bool) -> String {
    let build_net = build_network_id();
    let net = if st.network.is_empty() { build_net.as_str() } else { &st.network };
    let dot = if online { format!("{GREEN}●{RESET}") } else { format!("{RED}●{RESET}") };
    let frac = st.native_supply as f64 / MAX_SUPPLY_BASE as f64;
    // L4-A: lite scorecard carries the verify verdict — the whole point of a light client.
    let (height, vbadge) = match st.tip.as_ref().map(verify_tip) {
        Some(v) if v.ok => (v.height, format!("  {GREEN}✓tip{RESET}")),
        Some(v) => (v.height, format!("  {RED}✗tip{RESET}")),
        None => (st.height, String::new()),
    };
    // ST-2: FROZEN token — height not advancing (drop into tmux strips / SSH peeks)
    let stall = stall_check(st.height, online);
    let frozen = if stall.frozen { format!("  {RED}{BOLD}■FROZEN {}s{RESET}", stall.stalled_secs) } else { String::new() };
    format!(
        "  {dot} {VBRIGHT}◆ SIGIL{RESET} {DIM}{net}{RESET}  h{GOLD}{height}{RESET}{vbadge}{frozen}  {VIOLET}{}{RESET}peers  {GOLD}{}{RESET}{DIM}/21M {:.2}%{RESET}  {DIM}fold 2.5KB·1chk{RESET}\n",
        st.peers, fmt_supply(st.native_supply), frac * 100.0
    )
}
