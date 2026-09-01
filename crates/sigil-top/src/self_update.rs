//! Startup auto-update: fetch the signed release manifest, verify + hot-swap the
//! binary, and re-exec into it (with the Windows lock-fallback). Extracted from
//! main.rs. `use super::*` reaches Release/VERSION/version_gt/fetch_latest/the
//! updater helpers (fetch_binary_reqwest, preflight_binary) + self_replace.
use super::*;

/// Default-ON startup auto-update against the **pinned release channel**. Runs once at launch:
/// fetch the operator-controlled manifest, and ONLY if it names a version newer than this binary
/// (i.e. the operator has *promoted* a release by writing the manifest — publishing a GitHub release
/// alone does NOT advance the channel), download + BLAKE3-verify + hot-swap, then re-exec the new
/// binary so the node is immediately running the chosen version. Returns an Option<String> toast
/// for the TUI instead of eprintln! (which corrupts the alt-screen). Disable with `--no-update` o
/// `SIGIL_TOP_NO_AUTOUPDATE=1`.
pub(crate) fn maybe_auto_update(argv: &[String]) -> Option<String> {
    if argv.iter().any(|a| a == "--no-update")
        || std::env::var("SIGIL_TOP_NO_AUTOUPDATE").map(|v| v == "1").unwrap_or(false)
    {
        return None;
    }
    let rel = match fetch_latest() {
        Ok(r) if version_gt(&r.version, VERSION) => r,
        Ok(r) if version_gt(VERSION, &r.version) => return Some(format!("⚠ {}", release_channel_stale_msg(&r.version))),
        Ok(_) => return None,
        Err(e) if e.contains("MANIFEST SIGNATURE INVALID") => {
            return Some(format!("⚠ update channel signature invalid — {e}"));
        }
        Err(_) => return None, // channel unreachable/malformed → just run
    };
    match self_update(&rel) {
        Ok(msg) if msg.starts_with("staged v") => {
            // Windows lock-fallback path: a detached helper will move the staged binary over the
            // install path AFTER this process exits, then relaunch it. Exit now so it can proceed.
            std::process::exit(0);
        }
        Ok(_) => {
            // Relaunch into the new binary. self_replace installed the new version AT THE
            // CURRENT EXE PATH, so that's the canonical relaunch target; a versioned copy
            // beside us (if one survived) is only a fallback. The previous Windows branch
            // spawned ONLY the versioned file and, when it was absent, hit a bare exit(0) —
            // so the app updated in place but never restarted ("just exits"). Now every
            // platform relaunches the in-place exe. The new process re-runs this check,
            // sees its version == the channel, and proceeds — no update loop.
            // relaunch_new_binary replaces this process (unix exec) / spawns+exits
            // (win/mac) on success and only RETURNS on failure — it never spawns a
            // detached child that would fight the terminal. On success this line is
            // never reached; on failure the new binary is already swapped in place, so
            // we keep running the current process this time and pick it up next launch.
            relaunch_new_binary(&rel.version);
            Some(format!("↑ updated to v{} — restart to run it", rel.version))
        }
        Err(e) => Some(format!("auto-update skipped: {e}")),
    }
}

/// Fetch the bytes at `url` over HTTPS with a TLS 1.2 floor. Free-standing and
/// self-contained by design: it takes only a URL, builds its own short-lived
/// `reqwest` client, and touches no sigil-top state (no `App`, no wallet paths, no
/// globals like [`HTTP`] or `LAST_FEED_ERR`) — so it's meant to be lifted as-is into
/// a shared crate (e.g. a future `sigil-updater`) that other binaries can depend on
/// too. Mirrors the client-builder pattern already used by `fetch_latest` /
/// `self_update` (timeout + TLS floor + user-agent); a caller embedding this in a
/// different crate should swap the user-agent literal for its own.
// fetch_binary_reqwest moved to updater.rs (god-file split).

pub(crate) fn self_update(rel: &Release) -> Result<String, String> {
    let mut t = rel.for_self();
    if t.url.is_empty() { return Err(format!("manifest has no {SELF_TARGET} build")); }
    // v7.0.26: download from the SAME base the manifest came from. The manifest's
    // absolute URLs point at the HTTPS domain; on a filtered network only the
    // plain-HTTP :8099 mirror is reachable, so rebase by filename (blake3 gate
    // below authenticates the bytes regardless of transport).
    let bi = ACTIVE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if bi > 0 {
        if let Some(name) = t.url.rsplit('/').next() {
            t.url = format!("{}/{}", CHANNEL_BASES[bi.min(CHANNEL_BASES.len()-1)], name);
        }
    }
    // Use the ONE hardened download path (updater::fetch_binary_reqwest) rather than a
    // duplicate inline client — a single place to get TLS ≥1.2, the UA, and the timeout right.
    let bytes = fetch_binary_reqwest(&t.url)?;
    if t.size_bytes != 0 && bytes.len() as u64 != t.size_bytes {
        return Err(format!("size mismatch — got {} expected {} bytes", bytes.len(), t.size_bytes));
    }
    // BLAKE3 content-hash gate — the release channel signs binaries by blake3.
    if !t.blake3_hex.is_empty() {
        let got = blake3::hash(&bytes).to_hex().to_string();
        if !got.eq_ignore_ascii_case(&t.blake3_hex) {
            return Err(format!("BLAKE3 mismatch — refusing swap (got {}…)", &got[..12]));
        }
    }
    // LANE-C v0.50: provenance surfacing. The manifest carries the NEW build's
    // flux-rev (its fluxc `.proof` stamp). Show it next to the running FLUX_REV so
    // the operator can SEE provenance actually changed across the swap — a "new"
    // release whose flux-rev equals ours is suspicious (re-published same artifact).
    // Informational only: BLAKE3 above is the gate; this never blocks the swap.
    let prov = {
        let cur = FLUX_REV.strip_prefix("full:").unwrap_or(FLUX_REV);
        let newr = rel.flux_rev.strip_prefix("full:").unwrap_or(&rel.flux_rev);
        let short = |s: &str| s.chars().take(10).collect::<String>();
        if newr.is_empty() || newr == "unstamped" {
            " · prov: manifest unstamped".to_string()
        } else if short(newr) == short(cur) {
            format!(" · ⚠ prov UNCHANGED {}", short(newr))
        } else {
            format!(" · prov {}→{}", short(cur), short(newr))
        }
    };
    // Save beside the current exe as a versioned binary.
    // Windows: cannot swap running .exe; save as sigil-top-v{VERSION}.exe.
    // Unix: try atomic self-replace; fall back to versioned binary beside.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let beside = exe.with_file_name(format!("sigil-top-v{}{}", rel.version,
        if cfg!(windows) { ".exe" } else { "" }));
    std::fs::write(&beside, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&beside, std::fs::Permissions::from_mode(0o755));
    }
    // In-place self-replace on BOTH platforms — the self_replace crate handles the
    // Windows "rename the running .exe out of the way" trick, so the launched
    // sigil-top(.exe) actually becomes the new version (was unix-only → Windows kept
    // relaunching the old exe = "doesn't update").
    // v0.27.5: keep the CURRENT binary as the rollback image BEFORE swapping. If the new
    // version passes pre-flight but then crash-loops in real operation, `crashloop_guard()`
    // reverts to this on the next boot (self-healing updater). Best-effort.
    if let (Ok(cur), Some(prev)) = (std::env::current_exe(), prev_binary_path()) {
        let _ = std::fs::copy(&cur, &prev);
    }
    let mb = bytes.len() as f64 / 1.048576e6;
    // Common fast path (both platforms): atomic in-place self-replace. On Windows the
    // self_replace crate does the "rename the running .exe out of the way" trick itself.
    if self_replace::self_replace(&beside).is_ok() {
        let _ = std::fs::remove_file(&beside);
        return Ok(format!("swapped v{VERSION} -> v{} ({mb:.1} MB){prov} — restart to run", rel.version));
    }
    // self_replace FAILED.
    // Windows: the running .exe was locked (AV / image map / dir perms) and the old code
    // silently fell through to "saved … beside" — the install path kept the OLD binary, the
    // relaunch pre-flighted that OLD binary, saw a version mismatch, aborted, and the operato
    // drifted (DeepSeek root-cause: "running .exe is locked → rename fails → silent skip →
    // version drift"). Escalate instead of drifting. See windows_swap_fallback().
    #[cfg(windows)]
    { return windows_swap_fallback(&beside, rel, mb, &prov); }
    // Unix: self_replace almost never fails; keep the staged versioned binary beside us and let
    // relaunch_new_binary fall back to it.
    #[cfg(not(windows))]
    Ok(format!("saved v{} ({mb:.1} MB){prov} -> {}", rel.version, beside.display()))
}

/// Windows lock-failure fallback for [`self_update`]. `self_replace` could not swap the running
/// `.exe` (locked image / AV scan / directory perms). DeepSeek-designed escalation, fail-loud:
///   1. rename the running exe → `…exe.old` (Windows permits renaming a running image on the same
///      volume), then copy the staged bytes into the original install path → instant in-place swap
///      (relaunch_new_binary then re-execs the install path exactly as in the common case);
///   2. if that fails, write a DETACHED helper `.bat` that waits for THIS pid to exit, moves the
///      staged binary over the install path, relaunches it, and self-deletes — applied on exit;
///   3. if NEITHER works, return `Err` so the [U] handler shows a LOUD failure (never a silent
///      "saved" that drifts the version).
/// The `…exe.old` left behind by path 1 is cleaned up on the next boot (see main()).
#[cfg(windows)]
pub(crate) fn windows_swap_fallback(beside: &std::path::Path, rel: &Release, mb: f64, prov: &str)
    -> Result<String, String>
{
    use std::os::windows::process::CommandExt;
    let install = INSTALL_EXE.get().cloned()
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "cannot resolve install path for swap".to_string())?;
    // (1) rename-out + copy-in.
    let old = install.with_extension("exe.old");
    let _ = std::fs::remove_file(&old); // clear any stale .old first (best-effort)
    if std::fs::rename(&install, &old).is_ok() {
        if std::fs::copy(beside, &install).is_ok() {
            let _ = std::fs::remove_file(beside);
            return Ok(format!("swapped v{VERSION} -> v{} ({mb:.1} MB){prov} — restart to run", rel.version));
        }
        // copy failed AFTER the rename → restore the old binary so we don't brick the install.
        let _ = std::fs::rename(&old, &install);
    }
    // (2) detached helper that applies the swap once we exit.
    let pid = std::process::id();
    let bat = std::env::temp_dir().join(format!("sigil-top-swap-{pid}.bat"));
    // CRLF + quoted paths (the install dir routinely has spaces, e.g. "Viktor S. Kristensen").
    // Wait for our PID to vanish, move staged→install, relaunch in a fresh console, self-delete.
    let script = format!(
        "@echo off\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>nul | find \"{pid}\" >nul\r\n\
         if not errorlevel 1 ( timeout /t 1 /nobreak >nul & goto wait )\r\n\
         move /Y \"{src}\" \"{dst}\" >nul\r\n\
         start \"\" \"{dst}\"\r\n\
         del \"%~f0\"\r\n",
        pid = pid, src = beside.display(), dst = install.display());
    std::fs::write(&bat, &script).map_err(|e| format!("windows swap (locked exe): cannot write helper ({e})"))?;
    // DETACHED_PROCESS (0x8) | CREATE_NEW_PROCESS_GROUP (0x200): outlives us, no inherited console.
    std::process::Command::new("cmd")
        .args(["/C", &bat.to_string_lossy()])
        .creation_flags(0x0000_0008 | 0x0000_0200)
        .spawn()
        .map_err(|e| format!("windows swap (locked exe): cannot spawn helper ({e})"))?;
    Ok(format!("staged v{} ({mb:.1} MB){prov} — applying on exit", rel.version))
}

/// v0.25: pre-flight a freshly-swapped binary BEFORE handing off to it. `exec`/`spawn+exit`
/// destroys the running app; if the new binary is corrupt (truncated download), ABI/GLIBC-
/// incompatible, or hangs on start, the app would simply VANISH on the restart-after-sync —
/// the exact bug this fixes. Spawn `target --selfcheck` (a no-op that prints the version and
/// exits 0) with a short timeout; return Ok(version) only if it runs cleanly AND prints a
/// non-empty version. Anything else → don't hand off, keep the running app alive.
// Self-update subsystem being split into updater.rs (god-file split, 2026-09-01).
// preflight_binary moved there; more of the cluster follows across ticks.

/// Relaunch into the just-installed binary after a successful `self_update`. `self_replace`
/// put the new version at the current exe path, so that's the canonical target; a versioned
/// copy beside us (if any) is a fallback.
///
/// v0.25 FAIL-SAFE: we PRE-FLIGHT the target (`--selfcheck`) before any handoff. `exec`
/// destroys this process, so we only ever do it for a binary we've CONFIRMED starts and
/// reports a sane version. If the pre-flight fails (corrupt swap, ABI mismatch, hang) we
/// return `false` WITHOUT tearing anything down — the caller restores its TUI and tells the
/// user to restart manually, and the app keeps running on the current image. No more
/// "app vanishes when it tries to restart after sync". Returns `false` on any non-handoff
/// path; on unix a successful pre-flight + `exec` never returns.
pub(crate) fn relaunch_new_binary(version: &str) -> bool {
    // Use the startup-captured install path — current_exe() points at the
    // moved-aside OLD inode after self_replace (a "(deleted)" path), which would
    // make the pre-flight spawn fail with ENOENT even though the NEW binary is in
    // place. Fall back to current_exe() only if the early capture somehow missed.
    let exe = match INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()) {
        Some(e) => e, None => return false,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ver_exe = exe.with_file_name(format!(
        "sigil-top-v{}{}", version, if cfg!(windows) { ".exe" } else { "" }));
    // Prefer the original install path (now holding the new binary); the versioned
    // sibling is a fallback only if it still exists and current_exe() is unusable.
    let target = if exe.exists() { exe.clone() }
        else if ver_exe.exists() { ver_exe }
        else { exe.clone() };

    // GATE: never hand off to an unverified binary. A failed pre-flight means the swapped
    // binary can't start — abort the relaunch and stay alive on the current (working) image.
    match preflight_binary(&target) {
        Ok(reported) => {
            // v0.56 FAIL-CLOSED: the new binary MUST report the version the channel advertised.
            // A mismatch means the swap didn't take (self_replace no-op'd → still the OLD binary)
            // or the manifest version disagrees with the artifact — relaunching either would run
            // the WRONG version (operator hit "reports v0.40.4, expected v0.42.0 — relaunching
            // anyway"). Abort the handoff AND revert to the rollback image so the next start is
            // known-good, instead of silently running a mismatched binary.
            //
            // v0.59 LANE-O (c): but DON'T abort a genuinely-newer swap just because `version`
            // (= the possibly-STALE app.latest) disagrees. The channel can move 0.57<->0.58 while
            // app.latest still caches the older number; if the staged binary reports a version
            // NEWER than the one we're running, the swap is GOOD — relaunch it. Only revert when
            // the staged binary is NOT an upgrade over the current image (a real no-op / downgrade).
            if !reported.is_empty() && reported != version && !version_gt(&reported, VERSION) {
                eprintln!("  [update] relaunch ABORTED — new binary reports v{reported}, expected v{version} (version mismatch); reverting to the rollback image and staying on the current version. Restart manually once the channel is fixed.");
                if let Some(prev) = prev_binary_path() {
                    if prev.exists() && self_replace::self_replace(&prev).is_err() {
                        let _ = std::fs::copy(&prev, &target);
                    }
                }
                return false;
            }
        }
        Err(e) => {
            eprintln!("  [update] relaunch ABORTED — new binary failed pre-flight ({e}); staying on the current version, restart manually to apply.");
            return false;
        }
    }

    // Pre-flight passed → commit to the handoff.
    std::env::set_var("SIGIL_TOP_JUST_UPDATED", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec REPLACES this process — it only returns on FAILURE. Pre-flight already proved
        // the binary runs, so a failure here is exotic (e.g. ETXTBSY). Don't detach a child
        // that fights the foreground terminal; return false so the caller restores its TUI.
        let _err = std::process::Command::new(&target).args(&args).exec();
        false
    }
    #[cfg(not(unix))]
    {
        // Windows/macOS can't replace a running image — spawn the (pre-flighted) new one,
        // then exit. v0.59 LANE-O: the exe we JUST self_replace'd is frequently still
        // briefly LOCKED right after the swap (AV scan / the old file handle settling), so a
        // single spawn() fails → we used to return false and silently NEVER restart (the
        // operator's "swapped … restart to run, but it never restarts" frustration). Settle,
        // then RETRY a few times so the lock clears.
        use std::thread::sleep;
        use std::time::Duration;
        sleep(Duration::from_millis(300)); // let self_replace's handle + any AV scan settle
        // v0.75.2: on Windows relaunch into a FRESH console (CREATE_NEW_CONSOLE) so the new
        // binary gets its OWN visible window — a plain spawn() SHARES the parent's console,
        // which the launching .bat then reclaims for its `pause`, leaving the new TUI invisible
        // ("swapped but never reopens"). macOS keeps the plain spawn.
        for _ in 0..6 {
            #[cfg(windows)]
            let spawned = {
                use std::os::windows::process::CommandExt;
                std::process::Command::new(&target).args(&args)
                    .creation_flags(0x0000_0010 /* CREATE_NEW_CONSOLE */).spawn().is_ok()
            };
            #[cfg(not(windows))]
            let spawned = std::process::Command::new(&target).args(&args).spawn().is_ok();
            if spawned {
                sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            sleep(Duration::from_millis(300));
        }
        // Fallback: `cmd /C start` with a QUOTED target path — the install dir has SPACES
        // ("Viktor S. Kristensen"), and start parses an unquoted path only up to the first
        // space, so the old unquoted form silently failed. Quote it so start runs the real exe.
        #[cfg(windows)]
        {
            if let Some(t) = target.to_str() {
                let inner = format!("start \"\" \"{}\" {}", t, args.join(" "));
                if std::process::Command::new("cmd").args(["/C", &inner]).spawn().is_ok() {
                    sleep(Duration::from_millis(400));
                    std::process::exit(0);
                }
            }
        }
        // Every path failed → return false so the caller surfaces an HONEST "couldn't
        // auto-restart — please relaunch manually" (and does NOT overwrite it, see [U] handler).
        false
    }
}
