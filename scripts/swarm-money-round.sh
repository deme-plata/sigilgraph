#!/usr/bin/env bash
# swarm-money-round.sh — reusable agentic-money round for the NEXT Vast swarm event.
# Baked-in lessons from the 2026-06-01 session:
#   • deepseek-r1 has NO ollama tool-calling ("does not support tools") AND
#     format:json starves on <think> → 0/6. So: auto-detect tool support;
#     for reasoning models use free-form + HIGH num_predict + lenient extraction
#     (strip <think>, take last {...}, else regex dir+amount from prose).
#   • Verify the model is actually SERVING before the round (vLLM OOM showed 0.9G VRAM).
#   • Every action passes a Verified Execution Gate (dir whitelist, amount clamp,
#     balance check) before it ever hits the chain.
#
# Usage: swarm-money-round.sh <OLLAMA_URL> <MODEL> [N_SWAPS] [RPC_URL]
#   e.g. swarm-money-round.sh http://1.2.3.4:39768 qwen2.5:32b 8
set -u
B="${1:?ollama url, e.g. http://IP:PORT}"
MODEL="${2:?model, e.g. qwen2.5:32b or deepseek-r1:14b}"
N="${3:-6}"
RPC="${4:-http://127.0.0.1:8099}"
# v0.36.1: the /swap auth gate requires the `from` wallet to SIGN each request,
# so the trader must be a wallet we hold the key for — derive it from a seed and
# onboard it (which funds it from the finite OPERATOR faucet). The old hardcoded
# 0x11..11 wallet had no signable key. Override TRADER_SEED for a distinct trader.
TRADER_SEED="${TRADER_SEED:-1111111111111111111111111111111111111111111111111111111111111111}"
# Locate the reference signer (sigil-sign). Override with SIGIL_SIGN=/path.
SIGN="${SIGIL_SIGN:-$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)/../.target-shared/debug/sigil-sign}"
[ -x "$SIGN" ] || SIGN=sigil-sign   # else expect it on PATH
TRADER=$(curl -s "$RPC/onboard" -d "{\"seed\":\"$TRADER_SEED\"}" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("wallet",""))')
[ -n "$TRADER" ] || { echo "onboard failed (rate-limited or faucet empty) — set TRADER_SEED / check $RPC"; exit 1; }
echo "trader (seed-derived, signed): $TRADER"
POOL=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
USDS=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
WQUG=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
bal(){ curl -s "$RPC/balance?wallet=$TRADER&token=$1" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("balance",0))'; }

echo "### PRE-FLIGHT ###"
SERVING=$(curl -s --max-time 10 $B/api/tags | python3 -c 'import sys,json;m=json.load(sys.stdin).get("models",[]);print(len(m))' 2>/dev/null || echo 0)
[ "$SERVING" = "0" ] && { echo "ABORT: ollama not serving any model at $B"; exit 1; }
echo "ollama serving $SERVING model(s) at $B"
# tool-support probe (cheap, returns instantly with an error if unsupported)
TOOLS=$(curl -s --max-time 20 $B/api/chat -d "{\"model\":\"$MODEL\",\"stream\":false,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"noop\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}}]}")
if echo "$TOOLS" | grep -q "does not support tools"; then MODE=freeform; NP=4000; else MODE=tools; NP=1200; fi
echo "model=$MODEL  tool_support=$([ $MODE = tools ] && echo yes || echo NO)  → mode=$MODE  num_predict=$NP"

echo "### HEAVY AGENTIC-MONEY ROUND ($N swaps) ###"
echo "start: USDS=$(bal $USDS) wQUG=$(bal $WQUG)"
OK=0; VOL=0; LP=0; PF=0
for i in $(seq 1 $N); do
  echo "===== swap $i/$N ====="
  P=$(curl -s $RPC/pools)
  RA=$(echo "$P" | python3 -c 'import sys,json;print(json.load(sys.stdin)["pools"][0]["reserve_a"])')
  RB=$(echo "$P" | python3 -c 'import sys,json;print(json.load(sys.stdin)["pools"][0]["reserve_b"])')
  UB=$(bal $USDS); WB=$(bal $WQUG)
  SYS="You are a SIGIL DEX agent. Pool USDS=$RA wQUG=$RB (30bps). You hold USDS=$UB wQUG=$WB. Choose ONE small swap."
  if [ "$MODE" = tools ]; then
    RESP=$(curl -s --max-time 120 $B/api/chat -d "{\"model\":\"$MODEL\",\"stream\":false,\"messages\":[{\"role\":\"user\",\"content\":\"$SYS Call swap.\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"swap\",\"parameters\":{\"type\":\"object\",\"properties\":{\"dir\":{\"type\":\"string\",\"enum\":[\"AtoB\",\"BtoA\"]},\"amount_in\":{\"type\":\"integer\"}},\"required\":[\"dir\",\"amount_in\"]}}}],\"options\":{\"temperature\":0}}")
    DEC=$(echo "$RESP" | python3 -c 'import sys,json
d=json.load(sys.stdin);tc=d.get("message",{}).get("tool_calls") or []
print(json.dumps(tc[0]["function"]["arguments"]) if tc else "FAIL")' 2>/dev/null)
  else
    RESP=$(curl -s --max-time 400 $B/api/chat -d "{\"model\":\"$MODEL\",\"stream\":false,\"messages\":[{\"role\":\"system\",\"content\":\"$SYS End with ONE json line {\\\"dir\\\":\\\"AtoB\\\"|\\\"BtoA\\\",\\\"amount_in\\\":int}.\"},{\"role\":\"user\",\"content\":\"Decide.\"}],\"options\":{\"temperature\":0.2,\"num_predict\":$NP}}")
    C=$(echo "$RESP" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("message",{}).get("content",""))')
    DEC=$(python3 - "$C" <<'PY'
import sys,re,json
c=re.sub(r'<think>.*?</think>','',sys.argv[1],flags=re.S)
m=re.findall(r'\{[^{}]*"dir"[^{}]*\}',c,flags=re.S)
if m:
  try: print(json.dumps(json.loads(m[-1])));sys.exit(0)
  except: pass
# lenient fallback: pull dir + first int from the prose
d=re.search(r'(AtoB|BtoA)',c); a=re.search(r'(\d{3,5})',c)
print(json.dumps({"dir":d.group(1),"amount_in":int(a.group(1))}) if d and a else "FAIL")
PY
)
  fi
  echo "decision: $DEC"
  [ "$DEC" = "FAIL" ] && { echo "  parse fail — skip"; continue; }
  DIR=$(echo "$DEC" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("dir","AtoB"))')
  AMT=$(echo "$DEC" | python3 -c 'import sys,json;a=int(json.load(sys.stdin).get("amount_in",500));print(max(100,min(2000,a)))')
  # Verified Execution Gate
  [ "$DIR" != "AtoB" ] && [ "$DIR" != "BtoA" ] && { echo "  GATE: bad dir"; continue; }
  HAVE=$([ "$DIR" = AtoB ] && echo $UB || echo $WB)
  [ "$HAVE" -lt "$AMT" ] && { echo "  GATE: low $DIR bal → flip AtoB 500"; DIR=AtoB; AMT=500; }
  # v0.36.1 auth gate: sign the swap as the trader (field order MUST match the
  # server's authorize() call: from pool dir amount_in min_out). req_nonce = ms ts (monotonic).
  NONCE=$(date +%s%3N)
  SIGHEX=$("$SIGN" "$TRADER_SEED" "$NONCE" swap "$TRADER" "$POOL" "$DIR" "$AMT" 1 | python3 -c 'import sys,json;print(json.load(sys.stdin)["sig"])')
  S=$(curl -s $RPC/swap -d "{\"from\":\"$TRADER\",\"pool\":\"$POOL\",\"dir\":\"$DIR\",\"amount_in\":$AMT,\"min_out\":1,\"req_nonce\":$NONCE,\"sig\":\"$SIGHEX\"}")
  echo "  on-chain: $S"
  if echo "$S" | grep -q '"ok":true'; then
    OK=$((OK+1)); VOL=$((VOL+AMT))
    LP=$((LP+$(echo "$S"|python3 -c 'import sys,json;print(json.load(sys.stdin).get("lp_fee",0))')))
    PF=$((PF+$(echo "$S"|python3 -c 'import sys,json;print(json.load(sys.stdin).get("protocol_fee",0))')))
  fi
done
echo "### RESULT ###"
echo "executed=$OK/$N  volume_in=$VOL  lp_fees=$LP  protocol_fees=$PF"
echo "final: USDS=$(bal $USDS) wQUG=$(bal $WQUG)"
echo "pool: $(curl -s $RPC/pools)"
