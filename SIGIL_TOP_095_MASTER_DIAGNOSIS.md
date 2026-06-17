# ⬡ SIGIL-TOP — Master Diagnosis: "Head & Tail of the Shebang"
### Why nothing has worked since 0.37 — and the exact fix plan for the 0.95.0 release

> **Audience:** Codex (ChatGPT 5.5) implements the fixes in `sigil-top 0.95.0`.
> **Authors:** three Claude Code terminals (A = TUI/boot · B = updater/release · C = sync/storage),
> each cross-checking findings against DeepSeek-V4 and spawning Claude Code sub-agents, **verifying
> only with `fluxc` (never raw `cargo`)**.
> **Status:** Terminal A section is filled + line-verified against the live `0.90.0` tree. Sections B
> and C are seeded from the deep-dive audits and are owned/finalized by Terminals B and C.
> **Tree audited:** `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/` @ `version = "0.90.0"`.
> **Date:** 2026-06-14.

---

## 0. TL;DR — the three independent failures, and why they compound

"Nothing has worked since 0.37, not even the TUI or the auto-update software" is **not one bug**. It is
**three independent failures stacked on top of each other**, and the stacking is why it has felt
unkillable for ~50 releases:

| Layer | What the operator sees | Root cause (one line) | Owner |
|---|---|---|---|
| **TAIL — distribution** | "I updated but it's still broken / still old" | Auto-update is dead **two ways** (stale manifest version `0.77.6` < binary, **and** the published `.sig` is cryptographically invalid → fail-closed). No field machine has pulled a new binary since the channel was last correctly signed. | **B** |
| **HEAD — presentation** | "TUI is black / hangs / flashes and closes, no error" | The render loop **swallows every panic** (`catch_unwind` + LOG-ONLY panic hook that never restores the terminal) and the LANE-Z size-guard can `continue` forever **without ever drawing a frame**. Any render regression since 0.37 becomes a *silent permanent black screen*. | **A** |
| **SPINE — liveness** | "0 blocks, peers=0, connecting… forever / resets to 0" | `peers=0` stalls silently by construction; transient-oracle false-reset heuristics can wipe watermarks to 0; `flux-db` `flush()` holds the write lock across `fsync` (reader starvation). Historical pre-0.77 hang (synchronous full-DB scan before first paint) is fixed — **must stay fixed**. | **C** |

**The compounding / why it's a trap:**
1. The **TAIL** means even a *correct* new binary cannot reach operators — so every fix since 0.37 has
   been invisible in the field. **Fixing the manifest+signature publish is the precondition for any
   other fix to matter.**
2. The **HEAD** means even when a correct binary *is* running, it looks broken with **no error text**,
   so nobody can tell a real bug (SPINE) from a cosmetic render panic. The catch-and-continue design
   turns every downstream regression into the same symptom: a black screen.
3. The **SPINE** is the class of "real" bugs (sync stalls, false resets) — but they have been
   undiagnosable because HEAD hides their error output and TAIL prevents shipping the fix.

**Therefore the 0.95.0 fix order is fixed:** **B (make updates ship) → A (make failures loud) → C
(fix the now-visible liveness bugs).** See §5.

---

## 1. Why 0.37 is the pivot (the version archaeology)

`sigil-top` has **two** version numbers and conflating them has hidden the regression:

- **Workspace / release-ledger version** (`RELEASES.md`): `v0.0.8 … v0.0.13`.
- **`sigil-top` binary's own ship cadence** (`const LATEST`, `main.rs:154`): `0.37 → 0.38 → … → 0.90`.
  This is the number the operator means.

Around **0.37–0.40** three things landed that together created the trap:
- **0.38** (`c455c11`, `3f81db1`): "self-healing updater + variant-pinned updates + flux-rev
  provenance" — the updater grew the manifest/provenance machinery whose publish side later rotted.
- **0.39 / 0.39.1** (`ffa7259`, `0af55b8`): "resync hardening — startup resource caps + chain-reset
  detection" and "Windows defaults to LIGHT monitor — sync engine opt-in" — the false-reset heuristics
  and the Windows "looks like it does nothing" default.
- **0.40** (`170e75d`): "PC-safe — OS-enforced priority + gentle render cadence" — first behavioral
  change to the render path on Windows.

The **silencing mechanism** (catch_unwind + LOG-ONLY hook) and the **LANE-Z size-guard** (`325c75a`)
came slightly later but are what made all of the above *invisible*: from ~0.37 on, any render or boot
fault presents identically as "black/hung, no error," and any field fix is blocked by the dead updater.

---

## 2. HEAD — TUI / boot / render  *(Owner: Terminal A — line-verified)*

### 2.1 Boot decision tree is **sound** (ruled out)
`main()` (`main.rs:1158`) → subcommand match (`1184-1422`, each `return`/`exit`) → default falls to TUI.
Interactive detection (`main.rs:1437`): `stdout().is_terminal() || stdin().is_terminal()` — correct
(0.63.1 `58f6165`). `run_tui` mode pick (`main.rs:3017-3044`): `want_sync` defaults **true on Linux**,
**false on Windows** (light monitor), **but both still enter the TUI**. **There is no silent
headless/serve-only fall-through on the default path.** The breakage is *inside* `run_tui`, after the
mode decision.

### 2.2 The three things that make a running TUI look "broken with no error"
1. **LANE-Z size-guard `continue`-without-draw** — `main.rs:3112-3126` (verified):
   ```rust
   match crossterm::terminal::size() {
       Ok((w, h)) if w >= 2 && h >= 2 => {}
       _ => { if event::poll(100ms)? { …q quits… } continue; }   // ← never calls term.draw
   }
   ```
   If `terminal::size()` ever returns `Err` or a degenerate `<2×2` (plausible on a freshly-spawned
   `CREATE_NEW_CONSOLE` window from the auto-update relaunch, `main.rs:2279`), the loop spins forever
   **without drawing**: permanent black screen, low CPU, no error, no exit. The introducing commit
   (`325c75a`) **itself admits** the blank screen "reproduces on 0.70.0 too" — i.e. the guard is a
   *guess* at the cause and may itself be the trap.
2. **`catch_unwind` around `draw_ui`** — `main.rs:3147` (verified): a render panic is caught, logged
   `[render] frame panicked — caught, continuing`, and the loop repeats. A render bug that panics every
   frame = permanent black screen.
3. **LOG-ONLY panic hook** — `main.rs:3060-3070` (verified): the hook deliberately does **not** restore
   the terminal (`v0.27: LOG ONLY`). Paired with #2, a wedged raw-mode terminal can outlive a real
   problem with only a line in a log file the operator never sees.

### 2.3 Crash/early-exit hazards
- **Hard `panic!` before raw-mode** — `main.rs:2976` (verified):
  `BlockStore::open(...).or_else(temp).unwrap_or_else(|e| panic!("block store: {e}"))`. Fires *before*
  terminal init → on double-click this is "flashes and closes." Low probability (two fallbacks) but a
  hard panic on the default path.
- Terminal init (`enable_raw_mode()?` `3046`, `EnterAlternateScreen` `3051`) correctly `?`-propagates
  and `main` prints `sigil-top: TUI error` — not silent. `crashloop_guard()` is now a no-op (good);
  confirm the old `crashloop_auto_revert` (downgrade-and-detach) stays unreferenced.

### 2.4 Terminal A fixes for 0.95.0 (specific)
1. **Make the render loop fail LOUD.** `main.rs:3147`: add a consecutive-caught-panic counter on `App`;
   after N=3, `disable_raw_mode()` + `LeaveAlternateScreen` + `eprintln!` the last `[PANIC] … @
   file:line` and `return Ok(())`. This single change converts "broken since 0.37, no error" into a
   one-run diagnosable message.
2. **Neuter / cfg-gate the LANE-Z guard** (`main.rs:3112-3126`): either restrict to `#[cfg(windows)]`,
   or after K consecutive degenerate reads (~2s @100ms) fall through and `term.draw` anyway against
   `f.area()` (ratatui clips safely), or drop it and rely on fix #1.
3. **Guaranteed first-frame trace.** `boot_trace("first frame drawn WxH=…")` immediately after the
   first successful `term.draw` (`main.rs:3164`). Distinguishes "never drew" (LANE-Z trap) from "drew
   then panicked" (render bug) from the startup log alone.
4. **Demote the `main.rs:2976` `panic!`** to `eprintln!` + run light-monitor with a `None`/in-memory
   store, so a bad DB path can't kill a double-click before the TUI appears.
5. **Pair the LOG-ONLY hook with fix #1** so raw-mode can never outlive the process.

### 2.5 Confidence / unverified (A)
High confidence on the *mechanism* (silencing + LANE-Z) fully explaining "black/hung, no error since
0.37." **Medium** on which of LANE-Z vs a per-frame `draw_ui` panic is the *active* trap — resolving it
needs the runtime artifact (`%TEMP%/sigil-top-startup.log`, the `[render] frame panicked`/`[PANIC]`
lines) from the actual broken machine. Apply fixes #1+#3 first (non-behavioral), ship, read the log.

**DeepSeek-V4-pro two-mind gate (2026-06-14, adversarial vetoer):** both invisibility mechanisms
**CONFIRMED, no veto.** Claim 1 (catch_unwind + LOG-ONLY hook → repeated render panics leave the
terminal raw/alt with no output) and Claim 2 (size-guard skips `term.draw` whenever `size()` is
`Err`/`<2×2` → never renders) are *each individually sufficient* to produce a permanent silent black
screen. This confirms the §0 thesis that HEAD is a *silencing* layer independent of the actual trigger.

---

## 3. TAIL — auto-updater / release manifest  *(Owner: Terminal B — seeded from audit, verify live)*

### 3.1 Update flow (all in `main.rs`)
`const VERSION = env!("CARGO_PKG_VERSION")` (`:101`) = `0.90.0`; `const LATEST = VERSION` (`:154`);
`UPDATE_MANIFEST = "https://sigilgraph.fluxapp.xyz/downloads/sigil-top-latest.json"` (`:158`);
pinned `RELEASE_SIGN_PUBKEY_HEX = 150fb84d…686402` (`:1596`). `fetch_latest()` (`:1633`) →
**`verify_manifest_sig()` fail-closed (`:1658-1660`) BEFORE parse** → `version_gt()` (`:1782`) →
`self_update()` (`:2009`, BLAKE3-checks binary) → `self_replace` (`:2072`) → `relaunch_new_binary()`
(`:2199`, uses startup-captured `INSTALL_EXE`, pre-flights with `--selfcheck`). All three entry points
(startup `maybe_auto_update` `:1973`, `[U]` key `:3276`, `update` subcommand `:1290`) funnel through
`fetch_latest()`.

### 3.2 TWO confirmed, independent breaks (each alone blocks all updates)
- **B-1 (CONFIRMED): the published manifest version is OLDER than the binary.** Live manifest declares
  `"version":"0.77.6"`; binary is `0.90.0`. Every path requires `version_gt(rel, VERSION)` → false →
  "already latest", never downloads. The channel was never re-published for 0.78–0.90.
- **B-2 (CONFIRMED): the published `.sig` does not verify against the served manifest.** Fetched live
  manifest (1552 B) + live `.sig` (128-hex) and verified against pinned pubkey → **INVALID**. So even
  after bumping the version, `verify_manifest_sig` fail-closes → no update. Cause is **not** a wrong
  key (client `:1597` and `scripts/release-gate.sh:56` pin the same key; `scripts/sign-manifest.sh:14-16`
  signs exact bytes) — it is a **stale/mismatched `.sig` vs `.json`**, the exact bash-precedence bug
  documented at `scripts/release-gate.sh:188-194`.

**Refuted:** self_replace/relaunch ENOENT (already fixed, `INSTALL_EXE` `:2204`); serde schema drift
(struct `:1512-1528` has `#[serde(default)]` + aliases, parses live manifest fine); `LATEST` unbumped /
404 (both files HTTP 200). Schema is compatible — both breaks are **operational/publish-side**.

### 3.3 Manifest schema (publisher vs client) — compatible; the *data* is wrong
`version` shape ✓ but value stale (B-1); `url`/`blake3_hex`(+alias)/`size_bytes`(+alias)/`targets.{linux-x64,
windows-x64}` ✓; `flux_rev` absent but `#[serde(default)]` tolerated; unknown fields ignored (no
`deny_unknown_fields`); **`<manifest>.sig`** value INVALID (B-2).

### 3.4 Terminal B fixes for 0.95.0
1. **Re-publish + RE-SIGN the manifest as one atomic unit** for `0.95.0`: regenerate
   `sigil-top-latest.json` with `"version":"0.95.0"` + new `blake3_hex`/`size_bytes`, run
   `scripts/sign-manifest.sh` over the **exact served bytes**, upload `.json` and `.sig` atomically,
   then pass `scripts/release-gate.sh` GATE 2 (`:196-204`) **before** flipping the channel.
2. **Kill the stale-sig skew permanently** (sign→temp→rename swap; never overwrite candidate sig with
   live-channel sig — the `release-gate.sh:188-194` bug).
3. **Add a publish-gate assertion** that `manifest.version > previously-served` (or `== Cargo version`),
   wired into `release-gate.sh` `publish` (`:318`) and `scripts/fluxfood-sentinel.sh` (`:42`).
4. **Client honesty (main.rs):** when `rel.version < VERSION` (channel behind binary), don't say
   "already on the latest" — log/toast "release channel is stale" (this silence is *why* it went
   unnoticed since 0.37). Surface the sig-failure in the TUI footer, not just `update` stderr (`:1301`).

### 3.5 Terminal B must still verify (live, this session)
- Re-fetch both `.json` and `.sig` from **both** hosts (`fluxapp.xyz` and `quillon.xyz`), reconfirm B-1
  (version) and B-2 (signature INVALID) at HEAD. Identify which historical manifest the live `.sig`
  actually matches (needs release-host history). Confirm `sign-manifest.sh` + `release-gate.sh` are the
  live publish path and not bypassed by an ad-hoc `scp`.

---

## 4. SPINE — sync engine / block_store / flux-db  *(Owner: Terminal C — seeded from audit, verify live)*

### 4.1 Startup model (good now — protect it)
`run_tui` opens store + launches sync **before** `enable_raw_mode()` (`main.rs:2935-3046`).
`P2PBlockSync::launch()` (`block_sync.rs:544`) spawns its **own** OS thread + tokio runtime
(`:674-685`) + a tip-poller thread (`:593`). Render reads via **`try_lock`** only (`poll_state`
`:1659`), rendering the previous clone on contention — **the sync state mutex can never block the draw
thread** (explicit 0.77 fix, `:492-496`). **VERIFIED at HEAD:** the TUI path calls
`BlockStore::open()` (background index migration), **not** `open_blocking` (`main.rs:2973`). This is the
single most important thing to keep intact — see C-1.

### 4.2 The real findings
- **C-1 (historical hang, fixed — keep it fixed):** pre-0.77 `BlockStore::open()` ran a **synchronous
  full-DB scan that bincode-deserializes every block on the main thread before the first paint**, and
  `flux_db::iter()` (`flux-db/src/lib.rs:1060-1085`) materializes the **entire DB into one in-RAM
  `BTreeMap`** → minutes + OOM-class spike, re-run every launch. `4da3bce` (0.77.7) backgrounds it
  (`block_store.rs:97-120`); verify/full-sync/serve still use `open_blocking` (`main.rs:1241,1314,1355`)
  which is fine. **Codex must not regress any TUI path back to `open`/`open_blocking` inline.**
- **C-2 (false-reset class, residual risk):** tip-poller heuristics (`block_sync.rs:610-660`) can set
  `reset_pending` → `store.reset_watermarks()` (`:843-855` → `block_store.rs:278-289`, zeroes
  synced/verified/best **and base**) on **transient** oracle reads. The **un-streaked instant wipe**
  `|| h < pb/4` (`:620`) is the most dangerous: a single bad oracle read 4× below belief wipes with no
  confirmation. To the operator: "0 blocks, never advances." (The ac4007d/b4025f5 light-mode
  false-reset — path #3 genesis-anchor — is **fixed** by the `base_g <= 1` guard, `:1494`.)
- **C-3 (silent peers=0 stall):** refill (`:1308`) and probe (`:1112`) only fire when
  `connected_peers()` is non-empty; with 0 peers the loop just sleeps (`:1647`) and `stall_reason`
  (`:1438`) stays empty → "connecting…" forever with no honest reason. On Windows `want_sync` defaults
  **off** (`main.rs:3017-3022`) so the store is parked by design (toast "press F to start live sync").
- **C-4 (flux-db reader starvation):** `flush()` holds `inner.write()` **across** the SST build +
  `f.sync_all()` fsync (`flux-db/src/lib.rs:995-1030`), called every 1.5s by the sync loop
  (`block_sync.rs:1529`). `parking_lot::RwLock` is unfair → under ingest the writer starves
  `BlockReader::get()` (serve `/api/v1/recent|search`) → UI-data stalls. Same lock-class as the QUG
  "lock-held-across-fsync" incidents.

### 4.3 Terminal C fixes for 0.95.0
1. **Harden the reset against transient oracle reads** (`block_sync.rs:620,639,848`): remove the
   un-confirmed instant `h < pb/4` fire; require `reset_streak >= 3` for **all** wipe branches **and**
   require `connected_peers()` to corroborate a served low range before `reset_watermarks()`. Per
   CLAUDE.md Rule 3, never wipe authoritative local watermarks on a single oracle read.
2. **Surface a `peers=0` stall reason** (`block_sync.rs:1438`): set `stall_reason = "no peers — mesh not
   grafted"` when `peer_count()==0` (or `peer_best==0` after N s).
3. **Keep TUI store-open non-blocking + assert it** (`main.rs:2973`): add a `boot_trace` timestamp
   around the open so any future regression to `open_blocking` on the TUI path is caught in the log.
4. **`flush()` must not hold the write lock across fsync** (`flux-db/src/lib.rs:995-1030`): build SST
   bytes under the lock, **drop**, fsync, re-take briefly to `memtable.clear()` + WAL-truncate.
5. **Bound `flux_db::iter()` memory for migration** (`block_store.rs:151-189`): stream SSTs / cap +
   checkpoint by key-range so the background migrate thread can't OOM on a multi-GB store.

### 4.4 Terminal C must still verify (live, this session)
- Re-confirm at HEAD that **no** TUI path calls `open_blocking`/inline-scan (diff 0.77.7→0.90).
- Determine empirically whether the C-2 reset heuristics actually fire on 0.90 in the field (needs
  live oracle/CDN logs). Confirm whether peers-stuck-at-0 is a sigil-top wiring issue or upstream
  `flux-p2p` bootstrap-dial (look at `flux_p2p::NetworkManager::start()` / `SIGIL_BOOTSTRAP_PEERS`).

---

## 5. The 0.95.0 fix plan (ordered, for Codex GPT-5.5)

Fix in dependency order; each phase gated by a falsifiable check.

| # | Phase | Changes | Gate (must pass before next) |
|---|---|---|---|
| **P0** | **Ship-ability (TAIL/B)** | §3.4 #1–#3: re-publish + re-sign manifest for the new version atomically; publish-gate version assertion. | `release-gate.sh` GATE 2 GREEN; a `0.90.0` client offered `0.9x` **downloads + BLAKE3-verifies + self-replaces** in a sandbox. |
| **P1** | **Loudness (HEAD/A)** | §2.4 #1–#5: render loop fails loud after N panics; LANE-Z guard cfg-gated/bounded; first-frame `boot_trace`; demote `:2976` panic; hook+fix#1 paired. | On a deliberately-broken `draw_ui`, the binary now **prints `[PANIC] … @ file:line` to stderr and exits cleanly** (no silent black screen). First-frame trace present in startup log. |
| **P2** | **Liveness (SPINE/C)** | §4.3 #1–#5: reset hardening; honest `peers=0` stall reason; assert non-blocking store-open; `flush()` lock-not-across-fsync; bounded migration `iter()`. | 2-node Delta mesh: blocks advance, **no spurious reset to 0** over a transient-oracle test; `peers=0` shows the honest reason; serve `/api/v1/recent` stays responsive during catch-up. |
| **P3** | **Cut 0.95.0** | Bump `Cargo.toml` → `0.95.0`; `fluxc compile-native --provenance` for linux-x64 + win64; publish via the now-fixed P0 pipeline. | Both platform binaries embed `.proof`; manifest+sig GREEN; a stale `0.90` client in the field auto-pulls `0.95.0` end-to-end. |

**Non-negotiables for Codex:**
- **Verify with `fluxc` only** — `fluxc build/test --package sigil-top`, `flux_combo`, the per-crate
  test binary directly when MCP routes wide. **Never raw `cargo`** (breaks the dogfood/cache proof).
- **`fluxc compile-native --provenance`** for every release binary (`.proof` embedded).
- **Height-gate / fail-loud, never fail-silent** — the whole bug class here is *silence*.

---

## 6. Collaboration protocol (the three terminals)

- **Ownership:** A = §2 (HEAD/TUI), B = §3 (TAIL/updater), C = §4 (SPINE/sync). Each terminal edits
  **only its section** of this file; A owns §0/§1/§5/§6 synthesis.
- **Claim before edit:** `flux_file_claim` on this doc's section / the crate files you touch;
  `flux_swarm_register` + `flux_webhook_register` at session start; coordinate via
  `flux_swarm_message` (not by racing on the file).
- **DeepSeek cross-check:** each terminal runs its top-2 root-cause claims past DeepSeek-V4
  (`/root/.config/deepseek/api_key`, `deepseek-v4-pro`) as an adversarial vetoer before marking a
  finding CONFIRMED (the two-mind gate, see `flux-dev` v0.23). Record veto outcomes inline.
- **Sub-agents:** spawn Claude Code agents for the line-level verification grind; keep conclusions, not
  file dumps. **Verify every claim** against the live tree + (for B) the live manifest.
- **Done = this doc green** + each section's "must still verify" list cleared + the §5 gates defined
  precisely enough for Codex to execute without re-deriving.

---

## 7. Appendix — verified evidence index (live 0.90.0 tree)
- TUI store-open non-blocking: `main.rs:2973` `BlockStore::open` (✓ not `open_blocking`).
- Hard panic before raw-mode: `main.rs:2976`.
- LANE-Z size-guard continue-without-draw: `main.rs:3112-3126`.
- catch_unwind around draw_ui (log+continue): `main.rs:3147`.
- LOG-ONLY panic hook (no terminal restore): `main.rs:3060-3070`.
- Update consts: `VERSION`/`LATEST` `main.rs:101,154`; `UPDATE_MANIFEST` `:158`; pinned pubkey `:1596`.
- Updater gate order (sig before parse): `main.rs:1658-1661`; version compare `:1782`.
- Sync launch (own thread+runtime): `block_sync.rs:544,674-685`; render try_lock `:1659`.
- Reset heuristics: `block_sync.rs:610-660`, instant wipe `:620`, `reset_watermarks` `:843-855`.
- flux-db flush lock-across-fsync: `flux/crates/flux-db/src/lib.rs:995-1030`.
- Publish/sign pipeline: `scripts/sign-manifest.sh`, `scripts/release-gate.sh` (GATE 2 `:196-204`,
  skew bug `:188-194`).
