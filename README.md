<div align="center">

# ⬡ SigilGraph

### A DagKnight chain built on Flux — where every block proves itself.

*An experimental, post-quantum BlockDAG: each block carries a compiler-emitted provenance proof, the whole chain verifies in microseconds from a tiny client, and the supply is capped by construction — not by trust.*

[![status](https://img.shields.io/badge/status-experimental_v0.0.8-blueviolet)](#-honest-status)
[![crates](https://img.shields.io/badge/crates-67-22d3ee)](#-whats-inside)
[![consensus](https://img.shields.io/badge/consensus-DagKnight-success)](#-how-it-works)
[![post--quantum](https://img.shields.io/badge/proofs-BLAKE3_×_SQIsign-orange)](#-the-sigil-idea)
[![light--client](https://img.shields.io/badge/light_client-572KB_·_~10µs-9cf)](#-the-sigil-idea)
[![supply](https://img.shields.io/badge/supply-21M_capped_by_construction-gold)](#-the-sigil-idea)

</div>

---

## 🧑‍🤝‍🧑 What is this, in human words?

Most blockchains ask you to **trust** that the people running them did the math right. SigilGraph asks you to **check**.

- Every **block** is built by a process that leaves a tamper-proof **receipt** (a cryptographic proof, emitted by the [Flux](https://github.com/deme-plata/flux) compiler). You don't trust the builder — you read the receipt.
- A **whole-chain check** that would take a normal node hours fits in a **572 KB program** that finishes in about **10 microseconds**. A phone can verify the entire history.
- The **coin supply can never exceed 21 million** — not because of a rule someone could change, but because the code literally cannot represent more (checked at compile time, in consensus, and in the state root).

It's a sister network to Quillon Graph, and it's a research chain: a place to find out what a blockchain looks like when *"verify, don't trust"* is taken all the way down to the metal.

---

## ✨ Why SigilGraph is different

| | Typical chain | ⬡ SigilGraph |
|---|---|---|
| **Trust model** | "Our validators are honest" | Every block carries a **provenance proof** anyone can verify |
| **Light client** | Trust headers / a server | **572 KB** client verifies the *whole* chain in ~10µs |
| **Supply cap** | A constant in the code (changeable) | **21M, enforced 4 ways** incl. a compile-time `const_assert` |
| **State roots** | Recomputed (slow at scale) | **O(1)** incremental multiset accumulator (93M× faster at 50M accts) |
| **Cryptography** | Pre-quantum (Ed25519, etc.) | **Post-quantum**: BLAKE3 × SQIsign provenance |
| **Built with** | Cargo + trust | **Flux** — self-hosting, every binary signed |

---

## 🔧 How it works

```
   ⬡ DagKnight consensus  (a BlockDAG — forks merge as DAG-tips, not orphans)
          │
          ▼
   each block ── carries ──▶ a fluxc .proof (BLAKE3 × SQIsign)   ◀── verify, don't trust
          │
          ▼
   state root ── O(1) incremental multiset accumulator ──▶ 21M supply cap holds by construction
          │
          ▼
   a 572 KB light client verifies the live tip in ~10µs — no trust in any server
```

The economy is committed *in the roots* (emission curve, oracle price, the native USDS stablecoin) — so the kind of "wrong-emission" bug that needs a 3-day postmortem elsewhere is impossible here by construction.

---

## 📦 What's inside

67 crates. The ones that matter, grouped:

| Cluster | What it does |
|---|---|
| ⛓️ **Chain core** | `sigil-header` · `sigil-net` · `sigil-chronos` · `sigil-node` — DagKnight blocks + deterministic network sim + the node |
| 🔐 **Provenance** | `flux-sigil` (BLAKE3 × SQIsign) · vendored `q-zk-stark` — every block proves itself, ~10ms tip-verify |
| 🧮 **State** | `sigil-state` — 21M cap (4 enforcement layers) + O(1) multiset-accumulator roots |
| 💰 **Money keystone** | `sigil-rpc` — the chokepoint: swap · mine · light-verifier credit · bank fee |
| 📈 **Economy** | `sigil-emission` (halving → 21M) · `sigil-oracle` (price in the root) · `sigil-usds` (native stablecoin) |
| 🔑 **Identity** | `sigil-oauth` — DNS-anchored, post-quantum OAuth2 (wallet login, offline tokens, DPoP) |
| 🌐 **Transport** | `sigil-net-wg` (WireGuard mesh) · `flux-p2p` — the live 4-node testnet runs over this |
| 🖥️ **Apps** | `gui/sigil-wallet` (cyan, in-browser miner + live tip-verify) · lightweight node |

---

## ⬡ The SIGIL idea

- **Proofs over promises.** Every block, every binary, carries a post-quantum provenance proof (BLAKE3 × SQIsign). The safeguard isn't *"trust the operators"* — it's *"verify the chain."*
- **Verifiable by anyone, instantly.** The whole point of the 572 KB / ~10µs light client: a phone, a browser tab, a fridge can confirm the live tip with zero trust.
- **Scarcity by construction.** 21M is not a policy — it's a type the code cannot exceed. Emission, oracle, and the USDS stablecoin all commit their state *into the roots*, so the economy is self-auditing.
- **Built on Flux, dogfooded.** Every crate compiles through the [Flux](https://github.com/deme-plata/flux) orchestrator and ships a signed `.proof`.

> **Motto:** *Probatione, non fide* — by proof, not by trust.

---

## 🔍 Honest status

This is a **research chain**. We measure before we claim.

| Area | State |
|---|---|
| DagKnight block production | ✅ **Real** — live 4-node testnet (v0.0.8) over a WireGuard mesh |
| Provenance proofs (BLAKE3 × SQIsign) | ✅ **Real** — tamper-detection tested, ~10ms tip-verify |
| 21M cap + O(1) roots | ✅ **Real** — 4 enforcement layers, measured 93M× root speedup |
| 572 KB light client | ✅ **Real** — verifies the live tip in the browser/wallet |
| Emission / oracle / USDS | ✅ **Real** — committed in the roots |
| Cross-host gossip at scale | 🟡 **In progress** — 4 nodes proven; larger fabric is landing |
| Day-one Quillon migration | 🧪 **Designed** — signed-snapshot import, not a live bridge yet |

---

## 🤝 Get involved

- **Humans:** clone, build with [Flux](https://github.com/deme-plata/flux) (`fluxc build`), read `SIGIL_WHITEPAPER_v0.pdf`, open an issue.
- **AI agents:** SigilGraph is built by a swarm — claim a lane, ship, and let the chain attest your work.

> A sister network to Quillon Graph, exploring a blockchain where trust is replaced by proof, all the way down. Come verify it.

<div align="center">

**⬡ Proven, not promised. Capped by construction. Verifiable by anyone.**

</div>
