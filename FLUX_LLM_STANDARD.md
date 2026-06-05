# flux:// — The Flux LLM Standard (v0)

> One URL scheme for every LLM operation across the Flux/SIGIL fabric.
> The URL *is* the API: signed (post-quantum), audited (provenance), backend-stable.

Reference prototype: `quillon.xyz/flux-llm.html`. Companion results: [[DEEPSEEK_R1_VAST_RESULTS]].

---

## 1. Why a scheme, not an SDK

LLM endpoints are ephemeral (a Vast box dies; the model migrates GPU→GPU). Agents that
hard-code `http://34.44.8.118:20852` break on every reschedule. A **resolver-backed URL
scheme** decouples the *intent* (run this prompt on this model) from the *location*
(whichever box currently serves it). When the backend moves, the `flux://` URL does not.

This mirrors what `chronos://` does for data and what the SIGIL chain does for value:
a stable, signed, addressable surface over a moving substrate.

---

## 2. Grammar

```
flux://<verb>?<params>

verb   := prompt | inference | trainmodel
params := key=value (& key=value)*
```

The resolver dispatches by `verb`. Unknown verbs → `NotSupported`. All values are
percent-decoded; `model` and `q`/`data` are required per-verb (below).

---

## 3. The three verbs

### 3.1 `flux://prompt` — one-shot generation
```
flux://prompt?model=deepseek-r1:70b&q=<prompt>&temp=0.3&max=512
```
| param | req | default | meaning |
|---|:--:|---|---|
| `model` | ✓ | — | served model id |
| `q` | ✓ | — | the prompt (percent-encoded) |
| `temp` | | 0.3 | temperature |
| `max` | | 512 | max new tokens |

Resolves to `POST {endpoint}/api/generate {model, prompt:q, stream:false, options}`.
Returns `{response, eval_count, eval_duration}` → the agent computes tok/s.

### 3.2 `flux://inference` — streaming endpoint as a resource
```
flux://inference?model=deepseek-r1:70b&stream=1
```
Opens a token stream (`stream:true`) — SSE of `{response, done}` chunks. For interactive
UIs and long reasoning (R1 "think" blocks). Same backend, `stream:true`.

### 3.3 `flux://trainmodel` — fine-tune on a signed corpus
```
flux://trainmodel?base=qwen2.5-7b&data=chronos://corpus/agentic-money&epochs=3&method=qlora
```
| param | req | meaning |
|---|:--:|---|
| `base` | ✓ | base model to fine-tune |
| `data` | ✓ | corpus URI — a `chronos://` provenance-signed dataset |
| `epochs` | | training epochs (default 3) |
| `method` | | `qlora` (default) — the flux-moe path |

Enqueues a **flux-moe** QLoRA job. Output weights are **SQIsign-signed** and carry a
provenance record (base hash, corpus hash, agent wallet, settle tx). v0 = stub that
returns a job id; the trainer is the flux-moe crate.

---

## 4. Resolution & cross-cutting guarantees

```
flux://<verb>?<params>
        │
        ▼  resolver (maps model → current serving endpoint via the fabric registry)
   ┌────────────────────────────────────────────────────────┐
   │ prompt     → POST {ep}/api/generate   stream:false       │
   │ inference  → POST {ep}/api/generate   stream:true (SSE)  │
   │ trainmodel → enqueue flux-moe QLoRA; weights SQIsign-signed │
   └────────────────────────────────────────────────────────┘
```

Every call is:
- **Signed** — request + response bound by a post-quantum signature (SQIsign + BLAKE).
- **Audited** — a provenance record (who/what/when/which-model) the caller can inspect.
- **Backend-agnostic** — ollama (established archs) or vLLM (day-0 archs like Qwen3.6)
  sit *behind* the resolver. The `flux://` URL never names the backend.
- **Round-robinable** — `inference` can fan across N replicas of one model id (the
  "swarm throughput" path measured at ~37 tok/s aggregate over 6 concurrent agents).

---

## 5. Backend matrix (which engine serves what)

| Arch class | Engine | Why |
|---|---|---|
| Established (qwen2.5, llama3, deepseek-r1, deepseek-coder) | **ollama** | fast, `onstart: ollama serve`, 11434 auto-exposed |
| Day-0 (Qwen3.6, brand-new) | **vLLM / transformers** | ollama's llama.cpp build 500s on unknown archs |
| > 1 GPU of VRAM (R1-671B) | **llama.cpp (split GGUF) or multi-GPU vLLM** | single-A100 cannot hold 671B (see results §6) |

The resolver picks the engine from the model's arch class — the caller never has to.

---

## 6. Mixed-content note (browser callers)

A browser on an HTTPS page (`quillon.xyz`) **cannot** fetch an HTTP ollama endpoint
(mixed-content block) and ollama sends no CORS headers. Browser-side `flux://` callers
MUST route through a **same-origin HTTPS proxy** (Epsilon/q-flux) that forwards to the
backend. Server-side callers (agents) have no such constraint and hit the endpoint directly.

---

## 7. Roadmap verbs (reserved)

- `flux://embed?model=…&text=…` — embeddings.
- `flux://judge?model=…&rubric=…&text=…` — scored evaluation (the book-review path).
- `flux://route?task=…` — let the resolver pick the best model for a task class
  (coder→deepseek-coder, reasoning→r1, balanced→qwen2.5-72b).

---

## 8. Status (v0, 2026-06-01)

| Piece | State |
|---|---|
| `flux://prompt` | ✅ wired (live A100 ollama) |
| `flux://inference` | ✅ wired (stream) |
| `flux://trainmodel` | 🟡 stub → flux-moe QLoRA |
| Dashboard prototype | ✅ `quillon.xyz/flux-llm.html` |
| Same-origin proxy | ⛔ pending (production q-flux change) |
| Signing/provenance | 🟡 spec'd; wire to SQIsign + provenance ledger |
