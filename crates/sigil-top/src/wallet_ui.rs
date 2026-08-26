// LANE-U: extracted from main.rs (pure move, no behavior change).
// `use super::*` reaches main.rs's private helpers/consts/App — the heroes.rs pattern.
#![allow(clippy::too_many_lines)]
use super::*;

pub(crate) fn local_wallet_url() -> String {
    if let Ok(u) = std::env::var("FLUX_WALLET_URL") { if !u.is_empty() { return u; } }
    "http://localhost:9800/".into()
}

/// Headless fallback for [W]. `http://localhost:9800/` (what [W] shows when it
/// can't open a browser) is meaningless off this box — a copy-paste into a
/// browser on any OTHER machine just points that machine at itself. That's
/// the literal Linux/SSH-terminal bug: the message technically appears, but
/// the link it gives never works remotely (operator report, 2026-08-19).
///
/// Point at the SAME tron wallet UI, hosted publicly, with this node's
/// mining address pre-filled via `?addr=` — the wallet page's own gate
/// (`gui/sigil-wallet-tron-embedded.html`, ~line 290) already treats a valid
/// `?addr=` as "view this address", no login step, before falling back to
/// its localStorage-remembered wallet. That link now works from ANY device,
/// ANY browser, the moment it's opened.
///
/// This carries NO secret. `?addr=` is [`crate::miner_wallet`]'s PUBLIC
/// address — never a private key or recovery phrase. This app never lets a
/// signing key leave the browser that generated it (checked: the wallet
/// mnemonic lives only in that browser's `localStorage`, and this Rust
/// process never touches it at all — `SIGIL_MINE_SEED`, if set, is a
/// SEPARATE mining-auth signing key, deliberately env-only/never-persisted,
/// and is NOT threaded into this URL). So Send/Bridge/Swap on the far end
/// still prompt for the recovery phrase on demand, exactly like every other
/// flow in this wallet already does — this link only pre-authenticates the
/// READ side (balance, mining status, activity), which needs no secret at
/// all to be correct and safe to hand out.
pub(crate) fn headless_wallet_view_url() -> String {
    // 2026-08-20: was fluxapp.xyz — that apex's q-flux vhost proxies /v1 (and
    // /api/v1) to the dead sigil-rpcd (:8099), confirmed live (curl returns
    // 502 Bad Gateway on /v1/balance). This wallet page fetches same-origin
    // (const NODE=''), so opening it from fluxapp.xyz silently shows a wrong
    // balance rather than erroring loudly — exactly what an operator hit.
    // sigilgraph.org's vhost is the one proxying to the real sigil-api
    // (:18181); confirmed live (real balance returned, not a 502/404).
    let base = std::env::var("SIGIL_TRON_WALLET_URL").ok().filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://sigilgraph.org/sigil-wallet-tron-embedded.html".into());
    format!("{base}?addr={}", crate::miner_wallet())
}

/// When `SIGIL_MINE_SEED` is set, the LOCAL `:9800` server — not the remote
/// `sigilgraph.org` fallback above — can sign Send/Shield/Swap/Bridge with
/// zero recovery-phrase prompts (see main.rs's zero-prompt wiring, 2026-08-26).
/// A headless (SSH, no DISPLAY) session can still reach that exact server:
/// `:9800` binds `127.0.0.1` only, deliberately, because it holds a signing
/// key — so the way in is an SSH local port-forward, never exposing the port
/// itself. Returns `None` when no seed is configured: there would be nothing
/// zero-prompt to tunnel to, and offering the tunnel would just relocate the
/// same recovery-phrase prompt to a different browser tab.
pub(crate) fn ssh_tunnel_hint() -> Option<String> {
    crate::miner_seed()?;
    let user = std::env::var("USER").ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_else(|| "<user>".into());
    // SSH_CONNECTION = "<client ip> <client port> <server ip> <server port>" —
    // the 3rd field is the address THIS session was actually reached through,
    // more reliable than guessing a hostname.
    let host = std::env::var("SSH_CONNECTION").ok()
        .and_then(|s| s.split_whitespace().nth(2).map(str::to_string))
        .unwrap_or_else(|| "<this-machine>".into());
    Some(format!(
        "zero-prompt signing is running locally — from your OWN device: ssh -L 9800:localhost:9800 {user}@{host}  then open http://localhost:9800/?addr={}",
        crate::miner_wallet()
    ))
}

pub(crate) fn dirs_next() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var("APPDATA").ok().map(std::path::PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config"))
    }
}

/// The hosted SIGIL wallet that [L]/[W] open. Override with SIGIL_WALLET_URL.
///
/// 2026-08-26: repointed off `fluxapp.xyz` to `sigilgraph.org` — operator
/// instruction ("we are using sigilgraph.org not fluxapp"), and the routing
/// backs it up. `fluxapp.xyz`'s q-flux vhost still proxies BOTH `/api` and
/// `/v1` to the long-dead sigil-rpcd (`127.0.0.1:8099`); confirmed live this
/// session, every one of those paths returns 502 Bad Gateway. The wallet page
/// fetches same-origin (`const NODE=''`), so served from that apex its balance,
/// mining and activity panels all fail. `sigilgraph.org` is the vhost proxying
/// to the real `sigil-api` (`:18181`) — verified live returning real JSON.
///
/// Target the Tron page directly rather than `/sigil-wallet/`: on fluxapp that
/// path was only ever a redirect stub INTO `sigil-wallet-tron.html`, and on
/// sigilgraph.org it has no file at all (spa_fallback serves the landing page,
/// 3.7 KB of the wrong HTML). This is the same file `headless_wallet_view_url`
/// already uses, so [L]/[W]/[T]/[B] now all land on ONE wallet — the one that
/// actually carries the Recent Activity modal.
pub(crate) fn official_wallet_url() -> String {
    std::env::var("SIGIL_WALLET_URL").ok().filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://sigilgraph.org/sigil-wallet-tron-embedded.html".into())
}

/// True if there's no local GUI to open a browser into (headless box / SSH / proxmox
/// console). On Linux that's no DISPLAY and no WAYLAND_DISPLAY.
pub(crate) fn is_headless() -> bool {
    #[cfg(target_os = "linux")]
    { std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() }
    #[cfg(not(target_os = "linux"))]
    { false }
}

/// Best-effort open a URL in the local browser. Returns false if we're headless
/// (no GUI) — the caller then shows the link for the operator to copy instead.
pub(crate) fn open_browser(url: &str) -> bool {
    if is_headless() { return false; }
    let url = url.to_string();
    thread::spawn(move || {
        #[cfg(target_os = "linux")]
        { let _ = Command::new("xdg-open").arg(&url).spawn(); }
        #[cfg(target_os = "macos")]
        { let _ = Command::new("open").arg(&url).spawn(); }
        #[cfg(target_os = "windows")]
        { let _ = Command::new("cmd").args(["/c", "start", &url]).spawn(); }
    });
    true
}

/// The port the embedded wallet server binds. Kept next to [`local_wallet_url`]
/// so the probe and the URL can never drift apart.
pub(crate) const WALLET_PORT: u16 = 9800;

/// True if something is accepting connections on the local wallet port. Cheap
/// (300 ms connect timeout) so it is safe on a keypress path.
pub(crate) fn wallet_server_alive() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], WALLET_PORT));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Browsers we know how to put into private/incognito mode, in the order we try
/// them. Each entry is `(binary, private-mode flag)`; a `spawn` of a missing
/// binary fails with `ENOENT`, which is what makes this list a cheap probe.
#[cfg(target_os = "linux")]
const PRIVATE_BROWSERS: &[(&str, &str)] = &[
    ("firefox", "--private-window"),
    ("firefox-esr", "--private-window"),
    ("brave-browser", "--incognito"),
    ("google-chrome", "--incognito"),
    ("chromium", "--incognito"),
    ("chromium-browser", "--incognito"),
    ("microsoft-edge", "--inprivate"),
    ("vivaldi", "--incognito"),
];

/// macOS: `open -na <app> --args <flag> <url>`. `open` exits non-zero when the
/// app is not installed, so we can check `status()` instead of probing binaries.
#[cfg(target_os = "macos")]
const PRIVATE_BROWSERS_MAC: &[(&str, &str)] = &[
    ("Firefox", "--private-window"),
    ("Brave Browser", "--incognito"),
    ("Google Chrome", "--incognito"),
    ("Microsoft Edge", "--inprivate"),
];

#[cfg(target_os = "windows")]
const PRIVATE_BROWSERS_WIN: &[(&str, &str)] = &[
    ("chrome", "--incognito"),
    ("msedge", "--inprivate"),
    ("brave", "--incognito"),
    ("firefox", "-private-window"),
];

/// Open a URL in a **private / incognito** window.
///
/// [`open_browser`] hands the URL to the desktop's default handler, which always
/// reuses an ordinary window: logged-in session, saved cookies, entry in
/// history. The wallet is a money surface, so `w` (and the other wallet-UI
/// shortcuts) go through here instead — a private window starts from a clean
/// cookie jar and leaves no logged-in wallet tab behind on a shared machine.
///
/// Best-effort by design: we try the known browsers in order and fall back to
/// [`open_browser`] if none of them is installed, because an ordinary tab beats
/// no tab. Overrides, for anyone whose browser we guess wrong:
///
/// * `SIGIL_BROWSER=<binary|path>` plus optional `SIGIL_BROWSER_PRIVATE_FLAG=<flag>`
///   — try this first, ahead of the built-in list.
/// * `SIGIL_BROWSER_PRIVATE=0` — opt out entirely and use the default handler.
///
/// Returns `false` only when there is no GUI at all (see [`is_headless`]), which
/// keeps the same contract as [`open_browser`]: the caller prints a copyable
/// link instead. A `true` return means "we had a display and tried", not "a
/// window definitely appeared" — the default handler has never told us that
/// either.
pub(crate) fn open_browser_private(url: &str) -> bool {
    if is_headless() {
        return false;
    }
    if std::env::var("SIGIL_BROWSER_PRIVATE").as_deref() == Ok("0") {
        return open_browser(url);
    }
    let url = url.to_string();
    thread::spawn(move || {
        // 1) Operator override, if set.
        if let Ok(bin) = std::env::var("SIGIL_BROWSER") {
            if !bin.trim().is_empty() {
                let flag = std::env::var("SIGIL_BROWSER_PRIVATE_FLAG")
                    .unwrap_or_else(|_| "--private-window".to_string());
                let mut cmd = Command::new(bin.trim());
                if !flag.trim().is_empty() {
                    cmd.arg(flag.trim());
                }
                if cmd.arg(&url).spawn().is_ok() {
                    return;
                }
            }
        }
        // 2) The user's ACTUAL default browser, in private mode, before our list
        //    order gets an opinion. `xdg-settings` reports e.g. "firefox.desktop";
        //    match its stem against the table so we pass the right flag.
        #[cfg(target_os = "linux")]
        {
            if let Ok(out) = Command::new("xdg-settings")
                .args(["get", "default-web-browser"])
                .output()
            {
                let desktop = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
                if !desktop.is_empty() {
                    let stem = desktop.trim_end_matches(".desktop");
                    if let Some((bin, flag)) = PRIVATE_BROWSERS
                        .iter()
                        .find(|(bin, _)| stem.contains(bin) || bin.contains(stem))
                    {
                        if Command::new(bin).arg(flag).arg(&url).spawn().is_ok() {
                            return;
                        }
                    }
                }
            }
        }
        // 3) Known browsers, private flag first.
        #[cfg(target_os = "linux")]
        {
            for (bin, flag) in PRIVATE_BROWSERS {
                if Command::new(bin).arg(flag).arg(&url).spawn().is_ok() {
                    return;
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            for (app, flag) in PRIVATE_BROWSERS_MAC {
                let ok = Command::new("open")
                    .args(["-na", app, "--args", flag, &url])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return;
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            for (bin, flag) in PRIVATE_BROWSERS_WIN {
                if Command::new(bin).arg(flag).arg(&url).spawn().is_ok() {
                    return;
                }
            }
            // Not on PATH is normal on Windows; let the shell resolve the app.
            for (bin, flag) in PRIVATE_BROWSERS_WIN {
                if Command::new("cmd")
                    .args(["/c", "start", "", bin, flag, &url])
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
        // 4) Nothing recognised — an ordinary tab beats no tab.
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("xdg-open").arg(&url).spawn();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("open").arg(&url).spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("cmd").args(["/c", "start", "", &url]).spawn();
        }
    });
    true
}

/// Enable/disable launch-at-login (Windows HKCU\…\Run). `Err` (no-op) on other platforms.
#[cfg(windows)]
pub(crate) fn autostart_set(enable: bool) -> Result<(), String> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let out = if enable {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let val = format!("\"{}\"", exe.to_string_lossy());
        Command::new("reg").args(["add", KEY, "/v", "SigilTop", "/t", "REG_SZ", "/d", &val, "/f"]).output()
    } else {
        Command::new("reg").args(["delete", KEY, "/v", "SigilTop", "/f"]).output()
    };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(not(windows))]
pub(crate) fn autostart_set(_enable: bool) -> Result<(), String> { Err("launch-at-login is Windows-only".into()) }

#[cfg(windows)]
pub(crate) fn autostart_enabled() -> bool {
    Command::new("reg")
        .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "SigilTop"])
        .output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(not(windows))]
pub(crate) fn autostart_enabled() -> bool { false }

/// Spawn the Windows system-tray helper for this running node (Open Wallet / Open Block Explorer
/// / Start-at-login / Quit). Hidden PowerShell process, fully detached; it auto-exits when this
/// process dies. No-op off Windows and best-effort (any failure is swallowed — the node runs on).
#[cfg(windows)]
pub(crate) fn spawn_system_tray() {
    use std::io::Write;
    let pid = std::process::id().to_string();
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let script = include_str!("../assets/sigil-tray.ps1");
    let path = std::env::temp_dir().join("sigil-top-tray.ps1");
    if std::fs::File::create(&path).and_then(|mut f| f.write_all(script.as_bytes())).is_err() { return; }
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .args(["-NodePid", &pid, "-ExePath", &exe])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(windows))]
pub(crate) fn spawn_system_tray() {}

/// Keep only safe chars so a flux:// URL can't inject shell metacharacters.
pub(crate) fn flux_safe_arg(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || "._-/".contains(*c)).collect()
}

/// Ensure the :9800 wallet server is up (spawn a detached `serve` if not), open `path`.
pub(crate) fn flux_open_local(path: &str) {
    let up = "127.0.0.1:9800".parse::<std::net::SocketAddr>().ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(350)).ok())
        .is_some();
    if !up {
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(&exe).arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            thread::sleep(Duration::from_millis(900));
        }
    }
    open_browser_private(&format!("http://localhost:9800{path}"));
    thread::sleep(Duration::from_millis(500)); // let the browser launch before exit
}

/// Run `cmd` in a VISIBLE terminal (cross-platform) — so a flux:// URL can't run
/// fluxc commands behind the user's back.
pub(crate) fn flux_run_terminal(cmd: &str) {
    #[cfg(target_os = "linux")]
    {
        let hold = format!("{cmd}; echo; echo '[flux:// done — press enter]'; read _");
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "alacritty", "kitty", "xterm"] {
            let ok = match term {
                "gnome-terminal" | "xfce4-terminal" => Command::new(term).args(["--", "sh", "-c", &hold]).spawn().is_ok(),
                _ => Command::new(term).args(["-e", "sh", "-c", &hold]).spawn().is_ok(),
            };
            if ok { return; }
        }
    }
    #[cfg(target_os = "macos")]
    { let _ = Command::new("osascript").args(["-e", &format!("tell app \"Terminal\" to do script \"{cmd}\"")]).spawn(); }
    #[cfg(target_os = "windows")]
    { let _ = Command::new("cmd").args(["/c", "start", "cmd", "/k", cmd]).spawn(); }
}

/// Dispatch a flux:// URL.
pub(crate) fn flux_open(raw: &str) {
    // Debounce: if flux-open fired in the last 1.2s, ignore this one. Stops a tab
    // storm if the browser/OS invokes the handler repeatedly for a single action.
    {
        let stamp = std::env::temp_dir().join("sigil-flux-open.ts");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if let Ok(prev) = std::fs::read_to_string(&stamp) {
            if let Ok(p) = prev.trim().parse::<u128>() {
                if now.saturating_sub(p) < 1200 { return; }
            }
        }
        let _ = std::fs::write(&stamp, now.to_string());
    }
    let rest = raw.trim().trim_start_matches("flux://").trim_start_matches("flux:").trim_start_matches('/');
    let (head, tail) = match rest.split_once('/') { Some((a, b)) => (a, b), None => (rest, "") };
    let head = head.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let arg = flux_safe_arg(tail.split(['?', '#']).next().unwrap_or(""));
    match head.as_str() {
        "" | "wallet" | "tron" | "w" => flux_open_local("/"),
        "enter" | "enter-sigil" | "new" | "onboard" | "login" => flux_open_local("/enter-sigil.html"),
        "engine" | "vite" | "vite-engine" => flux_open_local("/vite-engine.html"),
        // content-addressed fetch (the existing flux:// meaning in flux-fleet)
        "b3" => flux_run_terminal(&format!("flux-fleet get flux://b3/{arg}")),
        // fluxc command surface — visible terminal, whitelisted verbs only
        "build" | "dev" | "serve" | "test" | "stats" | "watch" | "plan" | "mcp" | "quick" | "self" | "run" => {
            let c = if arg.is_empty() { format!("fluxc {head}") } else { format!("fluxc {head} {arg}") };
            flux_run_terminal(&c);
        }
        // anything else → try a served page (graceful 404 if absent)
        other => flux_open_local(&format!("/{}.html", flux_safe_arg(other))),
    }
}

/// Register flux:// → this binary as the OS URL-scheme handler.
pub(crate) fn flux_register_scheme() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().to_string();
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").map_err(|_| "no $HOME".to_string())?;
        let apps = format!("{home}/.local/share/applications");
        std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=Flux URL Handler\nComment=Open flux:// links (wallet, fluxc commands)\nExec={exe} flux-open %u\nTerminal=false\nNoDisplay=true\nStartupNotify=false\nMimeType=x-scheme-handler/flux;\n"
        );
        std::fs::write(format!("{apps}/flux-url-handler.desktop"), desktop).map_err(|e| e.to_string())?;
        let _ = Command::new("xdg-mime").args(["default", "flux-url-handler.desktop", "x-scheme-handler/flux"]).status();
        let _ = Command::new("update-desktop-database").arg(&apps).status();
        println!("  ✓ flux:// registered. Try typing  flux://wallet  in your browser.");
    }
    #[cfg(target_os = "windows")]
    {
        let cmd = format!("\"{exe}\" flux-open \"%1\"");
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux", "/ve", "/d", "URL:Flux Protocol", "/f"]).status();
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux", "/v", "URL Protocol", "/d", "", "/f"]).status();
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux\\shell\\open\\command", "/ve", "/d", &cmd, "/f"]).status();
        println!("  flux:// registered. Try: flux://wallet");
    }
    #[cfg(target_os = "macos")]
    {
        let _ = &exe;
        println!("  flux:// on macOS needs an .app bundle (manual) — open http://localhost:9800/ meanwhile.");
    }
    Ok(())
}

pub(crate) fn flux_unregister_scheme() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    { if let Ok(h) = std::env::var("HOME") { let _ = std::fs::remove_file(format!("{h}/.local/share/applications/flux-url-handler.desktop")); } }
    #[cfg(target_os = "windows")]
    { let _ = Command::new("reg").args(["delete", "HKCU\\Software\\Classes\\flux", "/f"]).status(); }
    println!("  flux:// handler removed.");
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// SIGIL_BROWSER_PRIVATE=0 is the operator's way to keep the wallet in an
    /// ORDINARY browser window (so the wallet's own "remember my unlock"
    /// storage survives across [W] presses) instead of a fresh, storage-wiped
    /// private window every time. Proves the actual shipped branch: with the
    /// override set, open_browser_private() calls the same code path as
    /// plain open_browser() (xdg-open, no incognito flag) — never even
    /// consulting the incognito-capable browser list.
    #[test]
    fn browser_private_opt_out_reaches_the_plain_opener() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-browser-private-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("invoked.txt");
        // A fake `xdg-open` that records exactly what it was called with —
        // shadows the real one via PATH so no actual browser needs to exist.
        let fake_xdg_open = dir.join("xdg-open");
        std::fs::write(&fake_xdg_open, format!(
            "#!/bin/sh\necho \"$@\" > {}\n", marker.display()
        )).unwrap();
        let mut perms = std::fs::metadata(&fake_xdg_open).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&fake_xdg_open, perms).unwrap();

        let prior_path = std::env::var("PATH").unwrap_or_default();
        let prior_display = std::env::var("DISPLAY").ok();
        let prior_wayland = std::env::var("WAYLAND_DISPLAY").ok();
        let prior_flag = std::env::var("SIGIL_BROWSER_PRIVATE").ok();
        std::env::set_var("PATH", format!("{}:{prior_path}", dir.display()));
        std::env::set_var("DISPLAY", ":99"); // defeat is_headless() — a display IS present
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("SIGIL_BROWSER_PRIVATE", "0");

        let opened = open_browser_private("https://example.invalid/?addr=test");
        // Give the spawned thread a moment to run the fake script.
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Restore env immediately, before any assertion can panic and skip it.
        std::env::set_var("PATH", prior_path);
        match prior_display { Some(v) => std::env::set_var("DISPLAY", v), None => std::env::remove_var("DISPLAY") }
        match prior_wayland { Some(v) => std::env::set_var("WAYLAND_DISPLAY", v), None => std::env::remove_var("WAYLAND_DISPLAY") }
        match prior_flag { Some(v) => std::env::set_var("SIGIL_BROWSER_PRIVATE", v), None => std::env::remove_var("SIGIL_BROWSER_PRIVATE") }

        assert!(opened, "a display was present, so this must report true (attempted)");
        let recorded = std::fs::read_to_string(&marker)
            .unwrap_or_else(|_| panic!("fake xdg-open was never invoked — SIGIL_BROWSER_PRIVATE=0 did not reach the plain opener"));
        assert_eq!(recorded.trim(), "https://example.invalid/?addr=test",
            "xdg-open must receive exactly the URL, no --incognito/--private-window flag");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
