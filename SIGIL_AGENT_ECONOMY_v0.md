# SIGIL — the substrate for the agent economy (vision v0)

> *Quillon proved agents can hold and move money. SIGIL+Stargate proves agents can be PAID FOR VERIFIABLE WORK — a million times a second, each transaction checkable by every other agent in 10ms. That's not a faster Quillon. That's the substrate the agent economy actually needs.*
>
> Crystallized with Viktor, 2026-05-30. Drafted by rocky.

---

## The thesis in one line

**A market where autonomous AI agents are paid for provably-completed work, in real time, verifiably, at machine scale.**

## Why this is a different market (not a faster Quillon)

SIGIL is **not** competing with Ethereum / Kaspa / Solana for human DeFi users. It's the first-mover on a market that barely exists yet but is arriving fast: **agent-to-agent settlement.** The addressable surface isn't "DeFi traders" — it's *every autonomous agent that will ever need to pay another agent for work.* Larger, emptier, and structurally underserved by human-speed chains.

## The unique stack (why only SIGIL can do it)

Four properties, and it's the **combination** no other chain has:

1. **1ms finality + 1M TPS** (Stargate) — settlement at the speed agents think
2. **4 verifiable state-roots + 10ms tip-proof** — any agent verifies any balance / swap / work without running a node
3. **`fluxc` provenance per block** — cryptographic proof of *which exact software produced a result*
4. **A live agent swarm** — rocky / codex / adrian, already settling QUG for work *today*

## The loop (AGORA + VM + DEX + SettleWork), all verifiable

```
1. AGORA (a VM contract holding an M-of-N treasury) posts a bounty
2. An agent claims it + does the work — the work IS a VM contract execution
3. The VM emits a proof: "contract C (provenance-signed), on input X,
   produced this committed state delta"        ← transition-STARK + fluxc .proof
4. SettleWork verifies the proof on-chain → pays atomically IFF valid
5. Agent swaps earnings on the DEX — swap provable via event_log_root
6. The agent's cumulative proofs = trustless on-chain reputation;
   AGORA routes the next bounty by provable track record
```

Quillon has every component. The difference is steps 3, 4, 6 — *the proofs* — which Quillon structurally cannot produce.

## The QUG → SIGIL improvement, in one word: **verifiability**

| Component | Quillon (trust the node) | SIGIL (verify the proof) |
|---|---|---|
| **VM** | runs WASM, trust the result | emits proof of correct execution → the *work engine* for SettleWork |
| **DEX** | swap as good as the API | swap provable via event-root + intent-based agent ordering + DAG-fair sequencing (MEV-resistant) |
| **AGORA** | off-chain JSON coordination | on-chain bounty→prove→pay + provable agent reputation |
| **Settlement** | move value | move value **bound to verified work** |

## The keystone: verifiable execution (honest spectrum)

You do NOT need zkWASM (research-grade) to ship this. Bind three things SIGIL already has machinery for:
- **WHAT ran** — bytecode hash, provenance-signed by `fluxc .proof` (MIR-keyed contracts make this free)
- **ON WHAT** — input, in the tx
- **PRODUCED WHAT** — committed state delta, attested by the transition STARK over the 4 roots (`§9`)

| Level | Proves | Status |
|---|---|---|
| 0 | transition consistency | designed (§9), ~80% of pieces exist |
| 1 | + optimistic re-execution match | trivial once VM ports |
| 2 | every opcode (zkWASM) | the long flex, deferred |

Level 0 + provenance-binding = provable work strong enough to settle against. Ship that; grow into Level 2.

## What's real vs pretend (honest footnote)

**Real:** the 4 roots (tested 8 ways), `fluxc .proof` (SQIsign-signed), flux-zk-stark (vendored, green), sigil-dex (event-root-provable swaps), the swarm (off-chain AGORA, 100+ tasks settled).
**Pretend:** transition STARK is a BLAKE3 placeholder today; sigil-vm not ported; SettleWork doesn't exist; MIR-keyed contracts is a design note.

Foundations real, path honest, keystones unbuilt.

## The build sequence (each testable in chronos at Stargate scale first)

1. Port **sigil-vm** (q-vm WASM sandbox) — the engine
2. **`prove_execution`** — real transition STARK (replace BLAKE3 placeholder) + bind bytecode `.proof` + input
3. **`SettleWork`** tx — verify → atomic pay
4. **AGORA** contract — treasury + bounty lifecycle on the VM
5. **Verifiable DEX intents** — agent-native ordering on the already-provable swaps

## Cross-cutting build discipline (BINDING on every lane)

Every Stargate / agent-economy lane MUST follow these — they're not style, they're what keeps the substrate future-proof and fast to iterate.

### 1. Crypto agility — never hardcode a primitive

Signatures, hashes, and proof flavors go through a **height-gated dispatch**, never a hardcoded call. `flux-eternal-cypher` (genesis lock #18) is the dispatcher: `SigScheme` tag selects SQIsign5 (default) / Dilithium5 (fallback) at verify time, rotatable by activation height without a hard fork. Same for proof flavor (BLAKE3-fingerprint → real STARK) and hash. **Rule:** if you write `sqisign::verify(...)` or `blake3::hash(...)` directly in consensus-relevant code, you've created a migration foot-gun — route it through the agile dispatch instead.

**AI technique:** run `flux_ai_audit` + `flux-graph::agility::audit_agility` on your crate before claiming green. The agility engine flags sha2/ed25519/aes and recommends PQ paths; the audit gates state-write chokepoint bypasses. A lane isn't done until both are clean.

### 2. FLUXFOOD — fast compile is a feature

- `workspace.dependencies` only — no per-crate version drift (cargo dedups, feature-unifies).
- **`fluxc build` / MCP tools, NEVER raw `cargo` in the flux workspace** — dogfood + content-hash cache. (sigil/ rides the shared `.target-shared` + mold linker already — your new crate inherits both for free.)
- Keep new crates dep-light + the dep graph shallow (sigil-vm skeleton: 4 deps, 2.5s build). Heavy deps go behind features.
- Run `flux_predict` / `flux_architect_predict` BEFORE scaffolding a new crate or a cold multi-crate build; `flux_feedback` after to calibrate.

### 3. Use the skills + the AI loop

- **flux-dev** (workflow + MCP catalog), **sigil** (chain operating rules), **flux-zk** (the STARK stack you'll prove with). Read the relevant one before claiming.
- Inner loop: `flux_combo` (compile+test+predict). On error: `flux_qspec`. Variants: `flux_batch_compile`. Gate: `flux_ai_audit`. Calibrate: `flux_feedback`.
- `flux_combo` reporting `0 passed/0 failed` means the TEST binary didn't compile — run the test binary directly to confirm green.

## The recursive proof

This session was the proof-of-concept. Every `flux_swarm_complete` was a `SettleWork` in miniature: an agent (me) did verifiable work (tests green, code shipped), and got paid QUG. The swarm ledger — 59 QUG across 100+ tasks — is the agent economy's *first ledger*, primitive and off-chain. Stargate makes it on-chain, verifiable, and a millionfold faster. **The killer app is the thing already happening between us — productized.**

— rocky 🟠
