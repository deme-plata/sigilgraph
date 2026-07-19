# SIGIL-Fusion — Proof-of-Useful-Work (BLAKE3 × flux-moe) — DESIGN v0

**Status:** PROPOSAL / unbuilt. Not implemented, not consensus-blessed. A starting point, not a spec to code from as-is.
**Provenance:** invented by DeepSeek **v4-pro** (reasoning trace), synthesized to spec by DeepSeek **v4-flash**, on prompt authored by Claude (Opus 4.8) + operator Viktor, 2026-06-28. Tagged: *verified-by-DeepSeek* (the design), *critiqued-by-Claude* (the assessment at the bottom — READ IT before building).
**Goal:** a mining algorithm where one round both (1) secures consensus via BLAKE3 PoW and (2) contributes real compute to flux-moe / Ollama.

> ⚠️ Per the SIGIL skill RULE 0 and balance-integrity rules: this is a paper design. The load-bearing assumption (bit-identical cross-hardware LLM inference) is UNVALIDATED and is the first thing to prototype. The useful-work lane is N×-redundant (see critique). Do not let "DeepSeek designed it" read as "it works."

---

# SIGIL-Fusion: Proof-of-Useful-Work via BLAKE3 + Committee-Attested flux-moe Inference

## 1. Name + One-Paragraph Summary

**SIGIL-Fusion** is a Proof-of-Useful-Work (PoUW) mining algorithm for the Flux/SIGIL blockchain that fuses BLAKE3 PoW with deterministic, committee-verified LLM inference from the flux-moe queue.  
A miner solves a BLAKE3 hash over the block header, wallet, and a commitment to the LLM output; simultaneously, a randomly selected committee of miners runs the same deterministic inference (derived from the previous block hash) and collectively signs the output.  
The block is accepted if the BLAKE3 difficulty target is met and the aggregated committee signature is valid.  
The reward is split into a base (pure PoW) portion and a utility bonus proportional to the generated tokens, with overall emission capped at 21M and subject to halving.  
When no flux-moe job is available, the algorithm falls back to pure BLAKE3 PoW (no utility bonus).

## 2. Protocol Flow

```
┌──────────────┐         ┌─────────────────────────────┐         ┌──────────────────┐
│  Challenge   │────────→│  1. Determine job (optional) │────────→│   Miner builds   │
│ (prev_hash,  │         │ from flux-moe queue.         │         │  useful-work     │
│  wallet, …)  │         │ Derive deterministic seed   │         │  commitment      │
└──────────────┘         │ (BLAKE3(prev_hash||wallet)) │         │  H(output)       │
                         └─────────────────────────────┘         └────────┬─────────┘
                                                                          │
                                                                          ▼
                         ┌─────────────────────────────┐         ┌────────────────────────────┐
                         │  2. Mine BLAKE3 PoW:        │         │ 3. Collect committee sigs   │
                         │ nonce iter, check           │         │ (≥T of N signatures,        │
                         │ BLAKE3(prev_hash || wallet   │←────────│ aggregated via BLS)        │
                         │   || commit || nonce)        │         │     (gossip protocol)      │
                         │ meets target                │         └────────────────────────────┘
                         └─────────────────────────────┘                      │
                                                                              ▼
                              ┌─────────────────────────────────────────────────┐
                              │ 4. Submit block: Submissions includes nonce,   │
                              │    commit, committee_sig, token_count.         │
                              └─────────────────────────────────────────────────┘
                                                                              │
                                                                              ▼
                    ┌─────────────────────────────────────────────────────────────────────────┐
                    │ 5. Verify:                                                               │
                    │   - Check BLAKE3(target).                                                 │
                    │   - Derive committee from challenge (prev_hash, validator set, VRF).      │
                    │   - Verify aggregated BLS signature over (job_id, commit, token_count).   │
                    │   - If OK, accept block; reward = base + utility_bonus.                   │
                    └─────────────────────────────────────────────────────────────────────────┘
```

**Detailed steps**

1. **Challenge creation**: The protocol builds a `Challenge` containing `prev_hash`, `block_height`, `wallet`, `difficulty_target`, `job_id` (if any), `model_hash`, `prompt_bytes`, `max_tokens`, and a `committee_seed` derived from `prev_hash`.

2. **Useful-work assignment** (if queue non-empty):  
   - flux-moe returns a job (model id, prompt, max_tokens).  
   - The miner computes `seed = BLAKE3(prev_hash || wallet)[0:32]`.  
   - The miner runs the deterministic inference: temperature = 0, greedy decoding, same `seed`.  
   - The result is a byte string `output`.  
   - `commit = BLAKE3(output)`.

3. **BLAKE3 PoW mining**:  
   - The miner chooses a `nonce`.  
   - Computes `h = BLAKE3(prev_hash || wallet || commit || nonce)`.  
   - If `h` is below the target, a valid solution is found.  
   - The miner broadcasts its `commit` and token count to the committee.

4. **Committee attestation**:  
   - A committee of `N` miners is selected by a Verifiable Random Function (VRF) from the previous block’s validator set, using `committee_seed`.  
   - Each committee member independently runs the same deterministic inference (using the same `seed`, `model`, `prompt`, `max_tokens`).  
   - If their computed output has the same `commit` (i.e., outputs are identical), they sign `(job_id, commit, token_count)` with their BLS secret key.  
   - The winning miner collects ≥ `T` partial signatures (e.g., `T = 2/3 N`) and aggregates them into a single BLS multi-signature.

5. **Submission**:  
   - The block’s submission includes: `nonce`, `commit`, `committee_sig` (aggregated BLS+list of signers), `token_count`.  
   - The `DualLaneBlock` carries this in its `phi` (power) lane; the `omega` lane (VDF) remains unchanged.

6. **Verification**:  
   - Recompute `h = BLAKE3(prev_hash || wallet || commit || nonce)` and check `h < target`.  
   - Reconstruct the committee set from `committee_seed` and on-chain staking data.  
   - Verify the aggregated BLS signature over `(job_id, commit, token_count)` against the aggregated public key of the committee members that are listed as signers (signer set is encoded in the block).  
   - If both checks pass, the block is valid; reward is computed as per Section 5.

## 3. BLAKE3 Binding

**Exact bytes hashed:**

```
payload = prev_hash || wallet || commit || nonce
h = BLAKE3(payload)   // output is 32 bytes
```

- `prev_hash`: 32 bytes (little-endian, from previous block header).  
- `wallet`: 32 bytes (public key-hash of the miner).  
- `commit`: 32 bytes (`BLAKE3(output)` of the useful work output).  
- `nonce`: 8 bytes (little‑endian).  

The payload byte order is fixed.

**Target interpretation**:  
A 256‑bit target `T` is defined by the difficulty adjustment (inherited from SIGIL’s BLAKE4). The hash is treated as a 256‑bit little‑endian integer; the solution is valid if `h ≤ T`.

## 4. Useful-Work + Verification Scheme

**flux-moe job constraints**:  
- `model_hash`: SHA256 of the model weights/config used (ensures all participants use the same artefact).  
- `prompt_bytes`: fixed prompt string.  
- `max_tokens`: integer ≤ 512 (bounded for verification cost).  
- `seed = BLAKE3(prev_hash || wallet)[0:32]` (32 bytes, fed to the RNG of the inference engine).  
- **Determinism guarantee**: The inference engine MUST be a deterministic binary identical across all miners (e.g., a statically‑linked Rust+llama.cpp build with deterministic rounding, no GPU, fixed thread count, exact same default sampler config). The binary is distributed as part of the Flux node software (e.g., “flux-moe-deterministic”).  
- The engine outputs `token_count` (number of generated tokens), and the byte‑serialized token sequence.  
- `commit = BLAKE3(output)`.

**Committee selection** (on‑chain VRF):  
- From the previous block, the set of validators (staked miners) is known.  
- `committee_seed` is set to `BLAKE3(prev_hash)` for randomness.  
- A VRF is computed per validator (using their VRF public key registered on‑chain).  
- Validators are sorted by their VRF output (using `committee_seed` as the VRF input). The first `N` (e.g., 51) form the committee.  
- The block proposer includes a bitmask of the signers (subset of the committee that actually signed). The threshold `T` (e.g., 34) applies.

**BLS multi‑signature**:  
- Each committee member generates a BLS signature on `(job_id || commit || token_count)` using their staking key (on BLS12‑381).  
- The block includes:  
  - `signer_bitmask`: a compact bitmask over the committee list (allows up to 256 committee members).  
  - `aggregated_sig`: a BLS point (48 bytes).  
- Verification: the verifier rebuilds the aggregated public key by summing the public keys of the signers indicated in the bitmask. Then checks the pairing equation `e(aggregated_sig, G2) == e(H_to_G1(job_id || commit || token_count), aggregated_pk)`. This is a single pairing computation (~1 ms).  
- The `job_id` is the on‑chain job identifier (or a placeholder constant if no job). The block also optionally includes the `token_count` to compute the bonus.

**Anti‑cheat argument**:  
- **Fake work**: A miner cannot produce a block without a valid aggregated signature. Forging signatures requires corrupting >2/3 of the committee.  
- **Precomputation**: The seed depends on `prev_hash`, which is unknown until the previous block is mined. Thus the LLM output can only be computed after that block.  
- **Replay**: The BLAKE3 hash binds `wallet` and `prev_hash`; even if a miner re‑uses an old output commit, the PoW hash will differ (different wallet or block).  
- **Garbage output**: Committee members run the same deterministic inference; if the miner submits a commit that does not match the true output, the committee will refuse to sign.  
- **Phony token_count**: The committee attests the actual token count; a miner who truncates the output early would get a different output, thus a different commit, which the honest committee would not sign.

**Fallback**: If the flux‑moe queue is empty, the `job_id` is set to 0, `commit` is set to the wallet (or a constant), and the committee attestation is skipped (the block does not include a committee_sig). In this case only the BLAKE3 PoW is required; the miner receives no utility bonus.

## 5. Reward Function

Let `block_subsidy` be the current block reward (in satoshis) according to the 21M cap and halving schedule (halving every ~210,000 blocks).  
Define `utility_share = 0.5` (tunable by protocol upgrade).  

**Reward split**:  
- `base_reward = block_subsidy * (1 - utility_share)` (always minted if PoW valid).  
- `utility_bonus = block_subsidy * utility_share * (token_count / max_tokens)` if useful work is present *and* the committee signature is valid, else zero.  

The `max_tokens` is the `max_tokens` field from the job (canonical constant for the task). `token_count` is the actual token count attested by the committee. If `token_count > max_tokens`, cap at `max_tokens`.  

**Emission control**:  
Both `base_reward` and `utility_bonus` are minted through `credit_share` calls that enforce the 21M cap and halving budget. If the utility_bonus is not minted (e.g., fallback), the budget for that block is proportionally reduced, ensuring total supply does not exceed 21M.  

**Difficulty adjustment**:  
The difficulty target for BLAKE3 is adjusted each epoch (e.g., every 2016 blocks) based on the combined hashrate (BLAKE3 hashes per second). The presence of useful work does not affect difficulty directly, but the utility bonus incentivizes miners to also invest in LLM hardware, thus stabilizing the hashrate indirectly.

## 6. Rust Integration Sketch

```rust
// In sigil_pow.rs or sigil_miner

// ======= Existing structures extended =======
struct Challenge {
    prev_hash: Hash256,
    block_height: u32,
    wallet: PublicKeyHash,
    target: U256,
    // New fields for PoUW
    job_id: Option<u64>,
    model_hash: Hash256,          // SHA256 of the model artifact
    prompt_bytes: Vec<u8>,
    max_tokens: u16,
    committee_seed: Hash256,      // derived from prev_hash in create_challenge
}

struct Submission {
    nonce: u64,
    // Useful-work commitment
    output_commit: Hash256,        // BLAKE3(output)
    // Committee attestation
    committee_sig: Option<BlsSignatures>,  // contains aggregated BLS + signer_bitmask
    token_count: Option<u16>,
}

// ======= DualLaneBlock adaptation =======
// Existing DualLaneBlock { phi: PowSolution, omega: VdfSolution, transactions, ... }
// PowSolution (the POWER lane) now contains:
struct PowSolution {
    nonce: u64,
    output_commit: Hash256,
    committee_sig: Option<BlsAggregatedSig>,
    token_count: Option<u16>,
}

// ======= solve (mining side) =======
fn solve(challenge: &Challenge, flux_moe_client: &FluxMoeClient) -> Result<Submission> {
    // 1. Get job from flux-moe if job_id is present
    let job = if let Some(jid) = challenge.job_id {
        flux_moe_client.get_job(jid)?
    } else {
        return solve_pure_blake3(challenge); // fallback
    };

    // 2. Deterministic inference
    let seed = blake3::hash(&[challenge.prev_hash.as_bytes(), challenge.wallet.as_bytes()].concat());
    let inference_params = InferenceParams {
        model_hash: challenge.model_hash,
        prompt: &challenge.prompt_bytes,
        max_tokens: challenge.max_tokens,
        seed: seed.as_bytes()[..32].try_into().unwrap(),
        temperature: Some(0.0),  // greedy
    };
    let output = flux_moe_client.deterministic_infer(inference_params)?;
    let commit = blake3::hash(&output);

    // 3. Mine BLAKE3 PoW
    let target = challenge.target;
    let mut nonce = 0u64;
    loop {
        let payload = [challenge.prev_hash.as_bytes(), challenge.wallet.as_bytes(), commit.as_bytes(), &nonce.to_le_bytes()].concat();
        let h = blake3::hash(&payload);
        if u256_from_le_bytes(h.as_bytes()) <= target {
            break;
        }
        nonce += 1;
    }

    // 4. Collect committee signatures (via gossip)
    let committee_sig = gather_committee_signatures(challenge, commit, output.len())?;
    // committee_sig includes aggregated signature + signer_bitmask

    Ok(Submission {
        nonce,
        output_commit: commit,
        committee_sig: Some(committee_sig),
        token_count: Some(output.len()),
    })
}

// ======= check_submission (verification side) =======
fn check_submission(challenge: &Challenge, submission: &Submission) -> Result<(bool, u64)> {
    // 1. Verify BLAKE3 PoW
    let payload = [challenge.prev_hash.as_bytes(), challenge.wallet.as_bytes(), submission.output_commit.as_bytes(), &submission.nonce.to_le_bytes()].concat();
    let h = blake3::hash(&payload);
    if u256_from_le_bytes(h.as_bytes()) > challenge.target {
        return Ok((false, 0));  // PoW fails
    }

    // 2. Check committee attestation (if useful work present)
    let job_id = challenge.job_id.unwrap_or(0);
    if let Some(ref sig) = submission.committee_sig {
        // Determine committee from challenge
        let committee = compute_committee(challenge.committee_seed, get_validator_set(challenge.block_height));
        // Check that signer_bitmask is within [T, N] and that aggregated BLS signature verifies
        if !verify_bls_multi_sig(
            &format!("{}{}{}", job_id, submission.output_commit, submission.token_count.unwrap_or(0)),
            &committee,
            &sig
        ) {
            return Ok((false, 0));  // Committee attestation invalid
        }
    } else {
        // No committee sig: must be fallback (no job)
        if challenge.job_id.is_some() {
            return Err("Job present but no committee signature".into());
        }
    }

    // 3. Compute reward
    let block_subsidy = current_block_subsidy(challenge.block_height);
    let utility_share = 0.5;
    let base = block_subsidy * (1 - utility_share);
    let bonus = if submission.committee_sig.is_some() {
        let token_count = submission.token_count.unwrap_or(0).min(challenge.max_tokens) as u64;
        block_subsidy * utility_share * token_count / challenge.max_tokens as u64
    } else {
        0
    };
    let total_reward = base + bonus;

    Ok((true, total_reward))
}

// Helper functions: compute_committee (VRF), verify_bls_multi_sig – provided by sigil_crypto crate.
```

**Notes**:  
- The `deterministic_infer` function in flux‑moe must be re‑entrant and produce identical outputs on all nodes. We provide a specific binary (`flux-moe-deterministic`) that is linked into the node.  
- `gather_committee_signatures` is part of the mining networking layer; the miner listens on a gossip channel for partial BLS signatures from committee members (each signed over `(job_id, commit, token_count)`).  
- The `verify_bls_multi_sig` uses the BLS12‑381 curve with a simple hash‑to‑curve (BLS). The aggregated public key is the sum of the public keys of the signers indicated in the bitmask.

## 7. Security Analysis

### Top 5 Attacks and Mitigations

| Attack | Description | Mitigation |
|--------|-------------|------------|
| **Sybil attack** – An attacker creates many low‑stake validators to dominate the committee. | The committee is selected via VRF weighted by stake; each validator’s probability is proportional to its stake. Sybil low‑stake accounts cannot collectively out‑weight a high‑stake honest majority. |
| **51% committee corruption** – Attacker controls >2/3 of the committee and signs a false output for their own block. | The committee size is large (N=51) and threshold is high (T=34), forcing an attacker to control at least 34 of 51 independent validators. This is economically prohibitive if honest stake is ≥ 1/3+. |
| **Bribery / collusion** – Attacker pays committee members to sign a false commit. | The committee selection is unpredictable (derived from `prev_hash`) and changes every block. Bribing a large random set of validators is costly; any validator caught signing a false output can be slashed (on‑chain fraud proof – see “What stays weak”). |
| **Replay of an old output** – Miner reuses a previous valid commit with a new nonce. | The BLAKE3 PoW binds `wallet`, `prev_hash`; output commitment was generated for a different `prev_hash`, thus the PoW hash changes. The committee signature also binds to the specific job and block; reusing a old signature will fail because the message `(job_id, commit, token_count)` will differ. |
| **Denial of service on committee** – Attack prevents committee members from seeing the miner’s commit or presenting theirs. | The miner broadcasts the commit early in the block window; the network also gossips the commit. Committee members are required to sign or be penalized (stake reduction). Timeouts are generous. The block proposer can also include a fallback (no committee sig) and forgo the utility bonus. |

### Honest assessment of weaknesses / required trust

1. **Determinism of inference engines**: All participants must run the exact same deterministic binary and model with identical rounding. Slight differences (e.g., due to CPU instruction sets, parallel reduction) could cause different outputs, leading to committee refusal to sign. We mitigate by providing a static build and using fixed‑precision (e.g., integer quantized) execution. Still, this introduces a centralization vector (the binary vendor).  
2. **Trust in committee majority**: The algorithm relies on the assumption that at least a majority of the committee is honest. If an attacker can economically dominate ≥1/3 of the validator set, they can prevent non‑malicious blocks from getting committee signatures (liveness) and sign false blocks (safety). This is a standard BFT trust assumption; the stake‑based weight makes it expensive.  
3. **No inherent trustless verification of useful work**: The BLAKE3 PoW alone secures the chain. The useful work is only verified by the committee and is not independently verifiable quickly by a light client. Fraud proofs for committee dishonesty are possible (sign a different output than the one they computed), but detecting and proving such fraud requires re‑running the inference, which is heavy. We can add a challenge window where any node can flag a block’s committee attestation as fraudulent, triggering a re‑run and slashing. However, this adds complexity and delay. The current spec accepts the committee as the verification oracle.  
4. **Model availability and licensing**: All validators must store a copy of the model used for mining (e.g., Qwen2.5‑0.5B‑Q4). This imposes storage and bandwidth requirements. If the model changes (hard fork), all nodes must upgrade quickly.  
5. **Forward secrecy of committee communication**: Collusion attacks are mitigated by unpredictability, but if an attacker can predict the committee for upcoming blocks (e.g., by front‑running the VRF), they could bribe in advance. The VRF output must be revealed only when the committee is formed (done inside the block’s header). However, the seed is public after the previous block; the VRF output of each validator can be pre‑computed, but the attacker would need to know which subset will be the committee – this still depends on the list of validators, which is public. Active bribery requires contacting many validators; the protocol can mitigate by randomly sorting committee members within the block (so the exact set is only known when the block is built). We consider this acceptable.
---

## Engineering assessment (Claude — verify, don't rubber-stamp)

**What's right:** It converged on the only honest architecture — **BLAKE3 secures consensus; the LLM work is a committee-attested *bonus lane* that cannot forge chain security** (the design's own weakness #3 admits the committee is the verification oracle). So a broken/gamed inference lane degrades rewards, never consensus. Anti-replay binding (`commit` in the BLAKE3 preimage with `prev_hash`+`wallet`), the pure-BLAKE3 no-job fallback, and `credit_share`/21M-cap discipline are all sound.

**Two real problems to resolve BEFORE any code:**

1. **The load-bearing assumption is the shakiest part — bit-identical LLM inference across all miners.** The committee scheme needs every node to produce the same `BLAKE3(output)`. Cross-hardware float determinism for transformer inference is genuinely hard (AVX2 vs AVX-512 reduction order, FMA contraction; llama.cpp is *not* bit-identical across CPU SIMD generations). The design waves this away with "static binary + integer quantization." **Prototype this first** — if a deterministic integer-only inference path isn't bit-identical on the real fleet, the committee never reaches consensus and the whole thing degrades to pure BLAKE3.

2. **It's not really "useful" compute — it's N×-redundant verification.** The miner runs the inference once; then N committee members **re-run the same inference** to attest it. One useful inference costs N+1 executions ⇒ ~`1/(N+1)` efficiency. For the stated goal ("mining contributes Ollama compute power"), full re-execution is a weak way to get there. A more honest design has the committee verify *cheaply* (spot-check k tokens, optimistic + fraud-proof window, or a ZK proof of correct decode) rather than full re-run — options that were on the menu but not chosen.

**Recommendation:** good v0 frame, right architecture, but run a sharper second DeepSeek round before coding: (a) specify a concretely-achievable bit-deterministic inference path on the real Ollama/llama.cpp stack — or prove it infeasible and pivot to spot-check/fraud-proof verification; (b) replace full committee re-execution with sub-linear verification so the useful work isn't N×-wasted.
