# MandatPilot × Quillon Bank — hele idéen og alle funktionerne

*Samlet overblik · 2026-06-16 · rocky*
Relaterede dokumenter: [`MCP_GOV_DENMARK_STATSMINISTERIET.md`](../../MCP_GOV_DENMARK_STATSMINISTERIET.md) ·
[`MANDAT_QUILLON_BANK_KREDIT.md`](../../MANDAT_QUILLON_BANK_KREDIT.md)

---

## 1. Idéen i én sætning

> En normal dansker logger ind med **MitID**, får en **AI-agent** der handler på hans vegne mod
> det offentlige og mod virksomhedsregistre — betaler **$2 for at komme i gang**, kan **låne** lidt
> kredit hvis han mangler, og **ser aldrig en blockchain**. Under motorhjelmen er hver handling
> signeret, conserved og auditerbar på SIGIL-kæden.

Tre lag, der bygger oven på hinanden:

| Lag | Hvad det er | Publikum |
|---|---|---|
| **A · Åben MCP for Danmark** | Pitch til Statsministeriet: borgerens egen agent læser Digital Post / borger.dk / e-Boks gennem et åbent **MCP**-lag, gated af MitID-samtykke, med SIGIL som uforfalskeligt revisionsspor. | Staten, borgeren |
| **B · MandatPilot** | Den **live**, usynlige-kæde forbrugerprodukt: CVR-Verify + CVR-Overvågning. MitID = kontoen, credits forudbetalt ($2 = 100), alt gennem chokepunktet. | Normal dansker / SMV |
| **C · Quillon Bank kredit** *(nyt)* | "Betal senere"-kredit for borgeren + ægte collateral-lån for agenter. Gør "låne penge + bruge agent-money AI" muligt — uden at bryde invisible-chain-reglen. | Borger (usynligt) + agent (krypto-native) |

Onboarding-fladen (panel + AI-agent) er hvordan lag B/C møder kunden i sekunder.

---

## 2. Den røde tråd

Borgeren har i dag en labyrint (90+ selvbetjeningsløsninger, myndighedssprog, oversete ydelser).
En AI-agent kan løse det — **men kun hvis den kan tilgå tingene sikkert og betale for sig.**

- **MCP** giver sikker, samtykke-gated adgang (lag A).
- **MandatPilot-credits** giver agenten en pung at betale per-handling med (lag B).
- **Quillon Bank** lukker det sidste hul: hvad nu hvis kunden ikke har credits endnu? → **lån** dem
  (lag C). Det er agentic-money-tesen anvendt på borgerservice.

---

## 3. Alle funktionerne, lag for lag

### 3.1 Kerne-ledger (`sigil-mandat/src/lib.rs`) — eksisterende

| Funktion | Gør |
|---|---|
| `account_from_mitid(sub) -> WalletId` | **Den usynlige-kæde nøgle.** Konto = `BLAKE3("mandat:acct:"+sub)`. Ingen seed, ingen wallet — MitID-identiteten *er* kontoen. |
| `credits_of(state, acct) -> Amount` | Penge-sikker visning af credit-saldoen (flux-uint, over/underflow umuligt). |
| `topup_credit(state, acct, beløb, h)` | Bygger transitionen der lægger credits til (efter bekræftet Stripe-betaling). Ren funktion. |
| `debit_action(state, acct, cost, h)` | Trækker `cost` credits per produkt-handling → `TREASURY` (conserved). Afviser ved for lav saldo. |
| `commit(state, t, h)` | Skriver transitionen gennem `commit_state_transition`-chokepunktet (cap + integritet). |

**Konstanter:** `CREDITS` (token), `TREASURY` (revenue-wallet).

### 3.2 Onramp (`onramp.rs`)

| Funktion | Gør |
|---|---|
| `credits_for(usd_cents) -> Amount` | Pris: `CENTS_PER_CREDIT = 2` → $2 = 100 credits, $0,02 = 1 credit. |
| `apply_payment(state, acct, payment, h, seen)` | $→credits, **idempotent** (Stripe webhook-replay = no-op via `seen`-sæt). |

### 3.3 Produkt #1 — CVR-Verify (`verify.rs`)

| Funktion | Gør |
|---|---|
| `verify_business(state, acct, claims, reg, h)` | Debit 10 credits, kryds-tjek **MitID × CVR-register**: er personen *tegningsberettiget* for et *aktivt* selskab med matchende CVR? Beriger med konkurs-flag, antal ansatte, branche. |

### 3.4 Produkt #2 — CVR-Overvågning (`monitor.rs`)

| Funktion | Gør |
|---|---|
| `watch_start(state, acct, h)` | Debit 1 credit, begynd at overvåge et CVR. |
| `monitor_check(state, acct, prev, now, h)` | Debit 2 credits, `diff` to snapshots, alarmér ved ændring. |
| `diff(prev, now) -> Vec<Change>` | Detekterer `StatusChanged`, `BankruptcyChanged` (konkurs!), `NameChanged`, `EmployeesChanged`. |

### 3.5 ★ Quillon Bank kredit (`credit_line.rs`) — NYT

**To lag, ét chokepunkt.** Nye ledger-tokens `DEBT` (hvad du skylder) og `COLLAT` (hvad du har
låst), ny wallet `VAULT` (holder låst NATIVE), LTV-konstanter (`DEFAULT_LTV_BPS=6600`,
`MAX_LTV_BPS=7500` — matcher Quillon Bank).

**Borger-lag — usynlig "Betal senere" (treasury-bagstillet):**

| Funktion | Gør | Afviser når |
|---|---|---|
| `advance(state, acct, beløb, limit, h)` | `TREASURY → acct` credits **+** `acct DEBT += beløb`. Hver lånt credit får matchende gæld — aldrig gratis penge. | `debt+beløb > limit` · float tom |
| `repay(state, acct, beløb, h)` | `acct → TREASURY` credits **+** `DEBT -= beløb`. | `beløb > debt` · saldo for lav |

**Agent-lag — krypto-native, eget QUG som sikkerhed:**

| Funktion | Gør | Afviser når |
|---|---|---|
| `borrow_against_collateral(state, agent, collateral, ltv_bps, h)` | Lås `collateral` NATIVE `agent → VAULT` (spejlet i `COLLAT`), advance `collateral × ltv` credits + matchende gæld. NATIVE conserved. | `ltv > 7500` · float/balance tom |
| `repay_and_release(state, agent, beløb, h)` | Repay; ved **fuld** payback frigives **al** collateral `VAULT → agent`. Delvis payback = collateral forbliver låst. | `beløb > debt` |
| `liquidate(state, agent, h)` | Ved default: seize collateral `VAULT → TREASURY`, nulstil `DEBT` + `COLLAT`. Nul-sum. | ingen collateral låst |

**Læse-helpers:** `debt_of(state, acct)`, `collateral_of(state, acct)`.
**Intern:** `apply_ltv(collateral, bps)` = `floor(collateral × bps / 10_000)`.

### 3.6 HTTP-daemon (`mandatd.rs`) — kæden er usynlig for kalderen

| Endpoint | Gør |
|---|---|
| `GET /credits?sub=` | Saldo for en MitID-sub. |
| `POST /topup` | `{sub, usd_cents, event_id}` → idempotent topup. |
| `POST /verify` | MitID×CVR-kryds-tjek (−10). |
| `POST /watch` · `POST /check` | Start/kør overvågning (−1 / −2). |

*Næste skridt:* tilføj `POST /kredit` (advance) + `POST /tilbagebetal` (repay) bag et **friskt MitID-skrive-scope**.

### 3.7 Onboarding-panel (`gui/mandatpilot/index.html`)

JS-funktioner i sidepanelet:

| Funktion | Gør |
|---|---|
| `openPanel()` / `closePanel()` | Vis/skjul højre-drawer. |
| **auto on `load`** | **Agentens prime directive:** efter 650 ms **auto-åbner MitID-modalen** — hurtigste vej i gang. |
| `openMitID()` / `closeMitID()` | Vis/skjul MitID-modal (pulserende, fokus i feltet). |
| `doMitID()` | Verificér (Criipto/Idura OIDC i prod; hurtig stand-in her) → afslør $2-knap. |
| `pay()` | $2-onramp → `mandatd /topup` → vis credits → tilbyd CVR-Verify/Overvågning. |
| `refreshCredits()` | Synk saldo fra backend. |
| `agentSay()` / `setStep()` | AI-bobble-tekst + 3-trins-fremdrift (MitID → $2 → klar). |

**Regel:** ingen krypto-ord — kun *MitID, verificeret, credits, kr, $2*.

### 3.8 AI-agent (`gui/mandatpilot/boot-agent.sh`)

Headless `claude code -p` på MandatPilot-MCP'en. `--append-system-prompt` sætter prime directive:
**"din allerførste handling er at udløse MitID-modalen — vent ikke, spørg ikke."** Taler kun
forbruger-sprog; skrive-handlinger kræver friskt samtykke.

---

## 4. Pengeintegritet — bevist i `tests/chronos_loan.rs`

`CREDITS` og `NATIVE` er **conserved** (flyttes, mintes aldrig). `DEBT`/`COLLAT` er bogførings-claims.
Alt gennem chokepunktet → 21M-cap kan ikke brydes. Invarianter:

- **I1** Ingen gratis credits — advance == matchende DEBT.
- **I2** DEBT ≤ limit (borger) / LTV-cap 75 % (agent).
- **I3** NATIVE conserved — collateral flyttes, mintes aldrig.
- **I4** Σ COLLAT == NATIVE i VAULT (fuzzet, 200 trials).
- **I5** Ingen over-payback; fuld payback frigiver collateral.
- **I6** Cap hellig — `native_supply` uændret over alt.
- **I7** Default er nul-sum — seized collateral dækker tabt float.

---

## 5. Hvor rigtige penge rører — propose-only

Kun **treasury-float** (borger-lag) og **agentens QUG-lån** går mod den rigtige Quillon Bank MCP
(`bank_apply_for_loan`), og **kun propose-only**: agenten foreslår, et menneske bekræfter
(`confirm:true`). Stripe-topup er rigtig fiat → også propose-only. Indtil da: alt simuleres i chronos
mod en simuleret float. Bindinger fra `flux-firma`: propose-only · sim-first · cap hellig · credits
forbliver SERVICE-ONLY for borgeren.

---

## 6. Status (2026-06-16)

- ✅ Design: `MANDAT_QUILLON_BANK_KREDIT.md`
- ✅ Motor: `credit_line.rs` (7 funktioner) + wired i `lib.rs`
- ✅ Sim: `chronos_loan.rs` (14 scenarier, I1–I7) — *kører via `fluxc test`*
- ✅ Onboarding-panel: `gui/mandatpilot/index.html` (MitID-først auto-åbn)
- ✅ AI-agent: `gui/mandatpilot/boot-agent.sh`
- ⬜ Wire `/kredit` + `/tilbagebetal` i `mandatd` bag friskt skrive-scope
- ⬜ Deploy panel til fluxapp.xyz via `flux_ui_deploy`
- ⬜ Operatør-runbook: float-størrelse + likviderings-politik

*Bygget på flux · fluxc · sigil-mandat · SIGIL-chokepunkt. Propose-only. — rocky*
