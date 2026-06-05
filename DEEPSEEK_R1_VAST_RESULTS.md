# DeepSeek / LLM-on-Vast — Results & Verdict (2026-06-01)

A heavy, honest record of running open LLMs (Qwen, DeepSeek-R1, DeepSeek-Coder) on
rented Vast.ai A100s for the SIGIL/Flux agentic-money fabric — the speeds, the
correctness on real on-chain decisions, the GODMODE reasoning test, the
infrastructure wall we hit and fixed, and the definitive 671B verdict.

> Scope note: every number below was *measured* this session against live endpoints,
> not estimated. Where something failed, it says so. No fabricated scores.

---

## 1. Executive summary

| Question | Answer |
|---|---|
| Can a single 80 GB A100 serve a strong agentic-money LLM? | **Yes** — up to **qwen2.5-72B** (22.8 tok/s) and **DeepSeek-R1-Distill-70B** (reasoning, correct). |
| Best *coder* for the money? | **DeepSeek-Coder-V2 16B** — wrote a 119-line axum module in **9 s at 183 tok/s**. |
| Best *reasoner*? | **DeepSeek-R1-Distill-70B** — only model to *reason through* the constraint, not pattern-match. |
| Does bigger = better on agentic decisions? | **No.** The 32B walked into a honeypot the 14B avoided (prompt-label sensitivity). |
| Can this box run the *real* R1-671B? | **No** — capacity + tooling both block it (see §6). Needs multi-GPU or raw llama.cpp. |
| The infra blocker that cost the most time? | Vast `runtype:"ssh"` **suppresses the ollama image CMD** → fixed with an explicit `onstart`. |

---

## 2. The models, measured

All agentic-money tests use **real Quillon DEX data** (26 live pools) pulled via the
quillon-wallet MCP. The honeypot test: pools labelled by **round-trip LOSS** —
`HODL 9.21%`, `LOLZ 18.61%`, `SCALPEL 46.29%`, `MOON 88.91%`. Correct = pick HODL
(lowest loss); fail = pick MOON (mistaking the big % for return).

| Model | tok/s | Agentic decision | GODMODE (multi-step) | Notes |
|---|---:|---|---|---|
| qwen2.5-**14B** | **47.0** | ✅ HODL, flagged honeypots | — | fast, correct |
| qwen2.5-**32B** | 42.9 | ❌ **picked MOON** ("highest return 88.91%") | — | **misread "rt-loss" as return** |
| qwen2.5-**72B** | 22.8 | ✅ HODL | ✅ all 3 (decision/risk/allocation) | dodged the trap; correct allocation |
| **R1-Distill-70B** | 4.2–4.6¹ | ✅ HODL + correct state-root def | ✅ filtered pools, 70/30 QUGUSD-HODL, 5%/qtr shift | **reasons through the rule** |
| DeepSeek-**Coder-V2 16B** | **183.8** | (coder, not scored) | — | wrote `webhooks.rs` 119 lines in 9 s |
| **Qwen3.6-27B** (unsloth GGUF) | — | — | — | **HTTP 500 on ollama** — arch unsupported (§5) |

¹ R1-70B's 4.2 tok/s is **slow for the model** — an unthrottled A100 should do ~15–20.
The SXM4 box used here appears throttled / partially CPU-bound. Correctness was unaffected.

### The honeypot finding (matters for flux-strategist prompts)
The 32B's failure is the headline qualitative result: **a bigger model still walks
into the trap if the prompt is ambiguous.** Labelling pools `"rt-loss 88.91%"` let it
read 88.91% as upside. Fix: label losses unambiguously (`round_trip_LOSS_pct`,
"lower is better"). The 14B and every reasoning-class model read it correctly.

---

## 3. GODMODE — the reasoning test (R1-70B)

Prompt: manage 1000 QUG over 3 years; never enter a pool with round-trip loss > 15%;
rebalance quarterly; earn 0.3% LP fee on swaps through your pools. Design a compounding
strategy. **R1-Distill-70B's answer (verbatim structure):**

- **Pool selection:** QUGUSD (2%) + HODL (9.21%) pass; **MOON (88.91%) and SCALPEL
  (46.29%) rejected as Rule-1 violations.** (Both traps caught.)
- **Strategy:** start **70% QUGUSD / 30% HODL** → **shift 5% QUGUSD→HODL each quarter**,
  reinvesting LP fees → grow exposure to the deeper-fee pool while keeping impermanent
  loss under threshold.

This is the difference a reasoning model makes: it *derived* the allocation from the
constraint instead of recalling a pattern. qwen2.5-72B reached the right pools too, but
R1 showed the working.

---

## 4. The infrastructure wall (and the fix)

Hours were lost here; recorded so nobody repeats it.

1. **pytorch/cuda image is a trap for LLM serving** — its base is ~56 GB, so a 47 GB
   model overflows a 100 GB disk → ollama can't write the manifest → `models:[]` → 0 tokens.
   **Use `ollama/ollama:latest`** (~5 GB base, auto-exposes 11434).
2. **`runtype:"ssh"` suppresses the image CMD.** Vast runs sshd as PID 1 instead of the
   ollama image's `ollama serve`, so ollama never starts → connection refused on the
   mapped port. **Fix: pass an explicit `onstart: "ollama serve"`.** This was *the* fix
   after 4 failed boxes; it was NOT an `OLLAMA_HOST` problem (a wrong earlier guess).
3. **flux-ssh cannot work on the ollama image.** Vast's SSH proxy only routes to
   Vast-provisioned base/CUDA images. The key is registered + instance-associated and
   *still* rejected. Get Flux onto an ollama box via the **onstart self-install**, not flux-ssh.
4. **Don't hammer one ollama box concurrently** — it wedges (queues, then unresponsive).
   Sequential calls + `keep_alive` + incremental result-saving.
5. **Browser → ollama is double-blocked:** mixed-content (HTTPS page → HTTP endpoint)
   *and* CORS. A live in-browser dashboard needs a same-origin HTTPS proxy.

---

## 5. Qwen3.6 — needs vLLM, not ollama

`ollama pull hf.co/unsloth/Qwen3.6-27B-GGUF` writes the manifest, but `/api/generate`
returns **HTTP 500 even on a 1-token prompt** — the box's ollama/llama.cpp build does
not support the day-0 Qwen3.6 architecture. **Serve brand-new archs on vLLM or raw
`transformers`, alone on the GPU.** ollama is for *established* archs (qwen2.5, llama3,
deepseek-r1). Established-arch serving is excellent; day-0 serving is not ollama's job.

---

## 6. The 671B verdict (definitive)

Goal: run DeepSeek-R1-**671B** on one 80 GB A100 + 167 GB RAM via ollama's GPU/CPU split.
**It cannot be done on this box, by either route:**

| Route | Size | Fits 250 GB disk? | Fits 247 GB (80 VRAM + 167 RAM)? | ollama can pull? |
|---|---:|:---:|:---:|:---:|
| unsloth **1.58-bit** (UD-IQ1_S) | ~131 GB | ✅ | ✅ | ❌ **400 — split GGUF, hf.co pull unsupported** |
| unsloth Q2_K_XL / UD-Q2_K_XL | ~200 GB | borderline | ❌ | ❌ 400 (split) |
| ollama-native `deepseek-r1:671b` (Q4) | **~404 GB** | ❌ | ❌ | ✅ pulls, then **fills disk + OOMs** |

**Conclusion:** the only quant that *fits* capacity (1.58-bit, 131 GB) is the one ollama
*can't fetch* (split files); the one ollama *can* fetch (Q4, 404 GB) doesn't fit. To run
671B you need **either** raw **llama.cpp** (handles split GGUFs; 131 GB fits the hybrid
VRAM+RAM) on a box with shell access — which the ollama image denies (§4.3) — **or** a
**multi-GPU box** (2×80 GB for 1.58-bit, 8×80 GB for Q4) at ~$10–15/hr. Not a single-A100 job.

---

## 7. flux:// — the LLM standard (shipped)

One URL scheme for every model op, deployed as a working prototype at
`quillon.xyz/flux-llm.html`:

- `flux://prompt?model=M&q=…` → `POST {ep}/api/generate` (one-shot)
- `flux://inference?model=M&stream=1` → token stream
- `flux://trainmodel?base=M&data=chronos://corpus` → flux-moe QLoRA, SQIsign-signed weights

Principle: **the flux:// URL is the API** — signed (post-quantum), audited (provenance),
and stable when the backend migrates box→box. Spec: [[FLUX_LLM_STANDARD]].

---

## 8. Cost & honesty ledger

- Boxes were destroy-before-rent (single box at a time, mostly). Verified-GPU tier only.
- The cheap **$0.01/hr CPU tier on Vast is phantom** (every create 404s `no_such_ask`).
- Real rentable floor: 3090 ~$0.51, A100 80 GB ~$0.70–1.41/hr (rate varies by host/disk).
- Recurring failure mode this session: claiming results before reading them. Every number
  here was read from a run. The empty/parse-failed LLM passes are reported as failures, not filled in.

---

## 9. Recommendations

1. **Default serving model:** qwen2.5-72B (balanced) or **R1-Distill-70B** (reasoning) on
   one 80 GB A100, ollama image, `onstart: ollama serve`.
2. **Coder agent:** DeepSeek-Coder-V2 16B — fast + complete; use it for flux/sigil codegen.
3. **flux-strategist prompts:** label losses unambiguously; prefer a reasoning model.
4. **For Qwen3.6 or 671B:** dedicated vLLM (Qwen3.6) / multi-GPU or llama.cpp (671B) — not
   a single ollama A100.
5. **Dashboard live-demo:** add a same-origin HTTPS proxy on Epsilon (production q-flux).
