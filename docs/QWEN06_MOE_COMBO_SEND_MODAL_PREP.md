# qwen3:0.6b × flux-moe MCP combo → wallet Send modal — NEXT-MISSION prep

Prepared by rocky (Claude Opus 4.8), 2026-06-17, on Epsilon. Propose-only. This doc
tees up the next mission; it does NOT implement the send fix yet.

## Goal (operator, via /goal hook)
> "prepare mcp combo to qwen 3 0.6b on epsilon through moe … tasks: make send
>  actually work on wallet [u] shortcut send modal"

## What is verified live (read-from-output, not claimed)

1. **qwen3:0.6b is already pulled in ollama on Epsilon** (`localhost:11434`,
   751.63M params). No GPU rental — runs local, free.
2. **Zero-shot tool-call works.** `POST /api/chat think:false`:
   - `"Send 12 QUG to qnkABCDEF"` → `{"tool_call":"send_qug","to":"qnkABCDEF","amount":12}` (~4.6s cold, ~2.0s warm).
   - Format quirk: emits `{"tool_call":...}` (NOT OpenAI `tool_calls`, NOT Qwen XML). The combo parser MUST accept this shape (+ JSON + XML), per MoE skill lesson #5.
3. **Brittleness (the reason for a gate):** with a clean address it filled
   `{to,amount,memo}` correctly; with a messy recipient (`"sgl1qz9k4 test recipient"`)
   it **dropped `to`** and returned only `{amount,memo}`. A 0.6B model cannot be
   trusted to extract a send unaided.

## The combo wiring (flux-moe)

Entry point: `flux_moe::tool_call(endpoint, model, system, user, tools_json)`
(`crates/flux-moe/src/lib.rs:513`) — auto-splits ollama (`/api/chat`+think:false)
vs OpenAI transport; we use ollama.

```
endpoint = "http://localhost:11434"   (Epsilon-local)
model    = "qwen3:0.6b"
tools_json = [ send_sigil(to:string, amount:number, memo?:string) ]   // matches SendModal fields
```

Money-safety: wrap in `flux_moe::two_mind(endpoint, proposer="qwen3:0.6b",
vetoer=<deepseek-v4 via DEEPSEEK_API_KEY>, system, user, tools_json)` so the
DeepSeek-v4 leg VETOES a malformed / missing-field / wrong-amount send before it
ever reaches the modal. **Propose-only: the combo fills the modal; the human
presses Sign & Send.** Never auto-execute (binding rule #1).

MCP surface: expose as a `mcp__fluxc__flux_*` combo (thin wrapper over
`two_mind` → returns `{to,amount,memo,verdict}`), same style as the existing
`flux_sigil_*` tools. The wallet's Send modal calls it to pre-fill from natural
language ("send 7.5 sgl to … memo …").

## The "make send actually work" target

File: `sigil/gui/sigil-wallet/src/components/SendModal.tsx` (the React wallet
served at :9800 / sigilgraph). The modal itself is sound — it already wires the
REAL path:

```
handleSend → signTransactionForP2P({from,to,amount,memo})   // services/walletAuth.ts
           → useLibP2P().submitTransaction(signed)          // contexts/LibP2PContext  (P2P gossip)
           → qnkAPI.sendTransaction(...)                    // services/api.ts  (HTTP fallback)
```

So "send doesn't work" lives in ONE of three services, NOT the modal UI:
- **signTransactionForP2P** (walletAuth.ts) — signing returns `{success:false}` / bad tx shape?
- **submitTransaction** (LibP2PContext) — known mesh-isolation → `peerCount===0`, so the
  P2P branch is skipped and it falls to HTTP every time. Verify peer wiring.
- **qnkAPI.sendTransaction** (api.ts) — HTTP fallback: does the server route accept the
  signed tx + return a real `transaction_id`? (the historical "ghost-send" class:
  modal shows success but no tx lands — cf. CLAUDE.md v10.11.18-FE ghost-send fix.)

## Next-mission plan (do these in order)
1. Reproduce: open :9800 wallet, send a tiny amount, capture which branch runs
   (peers>0 P2P vs HTTP) and the exact failure/`txHash`.
2. Trace the failing service of the three above to root cause (no swallow).
3. Fix at the root; verify a real tx lands (tx_status confirms on-chain).
4. THEN wire the qwen3:0.6b combo to pre-fill the modal (propose-only + two_mind veto).
5. Verify combo end-to-end via `flux_combo --package flux-moe` (never raw cargo).

## Status
- Combo target verified LIVE + viable on Epsilon. ✅
- Send-modal bug surface located + narrowed to 3 services. ✅
- Implementation = the next mission (not done here). ⏳
