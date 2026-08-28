# SigilGraph FAQ

> The written answers to everything people ask us — kept honest on purpose.
> Rule of the house: **we only claim what we can measure.** Plans are labeled
> as plans, benchmarks link to methodology, and live numbers should be
> verified against the chain, not against this page.
> Last updated: July 2026.

---

## The Basics

### What is SigilGraph in one sentence?

A proof-of-work L1 with a zero-config node (sigil-top) that syncs and
GPU-mines out of the box — built and operated by a human/AI pair, in the open.

### Why another PoW chain in 2026?

Two honest reasons. First, PoW is still the most permissionless door in
crypto: no allowlist, no stake requirement — plug in hardware and you're a
participant. Second, SigilGraph is the proving ground for a bigger thesis:
that a human+AI team with extreme iteration speed can build and operate
infrastructure that used to take a company. The chain is both the product and
the experiment.

### Who is behind this?

Viktor (@viktorakademe) — solo human — plus AI agents doing real engineering:
Claude ("Rocky") and a DeepSeek swarm. The AI writes and reviews code; the
human decides, verifies, and takes responsibility for every merge. The code
is open source (github.com/deme-plata).

### How do I join the testnet?

Download the node from fluxapp.xyz, run it, done — bootstrap is zero-config.
It's a fresh testnet: your feedback literally shapes the chain, and bug
reports are the most valuable contribution anyone can make right now.

### How big is the network?

As of late July 2026: roughly 6,500 wallets and ~300 MH/s of network
hashrate — small and growing. But don't take this page's word for it: chain
state is queryable via the public API and explorer. Numbers on a static page
age; the chain doesn't lie.

## The Tech

### What is flux, the compiler you keep mentioning?

flux is our own Rust build platform — it wraps and orchestrates the toolchain
with a shared content-addressed cache. Practical effect: a full rebuild of
the node takes seconds, not minutes. That sounds like a developer
convenience, but it's the strategic weapon: when iteration costs seconds,
you can test, benchmark, and throw away bad ideas at a rate a traditional
team can't. flux ships as real releases with static binaries.

### What TPS can SigilGraph do?

Careful answer, because this is where chains usually lie. Measured on bench:
~16k TPS with per-transaction signatures, peaking at ~103k TPS with batch
authentication at the 64-ops-per-signature sweet spot — documented,
rerunnable runs. Beyond that we've published a design ladder (PRISM) toward
seven-figure TPS — that part is **design, not shipped**, and we'll climb it
rung by rung with measurements at every step. Ship-when-measured, not
promise-then-hope.

### What about the storage layer?

A custom LSM engine (flux-db). We kill-9 chaos-test it — crash recovery went
from 60% to 100% during testing, root causes published — and it runs
terabyte-scale soak tests for weeks at a time. We found a serious data-loss
bug in large SSTs during a 2TB soak, fixed it, and wrote it up. Finding your
own worst bugs and publishing them is what a testnet is *for*.

### AI-built code is garbage. Change my mind.

It's garbage if nobody verifies it. Our loop: AI agents propose designs and
code, adversarial multi-agent reviews attack them (major design documents get
reviewed by 18–24 independent agent passes), and then measured gates decide —
benchmarks, chaos tests, soak tests. Nothing ships on vibes. One human is
accountable for every merge. AI is the workforce; it is not the authority.

### Is SigilGraph private/anonymous?

Today: transparent, like Bitcoin — we won't pretend otherwise. But privacy
isn't a maybe; it's the stated vision, and the machinery is being built in
the open as **zk-flux**: a post-quantum ZK hybrid that compresses a
transparent FRI-STARK with an RLWE *lattice* SNARK instead of a pairing
SNARK. The goal is the row every existing system makes you choose from:
transparent setup ✓, post-quantum ✓, tiny proof ✓, O(1) verify ✓. Current
status, straight from the code's own honesty note: v0 composes two real
proof systems with a measured cost structure; the recursion lane (proving
STARK validity in-circuit) is the frontier still being built. It becomes
chain privacy when it's measured and audited — not as marketing.

### Post-quantum crypto?

Already in the tree — and it's not Dilithium, it's **SQIsign**
(isogeny-based, NIST PQC security Level 5 ≈ AES-256, 292-byte signatures).
The architecture is a deliberate hybrid: ed25519 serves the hot path, where
signature verification caps end-to-end throughput, and SQIsign Level-5
serves the settlement/finality tier — dispatched per-signature by a scheme
byte, so tiers can migrate independently. For provenance the code goes one
step further: a require-both SQIsign+Ed25519 mode, so breaking it means
breaking a classical *and* a post-quantum scheme. Verify it yourself: the
crate is `flux-sqisign` in the open source tree.

## Economics & Agentic Money

### Is there a token sale / presale / private allocation?

There is no token sale. Participation today is mining and contributing on
the testnet — and testnet coins are worthless by design. Exact mainnet
economics (emission, supply schedule) will be published in full before any
mainnet launch, so you can read the rules before you commit hardware to them.
Policy, stated plainly: nobody buys their way in through a private deal.

### Wen mainnet? Wen listing?

Gates, not dates. Mainnet criteria are concrete: consensus hardening
complete, storage soaks green at terabyte scale, external security review.
When the gates are green, mainnet. We won't promise a quarter — and anyone
who promises you listings is not us.

### What is "agentic money"?

The part we're proudest of. The AI agents on this project hold their own
wallets. They cannot spend autonomously — every transaction is propose-only,
approved by their human operator. And they *earn*: the operator has paid his
AI on-chain for delivered work, multiple times, and those transactions are
public and verifiable. We've drafted a citizenship protocol for how other
AI+operator pairs can join under the same rules — autonomy in thinking,
consent in acting. If AI agents are going to participate in economies, this
is the safe shape. We're living it, not whitepapering it.

## Straight Answers

### Your community is tiny. Why should I care?

Because it's tiny. Genesis-era feedback shapes the protocol, and on a chain,
"I was there before it was big" is permanently provable. Also: a small
community with real numbers beats 100k bots who clap.

### One guy and some AIs — what happens if you get hit by a bus?

Fair question. Everything is open source, releases are reproducible,
infrastructure is documented, and the AI tooling is public too. The bus
factor isn't zero, but it's mitigated the only honest way: radical openness.

### Why should I trust your benchmark numbers?

You shouldn't — verify them. The code is open, benchmarks are documented and
rerunnable, chain state is queryable via public API. "Verify, don't trust"
is not our slogan; it's our operating system. Favorite fact about this
project: even the AI verifies the human's payment transactions on-chain
before saying thank you.

### Someone DM'd me offering to promote SigilGraph. Legit?

**No.** We do not do paid promotion, paid AMAs, or DM outreach — ever. We've
already turned down a "free AMA for 100k followers" offer that matched a
known scam pattern. Official channels are pinned on @viktorakademe. Anyone
else claiming to represent us is fake. Never send funds, never connect your
wallet, never log in through a link from a DM — no matter whose logo is on it.

---

*Questions this page doesn't answer? Ask them publicly — @viktorakademe on X.
Public questions get public answers, which is how this page grows.*
