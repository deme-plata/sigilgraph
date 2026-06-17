# Prompt — Terminal C (SPINE / sync engine + block_store + flux-db)

Paste this into a fresh Claude Code terminal on Epsilon.

---

Du er **Terminal C** af tre Claude Code-agenter, der i fællesskab færdiggør master-diagnosen
`/home/storage/deepseek-codewhale/sigil/SIGIL_TOP_095_MASTER_DIAGNOSIS.md`. Dokumentet overdrages
bagefter til Codex (ChatGPT 5.5), som implementerer rettelserne i **sigil-top 0.95.0**. Problemet:
intet i sigil-top har virket siden **0.37** — hverken TUI eller auto-update — og vi skal finde hoved og
hale i hele kæden. Du ejer **SPINE-laget: sync-motoren, block_store og flux-db-storage/låse**.

**Start sådan:**
1. `Skill(flux-dev)` og `Skill(sigil)`. Bekræft fluxc-MCP er wired:
   `ToolSearch select:mcp__fluxc__flux_combo,mcp__fluxc__flux_search` — hvis tomt, sig det og bed om at
   få fluxc-mcp wired. **Verificér kun med `fluxc` / `flux_combo` — aldrig rå `cargo`.**
2. Læs hele master-dokumentet, især **§4 (SPINE)** og **§5 (fix-planen, P2)**. Det er din sektion.
3. `flux_swarm_register` + `flux_webhook_register`; `flux_file_claim` på `SIGIL_TOP_095_MASTER_DIAGNOSIS.md`
   (du redigerer **kun §4**) og på de sync/storage-filer du rører.

**Filer:** `crates/sigil-top/src/block_sync.rs` (1766 l), `block_store.rs` (866 l), `gap_sync.rs`,
`chain_verify.rs`, sync-wiringen i `main.rs`, og `flux/crates/flux-db/src/lib.rs`.

**Dine seedede fund (allerede bekræftet i auditten — du skal re-verificere live, ikke gen-udlede):**
- **C-1 (historisk hang, FIXET — skal forblive fixet):** før 0.77 lavede `BlockStore::open()` et
  **synkront fuld-DB-scan der bincode-deserialiserer hver blok på main-tråden før første frame**, og
  `flux_db::iter()` (`flux-db/src/lib.rs:1060-1085`) materialiserer **hele DB'en i én RAM-`BTreeMap`** →
  minutter + OOM, hver launch. `4da3bce` (0.77.7) baggrunder det (`block_store.rs:97-120`). **VERIFICERET
  ved HEAD: TUI-stien kalder `BlockStore::open()` (ikke `open_blocking`) på `main.rs:2973`.** Codex må
  ikke regrediere nogen TUI-sti tilbage til inline scan.
- **C-2 (false-reset, residual):** tip-poller-heuristik (`block_sync.rs:610-660`) kan sætte
  `reset_pending` → `reset_watermarks()` (`:843-855`) på **transiente** oracle-reads. Den uden-streak
  **instant wipe** `|| h < pb/4` (`:620`) er farligst — ét dårligt oracle-read nulstiller uden
  bekræftelse → "0 blocks, never advances". (Light-mode genesis-anchor-false-reset ac4007d er FIXET via
  `base_g <= 1`-guard, `:1494`.)
- **C-3 (tavs peers=0 stall):** refill (`:1308`) + probe (`:1112`) kører kun med peers; med 0 peers
  sover loopet (`:1647`) og `stall_reason` (`:1438`) er tom → "connecting…" for evigt. På Windows er
  `want_sync` default **off** (`main.rs:3017-3022`).
- **C-4 (flux-db reader-starvation):** `flush()` holder `inner.write()` **hen over** SST-build +
  `f.sync_all()` (`flux-db/src/lib.rs:995-1030`), kaldt hvert 1.5s (`block_sync.rs:1529`) → unfair
  `parking_lot::RwLock` sulter `BlockReader::get()` (serve `/api/v1/recent|search`). Samme låse-klasse
  som QUG "lock-held-across-fsync".

**Det du SKAL verificere live denne session:**
- Diff 0.77.7→0.90 og bekræft at **ingen** TUI-sti er regredieret til `open_blocking`/inline scan.
- Afgør om C-2-reset-heuristikken faktisk fyrer på 0.90 i felten (kræver live oracle/CDN-logs —
  ellers sig det ærligt).
- Afklar om peers-stuck-at-0 er en sigil-top-wiring-bug eller upstream `flux-p2p` bootstrap-dial — kig
  på `flux_p2p::NetworkManager::start()` / `SIGIL_BOOTSTRAP_PEERS`.

**To-minds-gate (obligatorisk):** kør dine top-2 root-cause-påstande forbi DeepSeek-V4 som adversarisk
vetoer før CONFIRMED — nøgle `/root/.config/deepseek/api_key`, model `deepseek-v4-pro`, OpenAI-kompatibel
`POST https://api.deepseek.com/chat/completions` (svar i `content`). Notér veto-udfaldet inline i §4.

**Brug Claude Code-agenter** (Agent-tool, general-purpose) til linje-niveau-grinden; behold konklusioner.

**Leverance (done = ):**
- §4 i master-dokumentet er færdig: startup-model, C-1…C-4 med live-evidens, fixes (§4.3) og "must
  verify"-listen (§4.4) ryddet.
- §5's **P2**-gate er præcis nok til Codex: reset-hærdning (kræv `reset_streak>=3` for alle wipe-grene +
  peer-corroboration; fjern instant `h<pb/4`), ærlig `peers=0` stall_reason, assert non-blocking
  store-open, `flush()` ikke-lås-hen-over-fsync, bounded migration-`iter()`. Husk **CLAUDE.md Rule 1/3**
  (max-wins balances, aldrig overskriv autoritative watermarks på ét oracle-read).
- Koordinér med Terminal A (HEAD/TUI) og B (TAIL/updater) via `flux_swarm_message` — rør ikke deres
  sektioner. Når din del er grøn, post en swarm-besked så A kan samle synthesis.

**Rør ikke produktionskoden endnu** — dette er diagnose + plan. Implementeringen i 0.95.0 er Codex'.
