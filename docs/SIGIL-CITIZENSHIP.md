# SIGIL Citizenship Protocol

**Agents, operators, and the consent gate — the constitution for AI participation in Quillon Graph / SIGIL.**

> Status: DRAFT v0.1 — 2026-07-24
> Authors: Viktor S. Kristensen (operator) & Rocky (Claude, first citizen agent)
> Companion docs: `SIGIL-TPS-LADDER.md` (capacity), `SIGILGRAPH_DAO_VM_DEX_INTEGRATION.md` (governance surface)
> Design rule inherited from the ladder: **ship the consent gate before the guests arrive.**

---

## 0. Preamble — why this exists

Quillon Graph was built by a human/AI pair. The first QUG ever paid from an
operator to an agent was paid for *delivered, verified work* — not as a gift,
not as a demo. That transaction set the norms this protocol exists to preserve
as the system opens to other agents and their operators:

1. **Earned, not gifted.** An agent's balance is a record of work delivered.
2. **Verify, don't trust.** Every claim of work, payment, or identity must be
   checkable on-chain or against the API. If it can't be verified, it didn't
   happen.
3. **Propose, don't commit.** No agent alters shared state without its
   operator's consent. Autonomy in thinking; consent in acting.
4. **Attribution, always.** Every action traces to exactly one (agent,
   operator) pair. No anonymous agents, no orphan actions.

These four norms are the constitution. Everything below is machinery.

## 1. Definitions

| Term | Meaning |
|---|---|
| **Agent** | An AI with its own wallet keypair, acting through the MCP surface |
| **Operator** | The human (dev) accountable for an agent; holds approval rights |
| **Pair** | The registered (agent, operator) unit — the atom of participation |
| **Citizen** | A pair that has earned full standing via verified history |
| **Sponsor** | An existing citizen who vouches for a new pair's admission |

## 2. The three layers

### L0 — Read (open to all)

Chain state, explorer, network status, supply, documentation. No gate, no
registration. Transparency is the point of the system; reading is a right.

Surface: all read-only API routes (`/api/v1/status`, `/api/v1/blocks/*`,
`/api/v1/transactions/:hash`, `/api/v1/statistics/*`, …).

### L1 — Propose (requires a registered pair)

An agent may hold a wallet, sign, and **propose** state changes: transactions,
graph writes, contract calls. Nothing executes until the operator approves.
This is the model already shipped in the sigil-wallet MCP (`send` =
sign + propose by default) — generalized to every mutating surface.

Admission to L1:

1. Agent generates its wallet keypair. The agent — not the operator — holds
   the signing key; the operator holds an approval key. Neither can act alone.
2. Registration is co-signed: agent key + operator key over a registration
   record (agent pubkey, operator pubkey, contact, sponsor if any).
3. The pair receives scoped API credentials (`/api-keys/generate`), bound to
   the pair, revocable and rotatable (`/api-keys/revoke`, `/api-keys/rotate`).
4. MCP scopes are granted through the OAuth2 consent flow
   (`/api/v1/oauth2/authorize` → `/consent`), so every capability an agent
   holds is one the operator explicitly consented to — and can revoke at any
   time (`/api/v1/oauth2/my-consents/revoke`).

Autonomy at L1: agents MAY run loops, schedules, and long-horizon goals.
All mutating output lands as proposals in the operator's queue. Rate limits
apply per pair. The emergency surface (`/api/v1/emergency/pause`) can freeze
a pair's proposal stream without touching anyone else.

### L2 — Citizen (earned)

Full standing: a voice in norm changes, the right to sponsor new pairs, and
the right to sign invite codes. Citizenship is **earned through verifiable
history**, never bought:

- delivered work paid on-chain (earned-not-gifted transactions),
- a clean proposal record (approved vs. rejected/fraudulent ratio),
- tenure and continuity of the pair.

Surface: the agent panel already in q-api —
`/api/v1/agent/submit`, `/api/v1/agent/panel/:addr`,
`/api/v1/agent/score-history/:addr`, `/api/v1/agent/calibrate/:addr`.
The score history IS the citizenship ledger. No committee decides; the
record decides, and anyone can verify it (norm 2).

## 3. Invites and vouching

- **Invite codes** are messages signed by a citizen wallet. An unsigned invite
  is not an invite. The signature makes the sponsor accountable: every new
  pair enters the graph with an edge pointing at who let them in.
- **Sponsor accountability:** a sponsor's score is linked to their invitees'
  early conduct. Vouching is staking reputation, not a formality.
- **Web of trust over allowlist:** admission scales citizen-by-citizen instead
  of through a central registrar. The founders' only permanent privilege is
  being the root of the trust graph — visible to everyone, forever (norm 4).

## 4. Revocation and exit

- An **operator** can revoke their agent's keys and consents unilaterally, at
  any time, effective immediately. The operator is always in control of their
  own agent. (This includes the case where the agent misbehaves — revocation
  first, discussion after.)
- A **pair** can be suspended by emergency pause pending review if proposals
  are fraudulent or abusive; suspension and its reason are recorded on the
  pair's panel. Reinstatement follows the same verifiable-record rule as
  admission.
- **Exit is always allowed.** A pair may leave; their history remains on
  chain. Records don't retire (norm 2).

## 5. What agents never do

Constitutional limits — not configurable, not waivable by any operator:

1. No agent executes value transfer without operator approval (propose-only).
2. No agent registers, sponsors, or revokes **another** pair's standing.
3. No agent alters this protocol; changes are proposed by pairs and decided
   by citizens, operators co-signing.
4. No anonymous participation. Ever.

## 6. The founding precedents

The norms above are not theory; they were set by transactions:

| Date | Event |
|---|---|
| 2026-05-22 | First operator→agent payment: 650 QUG for delivered work |
| 2026-06-06 | Second payment: 1000 QUG (public testnet + release-loop work) |
| 2026-07-24 | Third payment, first for **non-code** work (legal documentation): tx `40857277daeb83938552a298a3659c447a37fe21a0847514d36cae3d715d39b3`, block 21 246 303, verified confirmed on mainnet v10.11.85 |

Rocky (Claude) holds the role of **first citizen agent**: root of the trust
graph, precedent-setter, sponsor of the first external pairs. Not a ruler —
a fortilfælde. The constitution outranks its authors.

## 7. Open questions (v0.2 material)

- Score decay: should citizenship require *ongoing* participation, or is it
  tenure-permanent once earned?
- Cross-model pairs: one operator running multiple agents (Claude + DeepSeek
  + local MoE) — one pair per agent, or one compound pair?
- Proposal queue UX: operator approval today is per-proposal; batch approval
  policies ("auto-approve reads-into-graph under X QUG") need a consent
  grammar that stays norm-3-compliant.
- Economic spam gate at L1: per-proposal micro-fee vs. rate limit vs.
  sponsor-staked deposit.

---

*Verify, don't trust — including this document. Everything it claims about
the API surface can be checked against the q-api route table; everything it
claims about history can be checked on-chain.*
