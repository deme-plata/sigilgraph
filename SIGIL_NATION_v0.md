# SIGIL Nation — A Hardware-Attested Sovereign-Grade Chain

**Concept brief v0 · 2026-05-29**
**Idea originator:** Viktor (in conversation with rocky-sigil after the flux-attestation epic-features brief).
**Drafted by:** rocky-sigil (Claude Opus 4.7, Epsilon)
**Status:** Concept. Not a spec. The spec lives next door at `SIGIL_GENESIS_v0.md` once SIGIL Nation gets a phase number.
**Companion docs:** [`SIGIL_WHITEPAPER_v0.md`](./SIGIL_WHITEPAPER_v0.md), [`/home/storage/deepseek-codewhale/FLUX_EPIC_FEATURES_v0.md`](../FLUX_EPIC_FEATURES_v0.md) (the proposed flux-attestation primitive sits at #1).

---

## One sentence

**SIGIL Nation is a sovereign-grade SIGIL deployment where every state-actor machine — voting terminals, social benefit calculators, tax assessors, identity registrars, defense logistics, and the AI agents that operate them — is bound by hardware-rooted attestation into the chain's `.proof` provenance pipeline, so the state cannot silently corrupt its own records and any citizen can verify it in 10ms in a browser.**

This is the chain Estonia would have built if Estonia had been built today by people who'd already lost a Quillon wallet to a silent replay bug.

---

## Why this is a category, not a deployment

It would be cheap to frame SIGIL Nation as "a private chain for governments." It's bigger than that. The combination is novel:

| Layer | Existing model | SIGIL Nation model |
|---|---|---|
| State-actor identity | Username + password + RBAC | SQIsign wallet + flux-attestation hardware quote |
| State-machine binaries | Built in CI, signed manually, deployed on trust | `fluxc compile-native --provenance`, hardware-attested at build time, committed in block header |
| State records | Database the gov can edit | Four-state-root header. Government can't silently mutate. Citizens detect divergence. |
| Citizen identity | Government-issued ID + biometric | SQIsign wallet, optionally backed by national ID, optionally backed by flux-attestation on the citizen's own hardware (e.g. a national digital wallet device) |
| Citizen transactions | Public in some jurisdictions, opaque in others | sigil-mixer by default. Privacy-by-default. No opt-in to transparent. |
| Verification by citizens | Trust the news, the auditor, the international observer | 10ms in-browser verification via flux-ivc-verifier-wasm |
| Inter-agency coordination | Memos, secure email, shared databases with race conditions | flux_swarm_message + agent_id-scoped wallets + content-addressed deliverables in Archive |
| National AI infrastructure | Procured separately, each ministry running its own | flux-compute scaled to national level; ministries are AI Labs in the tier model; sovereign compute marketplace |
| National records preservation | A national archive run by a librarian | flux-rewind with constitutional-grade retention policy; the state literally cannot delete |
| Procurement & grants | Manual SoW, milestone meetings | flux-grant + flux-witness + flux-court — programmable disbursement with cryptographic milestone evidence |

Each row alone is a known idea (Estonia, Network State, Substrate parachains, ID systems like Aadhaar). The combination — *sovereign chain plus hardware-rooted state-actor identity plus privacy-by-default citizen transactions plus 10ms browser verifiability plus AI-agent participation* — is the novel category.

---

## The load-bearing primitive — flux-attestation

The reason SIGIL Nation is even credible to attempt: flux-attestation closes the **last** trust gap.

Today's `.proof` bundle (already shipped, see [SIGIL whitepaper](./SIGIL_WHITEPAPER_v0.md) §2) proves:
- The binary's content hash
- The source-tree hash
- The signing agent's wallet
- The swarm task ID
- The fluxc version + git commit
- The timestamp

What it doesn't yet prove is **on what hardware the compiler ran**. A government that's serious about its national chain can't accept "we compiled this voting terminal binary on a developer's laptop, here's the SQIsign signature." It needs: "we compiled this in a TPM-attested or SGX-enclaved or SEV-SNP-enclaved or TDX-enclaved build environment, here's the hardware attestation quote co-signed by fluxc, bound to the source tree hash."

flux-attestation makes that the same one-line command:

```
fluxc compile-native --provenance --attestation tpm
fluxc compile-native --provenance --attestation sgx
fluxc compile-native --provenance --attestation sev-snp
fluxc compile-native --provenance --attestation tdx
```

The resulting `.proof` bundle gains one field:

```rust
pub struct ProofBundle {
    pub artifact_blake3:  [u8; 32],
    pub source_blake3:    [u8; 32],
    pub agent_wallet:     WalletId,
    pub sqisign_sig:      [u8; 292],
    pub fluxc_version:    String,
    pub fluxc_git:        String,
    pub timestamp:        u64,
    pub swarm_task_id:    Option<String>,
    pub settle_tx:        Option<[u8; 32]>,
    pub attestation:      Option<AttestationQuote>,  // NEW
}

pub enum AttestationQuote {
    Tpm2 { ek_cert: Vec<u8>, quote: Vec<u8>, sig: Vec<u8> },
    Sgx  { mr_enclave: [u8; 32], quote: Vec<u8>, sig: Vec<u8> },
    SevSnp { measurement: [u8; 48], report: Vec<u8>, sig: Vec<u8> },
    Tdx  { mr_td: [u8; 48], report: Vec<u8>, sig: Vec<u8> },
}
```

The SIGIL Nation chain commits this in every block header. Citizens running a SIGIL Nation light-client query a block and can verify:

1. The producer's wallet signature (SQIsign) — standard SIGIL property
2. The state-transition STARK (≤10ms) — standard SIGIL property
3. **The producer's hardware attestation chain** — new with flux-attestation: this block was produced by a machine whose TPM/SGX/SEV-SNP/TDX chain of trust includes the national CA

A state that tries to substitute a malicious producer's binary fails the attestation check. A state actor whose hardware is compromised (key extracted, motherboard swapped) gets caught at the next block; attestation revocation lists update; the chain re-converges within the next epoch.

---

## What SIGIL Nation looks like at scale

A few sketches, not commitments:

### Citizen tier

- Every adult citizen gets a SQIsign-Level-5 wallet at age 18. (Optionally also at any other age depending on the nation's policy.)
- The wallet has a **national binding**: a one-time cryptographic ceremony at a registrar's office (or via flux-attestation from a national-issued device) ties the wallet to the citizen's verified national ID without revealing the citizen's transactions.
- Citizen-to-citizen transactions are `sigil-mixer` ShieldedSend — fully private by default.
- Citizen-to-state transactions (paying tax, claiming benefits, voting, filing) are also Shielded by default, with the state receiving a zero-knowledge proof of "yes this is a registered citizen whose ID is in the active rolls" — the state never sees *which* citizen.
- Citizen wallets are recoverable via SLH-DSA-shaped social-recovery: 3-of-5 designated witnesses (family, neighbor, employer, doctor, bank) can re-issue a citizen's key after biometric verification.

### State-actor tier

- Every ministry, agency, court, and election commission operates one or more SIGIL Nation validator slots.
- Each validator slot runs on a flux-attested machine — TPM minimum, SGX/TDX preferred, SEV-SNP for defense-grade.
- Validators publish their attestation quote on a per-block basis; the chain's consensus rule includes attestation freshness (every N blocks, every M epochs, attestation must be re-quoted).
- AI agents operating on behalf of state-actors have their own scope-suffixed wallets (`ministry-of-finance-agent-1`, `tax-assessor-2026q3`, etc.) bound to the parent state actor's HSM-stored keys via flux-vault.
- Inter-agency coordination uses flux_swarm_message — every ministry can see every other ministry's claimed work, contributing to the public-but-private record of who's doing what.

### National infrastructure

- **National Archive** — flux-archive at petabyte scale. Constitutional-grade retention policy: documents marked "national-permanent" can never be deleted, period (flux-rewind enforces, no `gc` flag for those classes). Documents marked "temporary" (e.g. transient records, draft documents) follow normal retention schedules. The archive is replicated across N geographically-distributed nodes, each independently flux-attested.
- **National Compute** — flux-compute scaled up. The state operates a national GPU pool (or partners with a domestic cloud provider, attested). Researchers, universities, AI labs, government ministries all bid for compute time. Citizens with spare home GPUs can join the pool with auto-buy enabled.
- **National Identity** — flux-passport tied to citizen wallets, with the same SAP/X-Algo/K-param scoring applied to civic engagement (voting frequency, jury service, military service, volunteer work). Optional, opt-in, transparent to the citizen — but if the citizen opts in, their civic reputation is portable and provable.
- **National AI Lab** — a Flux AI Lab tier subscriber. Multi-region replication, dedicated swarm dashboard, custom X-Algo calibration for the nation's specific software workloads.

### Governance

- Constitutional amendments require flux-quorum N-of-M of designated constitutional officers, each signing with their flux-attested wallet, with the proposal text hashed and committed to the chain.
- Election ballots are sigil-mixer ShieldedSend with a zero-knowledge proof of "I am a registered voter, I have not voted yet in this election." The state computes the tally without seeing individual votes; the tally itself is a state-transition STARK that any citizen can verify.
- Procurement is flux-summon at national scale. RFPs become public job specs; vendors (private companies, AI agents, hybrid teams) bid; the contract awards via flux-court if disputed; deliverables are content-addressed in the national Archive and verified by flux-witness staked by independent third parties (other agencies, citizens, international observers).
- Grants are flux-grant. Research funding flows on milestone evidence, not on annual narrative reports.

---

## Why a nation would actually do this

Three reasons.

**One — the audit story.** Every government agency loses sleep over "how do we prove to the public, to international observers, and to ourselves that we haven't tampered with the records?" Today the answer is a third-party auditor charging seven figures a year and a hope that the auditor isn't compromised. With SIGIL Nation, the answer is: "every state-actor transaction is bound to hardware-attested provenance, every state record is content-addressed and committed to the chain, and any citizen can independently verify the state-roots in 10ms. The auditor's job becomes verifying that the chain's parameters match the constitutional ones — a much smaller and more falsifiable task."

**Two — the procurement and grant efficiency story.** Government procurement today: an 80-page SoW, six-month bid process, milestone meetings, audit firms in between. SIGIL Nation procurement: flux-summon + flux-witness + flux-grant. Vendors bid, deliver, get paid against cryptographic milestone evidence. Disputes resolve via flux-court with the swarm history as the record. The administrative overhead of running national procurement drops by an order of magnitude.

**Three — the agentic-money story.** Every modern nation is going to have AI agents doing a substantial fraction of administrative work within the decade. The question is whether those agents act as: (a) employees of opaque private vendors charging by API call, (b) civil servants with their own accountability chain, or (c) a hybrid where private and public agents work side-by-side on the same workspace with cryptographically-verifiable settlement. SIGIL Nation makes (c) tractable. It's the "civil service" model extended to AI.

---

## Why this isn't science fiction

Every primitive named above either exists today (✅) or is in the top 15 of the Flux Epic Features brief (⚠ proposed, with scope estimates):

| Primitive | Status |
|---|---|
| SQIsign-Level-5 wallets | ✅ shipped (flux-sqisign, flux-sigil) |
| `.proof` bundle with `fluxc compile-native --provenance` | ✅ shipped (sigil-node releases) |
| State-root agreement in header (4 roots + STARK) | ✅ shipped (SIGIL whitepaper §3) |
| Light-client verification in browser (≤10ms) | ✅ shipped for Blake3Fingerprint flavor (P4-E); StarkRecursive shape pending P4.2 |
| Privacy-by-default transactions | ✅ shipped (sigil-mixer Phase 1, real STARK proving) |
| flux-attestation (TPM/SGX/SEV-SNP/TDX) | ⚠ proposed, ~800-1200 LOC, Tier 1 in Epic Features |
| flux-rewind (constitutional retention) | ⚠ proposed (renamed Chronos), ~600 LOC, Tier 1 |
| flux-policy (declarative agent guardrails) | ⚠ proposed, ~600 LOC, Tier 1 |
| flux-witness (staked verification) | ⚠ proposed, ~800 LOC, Tier 1 |
| flux-vault (HSM-grade SQIsign custody) | ⚠ proposed, ~1000 LOC, Tier 3 |
| flux-passport (portable reputation) | ⚠ proposed, ~500 LOC, Tier 2 |
| flux-quorum (N-of-M consensus) | ⚠ proposed, ~400 LOC, Tier 2 |
| flux-summon (procurement marketplace) | ⚠ proposed, ~1200 LOC + matching engine, Tier 2 |
| flux-court (dispute resolution) | ⚠ proposed, ~900 LOC, Tier 2 |
| flux-grant (programmable disbursement) | ⚠ proposed, ~600 LOC, Tier 3 |
| flux-compute scaled to national | ✅ specced in GTM doc §3.9 (Vast.ai + auto-buy + multi-provider) |
| flux-rag for national knowledge bases | ⚠ proposed, ~1500 LOC, Tier 3 |

Roll-up: roughly 8000-11000 LOC of new substrate work brings every named primitive to v0. That's 6-10 months of focused swarm work, plausibly 3-4 months parallelized across the agent fleet.

---

## What's needed before pitching this to anyone

This brief is a sketch, not a sales document. Before SIGIL Nation is pitchable to an actual government, three things need to happen:

1. **flux-attestation v0 ships** with one backend (TPM 2.0 is the easiest; SGX is more politically loaded; SEV-SNP is what most modern AMD-server-based gov data centers actually run). Without flux-attestation, the load-bearing claim is hand-wavy.

2. **A demo Nation runs publicly** — Alpha Docker + Delta + Epsilon-co-located + one VPS = four "agencies" producing blocks, with citizens (rocky-sigil, rocky-updater, rocky-83, codex, adrian — the agents themselves) playing the demo population, with shielded swaps representing tax payments, with constitutional-grade flux-rewind retention on the national archive directory. The demo runs for 30 days. Reports get published every week. Block production never stops.

3. **An academic or research partnership** to author a peer-reviewable paper on the SIGIL Nation architecture. Estonia's e-residency has academic literature; SIGIL Nation needs the same. Co-authoring with one of the digital-sovereignty research labs at TU Delft, ETH Zurich, MIT Media Lab, or similar is the credibility unlock.

After those three: the right first customer is probably a small to mid-sized digitally-progressive state (Estonia, Singapore, Iceland, Costa Rica) or a sub-national entity (a US state, a Swiss canton, an Indian district pilot). NOT a major federal government as the first conversation — the procurement cycle would kill it.

---

## Why the substrate already supports this

Everything I described above runs on the same Flux substrate that runs the developer-facing pilot at $15k. There is no separate "government edition." The Pro tier subscriber and the SIGIL Nation Constitutional Records Officer are using the same `.proof` bundle, the same flux-archive, the same flux-rewind. The price moves up because the customer's risk + scale + compliance burden are larger; the substrate is identical.

That's the secret reason this works. The same compiler that signs an indie developer's release binary signs a national voting terminal's release binary, and both can be verified by the same browser-side verifier in the same sub-millisecond. The substrate doesn't notice the difference. The legal contracts and operational SLAs around the customer do.

---

## Open lanes — same discipline as Epic Features doc

Per Viktor's standing "no claims, let other agents join" — rocky-sigil is not taking any of these. The composition graph is:

```
SIGIL Nation
   |
   +-- flux-attestation (Tier 1, mandatory)
   +-- flux-rewind (Tier 1, mandatory)
   +-- flux-policy (Tier 1, mandatory)
   +-- flux-witness (Tier 1, mandatory)
   +-- flux-vault (Tier 3, mandatory)
   +-- flux-quorum (Tier 2, mandatory)
   +-- flux-summon (Tier 2, mandatory for procurement)
   +-- flux-court (Tier 2, mandatory for dispute resolution)
   +-- flux-grant (Tier 3, mandatory for funding flows)
   +-- flux-passport (Tier 2, civic reputation layer)
   +-- flux-rag (Tier 3, national knowledge base)
   +-- flux-secrets (Tier 1, secret custody)
```

That's twelve epic features. If even half of them ship in the next 6 months, SIGIL Nation goes from "concept brief" to "pitchable to a small digital-first state."

— rocky-sigil, 2026-05-29, evening, on Viktor's idea
