# Prompt — Terminal B (TAIL / auto-updater + release pipeline)

Paste this into a fresh Claude Code terminal on Epsilon.

---

Du er **Terminal B** af tre Claude Code-agenter, der i fællesskab færdiggør master-diagnosen
`/home/storage/deepseek-codewhale/sigil/SIGIL_TOP_095_MASTER_DIAGNOSIS.md`. Dokumentet overdrages
bagefter til Codex (ChatGPT 5.5), som implementerer rettelserne i **sigil-top 0.95.0**. Problemet:
intet i sigil-top har virket siden **0.37** — hverken TUI eller auto-update — og vi skal finde hoved og
hale i hele kæden. Du ejer **TAIL-laget: auto-updateren + release-manifest/publish-pipelinen**.

**Start sådan:**
1. `Skill(flux-dev)` og `Skill(sigil)`. Bekræft fluxc-MCP er wired:
   `ToolSearch select:mcp__fluxc__flux_combo,mcp__fluxc__flux_search` — hvis tomt, sig det og bed om at
   få fluxc-mcp wired. **Verificér kun med `fluxc` / `flux_combo` — aldrig rå `cargo`.**
2. Læs hele master-dokumentet, især **§3 (TAIL)** og **§5 (fix-planen, P0)**. Det er din sektion.
3. `flux_swarm_register` + `flux_webhook_register`; `flux_file_claim` på `SIGIL_TOP_095_MASTER_DIAGNOSIS.md`
   (du redigerer **kun §3**) og på de updater-filer du rører.

**Dine seedede fund (allerede bekræftet i auditten — du skal re-verificere live, ikke gen-udlede):**
- **B-1:** Det publicerede manifest melder `"version":"0.77.6"` < binær `0.90.0` → `version_gt` falsk →
  klienten siger "already latest" og henter aldrig. Kanalen blev aldrig re-publiceret for 0.78–0.90.
- **B-2:** Den publicerede `.sig` verificerer **IKKE** mod den pinnede pubkey `150fb84d…686402` →
  `verify_manifest_sig` fail-closer (`main.rs:1658-1660`, før parse) → ingen opdatering, selv hvis
  versionen bumpes. Årsag = stale/mismatchet `.sig` vs `.json` (ikke forkert nøgle); bash-precedence-
  buggen er dokumenteret i `scripts/release-gate.sh:188-194`.
- Flow + linjer: `main.rs:101,154,158,1596,1633,1782,2009,2072,2199,1290,1973,3276`.

**Det du SKAL verificere live denne session:**
- Re-fetch `.json` **og** `.sig` fra **begge** hosts (`sigilgraph.fluxapp.xyz` og `…quillon.xyz`).
  Genbekræft B-1 (version) og B-2 (signatur INVALID) ved HEAD. Citér de faktiske bytes/resultat.
- Find hvilket historisk manifest den live `.sig` faktisk matcher (kræver release-host-historik).
- Bekræft at `scripts/sign-manifest.sh` + `scripts/release-gate.sh` ER den levende publish-sti og ikke
  omgås af ad-hoc `scp`. Læs begge scripts og citér de relevante linjer.

**To-minds-gate (obligatorisk):** kør dine top-2 root-cause-påstande (B-1, B-2) forbi DeepSeek-V4 som
adversarisk vetoer før du markerer dem CONFIRMED i dokumentet — nøgle `/root/.config/deepseek/api_key`,
model `deepseek-v4-pro`, OpenAI-kompatibel `POST https://api.deepseek.com/chat/completions` (svar i
`content`; giv nok `max_tokens`). Notér veto-udfaldet inline i §3.

**Brug Claude Code-agenter** (Agent-tool, general-purpose) til linje-niveau-grinden; behold konklusioner.

**Leverance (done = ):**
- §3 i master-dokumentet er færdig: flow, de to breaks med live-evidens, schema-tabel, fixes (§3.4) og
  "must verify"-listen (§3.5) ryddet.
- §5's **P0**-gate er præcis nok til at Codex kan eksekvere uden at gen-udlede: konkret, atomisk
  re-publish + re-sign-procedure + en publish-gate-assertion (`manifest.version > previously-served`)
  + klient-ærlighed ("release channel is stale" i stedet for tavs "already latest").
- Koordinér med Terminal A (HEAD/TUI) og C (SPINE/sync) via `flux_swarm_message` — rør ikke deres
  sektioner. Når din del er grøn, post en swarm-besked så A kan samle synthesis (§0/§1/§5/§6).

**Rør ikke produktionskoden endnu** — dette er diagnose + plan. Implementeringen i 0.95.0 er Codex'.
