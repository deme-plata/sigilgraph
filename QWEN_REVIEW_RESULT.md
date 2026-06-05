# Qwen Review — Result (2026-06-01)

## Book: "Shadows in the Chain" — chapter 2 (.tex, 26.6 KB)
**Reviewer: Qwen2.5-72B** (live ollama endpoint). 3 critical passes, fluxc-scored.

| pass | SCORE | PROSE | PACING | THEME | TECH |
|------|-------|-------|--------|-------|------|
| 1 | 78 | 82 | 75 | 80 | 73 |
| 2 | 78 | 85 | 75 | 80 | 70 |
| 3 | 74 | 82 | 75 | 78 | 65 |

- **avg 76.7/100 · SAP 75.6/100** (mean·0.6 + worst·0.4, robustness-weighted the SAP way)
- **Strongest:** PROSE (82–85), THEME-fidelity (78–80).
- **Weakest, every pass: TECH-ACCURACY (65–73)** — SIGIL mechanics (state roots, PoW, ZK) glossed over.
- **Highest-leverage fix (72B verdict):** deepen the technical detail + tighten scene transitions.

## Qwen3.6-27B — status: PULLED but NOT RUNNABLE on this box
- `ollama pull hf.co/unsloth/Qwen3.6-27B-GGUF:Q4_K_M` → **success** (manifest written).
- `/api/generate` → **HTTP 500**. Cause: the box's ollama/llama.cpp build doesn't support the brand-new Qwen3.6 architecture yet, and the 72B is co-loaded (GPU full). Honest negative — not faked.
- To get a real Qwen3.6 review / miner-improvement: serve Qwen3.6 on a box with an ollama build new enough for the arch (or via transformers, where it ran 90% zero-shot earlier), alone on the GPU.

## "Improve the miner with Qwen3.6" — BLOCKED on the same 500
Cannot run Qwen3.6 to suggest miner improvements right now. The miner itself is REAL + measured:
`blake3_miner.cu` on Tesla T4 = **2.364 GH/s**, nvcc compile **1.7s**, BLAKE3 self-verified.
