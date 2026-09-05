//! `sigil-apex` — the tower... no, the *arena*, in your terminal.
//!
//!   sigil-apex legends                       the roster and what each edge does
//!   sigil-apex play  [--seed N] [--wallet H] [--legend I]   you choose each turn
//!   sigil-apex auto  [--seed N] [--wallet H] [--legend I]   the auto-pilot plays it out
//!   sigil-apex verify --seed N --legend I --actions 1,2,3 [--expect HEX]
//!
//! `play` reads one number (1–6) per turn from stdin. `auto` needs no input — it plays a
//! reasonable match and prints the verifiable id at the end, which is what you hand a
//! second machine to check. `verify` re-runs a match from its inputs and prints (and
//! optionally checks) its id: the "don't trust, verify" button.
use sigil_apex::{replay, Action, Match, Rng, LEGENDS};
use std::io::{BufRead, Write};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn resolve_seed(args: &[String]) -> u64 {
    if let Some(w) = arg(args, "--wallet") { return Rng::seed_from_wallet(&w); }
    arg(args, "--seed").and_then(|s| s.parse().ok().or_else(|| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())).unwrap_or(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("auto");
    let legend = arg(&args, "--legend").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0) % LEGENDS.len();
    let seed = resolve_seed(&args);

    match cmd {
        "legends" => {
            println!("\n  ✦ SIGIL APEX — the roster ✦\n");
            for (i, l) in LEGENDS.iter().enumerate() {
                println!("  {i}  {} {:<11} {:<11} — {}", l.class.glyph(), l.name, l.class.name(), l.perk);
            }
            println!("\n  pick with --legend <n>, or let a --wallet choose your seed.\n");
        }
        "verify" => {
            let acts = parse_actions(&arg(&args, "--actions").unwrap_or_default());
            let (h, w) = replay(seed, legend, &acts);
            println!("match id : {h}");
            println!("winner   : squad {}", w.map(|c| c.to_string()).unwrap_or("—".into()));
            if let Some(exp) = arg(&args, "--expect") {
                let ok = exp.eq_ignore_ascii_case(&h);
                println!("expected : {exp}");
                println!("verdict  : {}", if ok { "✓ MATCHES — this match replays exactly" } else { "✗ MISMATCH — a different match" });
                std::process::exit(if ok { 0 } else { 1 });
            }
        }
        "auto" | "play" => run_match(seed, legend, cmd == "auto"),
        other => { eprintln!("unknown command {other:?} — try: legends | play | auto | verify"); std::process::exit(2); }
    }
}

fn parse_actions(s: &str) -> Vec<Action> {
    s.split([',', ' ']).filter_map(|t| t.trim().parse::<u32>().ok()).filter_map(Action::from_num).collect()
}

fn banner(m: &Match, legend: usize, seed: u64) {
    println!("\n  ╔════════════════════════════════════════════════════════╗");
    println!("  ║   ✦  S I G I L   A P E X  ✦   seed {seed:<16}   ║");
    println!("  ╚════════════════════════════════════════════════════════╝");
    let p = m.player();
    println!("  you are {} {} the {} — {}", p.legend.class.glyph(), p.legend.name, p.legend.class.name(), LEGENDS[legend % LEGENDS.len()].perk);
    println!("  6 squads land. The ring closes every turn. Last squad standing wins.\n");
}

fn scoreboard(m: &Match) {
    println!("  ── squads ─────────────────────────────────────────────");
    for s in &m.squads {
        if s.alive() {
            println!("   {}{}  {:<10} zone {}  ▮{}  hp {:<3} sh {:<3} gun{} kills {}",
                if s.is_player { "▶" } else { " " }, s.tag, s.legend.name, s.pos, "◆".repeat(s.members.max(0) as usize), s.hp, s.shield, s.loot, s.kills);
        } else {
            println!("    {}  {:<10} — out", s.tag, s.legend.name);
        }
    }
}

fn run_match(seed: u64, legend: usize, auto: bool) {
    let mut m = Match::new(seed, legend);
    banner(&m, legend, seed);
    let stdin = std::io::stdin();
    let mut turn = 0;
    while !m.over() && turn < 12 {
        turn += 1;
        println!("{}", m.render_ring());
        scoreboard(&m);
        let action = if auto {
            auto_pilot(&m)
        } else if m.player().alive() {
            prompt(&stdin)
        } else {
            Action::Rotate // spectating your bots to the end
        };
        if m.player().alive() {
            println!("  ▶ you: {}\n", action.label());
        }
        for line in m.step(action) { println!("     {line}"); }
        println!();
    }
    println!("{}", m.render_ring());
    match m.winner() {
        Some(w) if w.is_player => println!("\n  🏆 CHAMPIONS — you won, {} of {}.\n", w.legend.name, w.legend.class.name()),
        Some(w) => println!("\n  match over — squad {} ({}) takes the crown. You placed higher than most.\n", w.tag, w.legend.name),
        None => println!("\n  the storm took everyone. No champion.\n"),
    }
    println!("  match id : {}", m.hash());
    println!("  verify it: sigil-apex verify --seed {seed} --legend {legend} --actions <your moves> --expect {}\n", m.hash());
}

/// A decent default policy, so `auto` plays a watchable match and so a `play` session with
/// a dead squad still finishes.
fn auto_pilot(m: &Match) -> Action {
    let p = m.player();
    if !(p.pos >= m.lo && p.pos <= m.hi) { return Action::Rotate; }
    if p.shield == 0 && p.hp < p.members * 60 { return Action::Heal; }
    if m.turn >= 3 && p.shield >= 30 && p.loot >= 2 { return Action::Fight; }
    if p.loot < 3 { return Action::Loot; }
    Action::Rotate
}

fn prompt(stdin: &std::io::Stdin) -> Action {
    loop {
        print!("  your move [1 loot · 2 rotate · 3 fight · 4 heal · 5 revive · 6 ult] > ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 { return Action::Rotate; }
        if let Some(a) = line.trim().parse::<u32>().ok().and_then(Action::from_num) { return a; }
        println!("  (type 1–6)");
    }
}
