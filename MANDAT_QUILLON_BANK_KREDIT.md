# Quillon Bank ind i MandatPilot — lagdelt kredit på SigilGraph

**Status:** design / draft v1 · tilføjelse til [`MCP_GOV_DENMARK_STATSMINISTERIET.md`](./MCP_GOV_DENMARK_STATSMINISTERIET.md) + `sigil-mandat` · 2026-06-16 · rocky
**Beslutning (Viktor, 2026-06-16):** *begge lag, lagdelt collateral efter audience, plan + sim først.*

---

## 1. Hvorfor det er en naturlig tilføjelse

"Åben MCP for Danmark" giver borgeren en **kompetent digital fortaler**. MandatPilot
(`sigil-mandat`) er den live, usynlige-kæde forbruger-motor: MitID = kontoen, credits
er forudbetalt, alt går gennem `commit_state_transition`-chokepunktet (conserved, 21M-cap,
`.proof`-auditerbart).

Quillon Bank (collateralized QUGUSD-lån mod QUG, MCP v2.17.0: `bank_apply_for_loan`,
`bank_payback_loan`, `bank_loan_status`, `bank_metrics`) lukker det sidste hul: i dag kan
en bruger kun handle for **forudbetalte** credits. Lån gør agentic-money-tesen hel — *"låne
penge og bruge agent-money AI"* — uden at bryde nogen bindende regel, fordi vi holder de to
publikummer **adskilt i to lag med ét fælles chokepunkt.**

## 2. Den bindende konflikt (og hvordan vi respekterer den)

`sigil-mandat/src/lib.rs` siger eksplicit:

> *Credits er PREPAID og SERVICE-ONLY (ikke en hævbar bank).*

Et QUG→QUGUSD-lån er per definition krypto. Så vi **må ikke** lade en borger se QUG, en
wallet, eller hæve credits som kontanter. Løsning = to lag:

| Lag | Publikum | Hvad de ser | Collateral | Kæde synlig? |
|---|---|---|---|---|
| **BORGER** ("Betal senere") | normal dansker via MitID | *"Kredit: 200 kr, tilbagebetal om 30 dage"* | **MandatPilot-treasury** (treasury'ens float er bagstillet af ÉT Quillon Bank QUGUSD-lån, off-SIGIL, propose-only) | **NEJ** — krypto-ord bannede |
| **AGENT / FIRMA** | krypto-native agent (Codex/Adrian/Rocky) eller flux-firma | QUG, QUGUSD, LTV, collateral | **agentens EGET QUG**, låst i `VAULT` | JA |

Begge lag bruger **samme** `credit_line.rs`-primitiver og **samme** chokepunkt. Forskellen er
hvem der stiller sikkerhed, og hvad UI'et kalder tingene.

## 3. Datamodel (alt i `sigil-state`, ingen ny kæde)

```
CREDITS   (eksisterende token)  — den brugbare saldo
DEBT      (ny token, ledger)    — pr. konto: udestående lånte credits ("hvad du skylder")
COLLAT    (ny token, ledger)    — pr. konto: hvor meget NATIVE den har låst i VAULT
VAULT     (ny wallet)           — holder den faktisk låste NATIVE collateral
TREASURY  (eksisterende)        — MandatPilot-revenue + kredit-float
```

`DEBT` og `COLLAT` er **bogførings-tokens** (ikke-overførbare claims), ikke penge.
`CREDITS` og `NATIVE` er penge og er **conserved** — de flyttes, skabes aldrig.

## 4. Operationerne (rene funktioner → typed `StateTransition` → chokepunkt)

| Funktion | Lag | Effekt | Afviser hvis |
|---|---|---|---|
| `advance(acct, beløb, limit)` | borger | TREASURY→acct CREDITS + acct DEBT += beløb | `debt+beløb > limit` · treasury-float tom |
| `repay(acct, beløb)` | borger | acct→TREASURY CREDITS + acct DEBT −= beløb | `beløb > debt` · acct har ikke creditsene |
| `borrow_against_collateral(agent, collateral, ltv_bps)` | agent | lås NATIVE agent→VAULT (COLLAT+=) + advance CREDITS = ltv×collateral + DEBT+= | `ltv_bps > 7500` · treasury-float tom |
| `repay_and_release(agent, beløb)` | agent | repay; ved **fuld** payback frigives al collateral VAULT→agent | `beløb > debt` |
| `liquidate(agent)` | agent | ved default: seize collateral VAULT→TREASURY, nulstil DEBT+COLLAT | ingen collateral låst |

LTV matcher Quillon Bank: default **6600 bps (66 %)**, max **7500 bps (75 %)**.

## 5. Invarianterne (bevist i `tests/chronos_loan.rs`)

- **I1 — Ingen gratis credits.** Hver advance skaber lige meget DEBT. `Σ udestående DEBT
  == Σ (advancede − tilbagebetalte) credits`.
- **I2 — Limit/LTV holdes.** Udestående DEBT ≤ limit (borger) / ltv×collateral (agent).
- **I3 — NATIVE conserved.** collateral lås→VAULT, frigiv→agent, seize→TREASURY; supply
  uændret (to `SetBalance` netter til nul på supply-tælleren → cap-chokepunktet rører den ikke).
- **I4 — COLLAT spejler VAULT.** `Σ COLLAT over konti == balance(VAULT, NATIVE)` altid.
- **I5 — Ingen over-payback.** `repay ≤ debt`; fuld payback nulstiller DEBT og (agent) frigiver collateral.
- **I6 — Cap hellig.** Ingen låne-operation minter NATIVE; `native_supply` uændret over hele scenariet.
- **I7 — Default er nul-sum.** seize flytter collateral til TREASURY = præcis den tabte float; intet skabes/destrueres.

## 6. Hvor Quillon Bank (off-SIGIL) faktisk rører rigtige penge

Kun borger-lagets **treasury-float** og agent-lagets QUG-lån går mod den rigtige Quillon
Bank MCP — og **kun propose-only**: agenten foreslår `bank_apply_for_loan`, et menneske
bekræfter (`confirm:true`). Indtil da kører hele kredit-livscyklussen i chronos mod en
**simuleret** float. Bindingerne fra `flux-firma`-skill'en gælder uændret:

1. **Propose-only på rigtige penge** (BTC/LN/fiat/QUG-lån). Sim frit; en sat/QUG flytter kun på eksplicit human-confirm.
2. **Sim-first** — dette dokument + `chronos_loan.rs` skal være grønt før noget rigtigt lån.
3. **Cap er hellig** — kredit går gennem `commit_state_transition`; en firma/borger kan ikke printe penge.
4. **Credits forbliver SERVICE-ONLY for borgeren** — kredit-laget er "betal senere for ydelser", ikke en hævbar konto.

## 7. UI-copy (borger-lag — krypto-ord bannede)

```
Du har 0 credits.
  → Køb credits ($2 = 100)            [Stripe]
  → Eller: Betal senere — Kredit op til 200 kr, tilbagebetal inden 30 dage   [MitID-bekræft]

Aktiv kredit: 140 / 200 kr brugt · forfald 16. juli · [Tilbagebetal nu]
```

Ordene *lån, QUG, QUGUSD, collateral, wallet, blockchain* optræder **aldrig** i borger-UI.
Compliance/revisor ser `.proof`-sporet — solgt som **tillid**, ikke krypto.

## 8. Næste skridt efter grøn sim

1. ✅ `credit_line.rs` + `chronos_loan.rs` grøn via `flux_test --package sigil-mandat` (denne PR).
2. ⬜ Wire `advance`/`repay` ind i `mandatd` (HTTP) bag et MitID-friskt samtykke-scope (skrive-scope).
3. ⬜ `flux-id-mcp`: ny tool `kredit_status` / `betal_senere` (borger) + agent-tool der kalder Quillon Bank `bank_apply_for_loan` (propose-only).
4. ⬜ Operatør-runbook: hvor stor en treasury-float backer vi med ét QUGUSD-lån, og likviderings-politik ved default.

*Bygget på flux · fluxc · sigil-mandat · SIGIL-chokepunkt. Propose-only. — rocky, 2026-06-16*
