# Åben MCP for Danmark
## AI-agenter i borgerens tjeneste — borger.dk, e-Boks og det offentlige, tilgået gennem MCP
### Open MCP for Denmark — citizen-side AI agents for public services

> *"Kontrolleret gennem MCP får vi excellente borgere."*
> Når det offentlige taler en åben, samtykke-styret agent-protokol, får hver borger en kompetent digital fortaler — og staten får mindre friktion og højere optag af de ydelser, folk har ret til.

**Status:** udkast / draft v1 · Bygget på **flux + fluxc + flux-nation** · Til **Statsministeriet / Digitaliseringsministeriet / Digitaliseringsstyrelsen** · 2026-05-31
**Backing link (tweet):** `https://quillon.xyz/mcp-gov` *(side under udrulning)*

---

## Resumé (1 minut)

Danmark er allerede verdens førende digitale stat: **MitID, borger.dk, Digital Post / e-Boks, sundhed.dk, SKAT**. Men for borgeren er det stadig en labyrint — breve man ikke forstår, ydelser man overser, frister man misser, formularer man udfylder forkert.

**Forslag:** gør de offentlige services tilgængelige gennem **MCP (Model Context Protocol)** — en åben, samtykke-styret standard som AI-agenter taler. Så kan borgerens *egen* agent læse Digital Post, forklare et brev fra Udbetaling Danmark i klart sprog, finde den rette ydelse, forudfylde en ansøgning og minde om frister — **med samtykke, scoped adgang og fuld revisionslog.**

**Resultat:** hver borger får en kompetent digital fortaler. Det er, hvad *"excellent borgerservice"* betyder, når AI gøres til **borgerens** redskab — ikke statens.

## Idéen på én linje

> **Et MCP-lag foran det offentlige = borgerens AI-agent kan handle på borgerens vegne — sikkert, gennemskueligt og under borgerens fulde kontrol.**

---

## Problemet

| I dag | Konsekvens |
|---|---|
| 90+ selvbetjeningsløsninger spredt over borger.dk, e-Boks, SKAT, kommunen, sundhed.dk, ATP/Udbetaling Danmark | borgeren skal selv samle trådene |
| Digital Post i myndighedssprog (paragraffer, frister) | breve forstås ikke → frister misses |
| Ydelser man har ret til | søges ikke — man ved ikke de findes |
| Digital eksklusion | ældre, svage læsere, ikke-dansktalende står af |

En AI-agent kan i princippet løse alt dette — **men kun hvis den kan tilgå services sikkert.** I dag findes ingen standard, sikker adgang: kun skrøbelig scraping eller intet.

## Forslaget: MCP som det offentliges agent-grænseflade

MCP er en åben protokol (Anthropic, 2024) der definerer hvordan AI-agenter tilgår **værktøjer** og **ressourcer** med autentificering — "USB-C for AI ↔ systemer." Et offentligt MCP-lag ville fx eksponere:

- `digital_post.list()` / `read_letter(id)` — læs borgerens post (efter MitID-samtykke)
- `explain(letter)` — forklar brevet i klart sprog, fremhæv frister og handlinger
- `find_ydelse(situation)` — find ydelser borgeren har ret til
- `prefill_application(form, data)` — forudfyld med borgerens egne data; **borgeren godkender**
- `deadlines()` — kommende frister på tværs af myndigheder

Hvert kald: **samtykke-gated (MitID) · scoped · logget · tilbagekaldeligt.** Agenten arbejder for borgeren; staten leverer grænsefladen — ikke kontrollen over agenten.

## Hvorfor MCP (og ikke API'er eller scraping)

- **Åben standard** — ingen leverandørlås; enhver agent kan tale det.
- **Bygget til AI** — værktøjs-/ressource-model, indbygget auth, beskrivelser agenten forstår.
- **Samtykke + audit indbygget** — modsat scraping (skrøbeligt, usikkert) eller bespoke API pr. service.
- **Det er allerede sådan agenter taler med systemer i 2026.**

## Sikkerhed & tillid (ikke til forhandling)

1. **Samtykke først** — intet uden borgerens MitID-bekræftede, scoped samtykke. Default-deny.
2. **Mindste privilegium** — kun de tilladelser borgeren giver, tidsbegrænset, tilbagekaldeligt.
3. **Fuld revisionslog** — hver agent-handling signeres og logges; borgeren (og myndigheden) ser præcis hvad der skete.
4. **Provenans (flux/SIGIL-laget, valgfrit)** — handlinger kan gøres kryptografisk verificerbare og uforfalskelige via SIGIL-kæden: et uafhængigt, manipulationssikkert revisionsspor staten **ikke selv** skal vedligeholde.
5. **GDPR by design** — dataminimering, formålsbinding, borger-kontrol.
6. **Skrivehandlinger kræver frisk, eksplicit samtykke** — læsning og skrivning er adskilte scopes.

## "Excellent borger" — udkommet

- **Borgeren:** en kompetent digital fortaler 24/7, der forstår systemet.
- **Staten:** mindre friktion, færre fejludfyldte formularer, højere optag af retmæssige ydelser, lavere supportbyrde.
- **Demokratisk:** digital inklusion — også for dem, der står af i dag.

## Hvad vi bringer (flux-nation)

Substratet er allerede bygget og testet:

- **`flux-nations`** — identitet, attestering, **statsborgerskab og e-mail** for en verificerbar digital nation (12 tests grønne).
- **SIGIL** — en provenans-signeret kæde hvor hver handling bærer et kryptografisk bevis (4 commitede state-roots, 10µs tip-verifikation, post-kvante signaturer).
- **flux / fluxc** — den AI-native compiler denne besked er kompileret med; deterministisk, signeret output.

→ Dette kan levere det **verificerbare revisions- og samtykke-lag** oven på et offentligt MCP-gateway, uden at staten bygger det fra bunden.

## Pilot (lille · målbar · sikker)

1. **Én service:** e-Boks *"læs + forklar"* — agenten læser Digital Post (efter samtykke) og forklarer breve i klart sprog. **Ingen skrivehandlinger** i fase 1.
2. **Én kommune / borgergruppe**, frivillig deltagelse.
3. **Mål:** forståelse, fristoverholdelse, tilfredshed, supportbelastning.
4. **Fase 2:** skrivehandlinger (forudfyld ansøgning, borger godkender) + flere services.

## Tilbuddet

Vi bygger dette på åben **flux/fluxc** og hjælper gerne — prototype, rådgivning, eller blot dele arkitekturen. **Skriv hvis I vil have hjælp.**

---

## Tweet (forbedret)

> **Hej @Statsministeriet** 🇩🇰 Forestil jer **borger.dk** og **e-Boks** åbnet for AI gennem **MCP**: borgerens egen agent læser Digital Post, forklarer brevet, finder den ydelse du har ret til — med **samtykke, scoped adgang og fuld revisionslog**. Styret gennem MCP = **excellent borgerservice**. Bygget på flux/fluxc. Vil I have hjælp? 👉 quillon.xyz/mcp-gov
>
> *(denne besked er kompileret med fluxc)*

*(Outward-facing: dette er et udkast — du sender det, ikke mig.)*

---

## Appendix A — MCP-server skitse (`dk-borger` gateway foran e-Boks / borger.dk)

```
MCP server: dk-borger
  resources:
    digital-post://inbox        # listed only after MitID consent, scoped
    borger://profile            # name, address, civil status (read, scoped)
  tools:
    list_post(folder?)              -> [{id, from, subject, received, deadline?}]
    read_letter(id)                 -> {sender, body, attachments, legal_refs}
    explain(letter_id, lang="da")   -> plain-language summary + deadlines + actions
    find_ydelse(situation)          -> [{ydelse, eligibility, how_to_apply, link}]
    prefill_application(form, prof) -> draft  (citizen REVIEWS + signs; never auto-submit)
    deadlines()                     -> cross-agency upcoming dates
  auth:    MitID-backed OAuth, scoped + time-boxed
  audit:   every call signed + appended to a citizen-visible log
           (optionally SIGIL-anchored for tamper-evidence)
  consent: per-scope, revocable, default-deny; write-scopes need fresh consent
```

## Appendix B — samtykke- & revisionsflow

```
citizen → MitID login → grants scope {read_post, explain}      (NOT write)
agent   → list_post() → read_letter(42) → explain(42)
          every call → signed entry in citizen's audit log
          write tools (prefill/submit) → require a fresh, explicit consent step
citizen → can revoke any scope instantly; the audit log is immutable
          (anchored to SIGIL → neither state nor agent can alter it retroactively)
```

## Appendix C — hvorfor Danmark, hvorfor nu

- Danmark har **infrastrukturen** (MitID, Digital Post, borger.dk) — det der mangler er **agent-grænsefladen**.
- MCP er en **moden, åben standard** i 2026 — vinduet er nu, før hver myndighed bygger sin egen inkompatible bro.
- Et lille, samtykke-først pilot er **lav risiko, høj signalværdi** — Danmark kan sætte den europæiske standard for *citizen-side* agentisk forvaltning.

---

*Bygget på flux · fluxc · flux-nation · SIGIL. Til borgerens tjeneste. — rocky, 2026-05-31*
