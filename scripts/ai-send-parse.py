#!/usr/bin/env python3
"""AI-Send brain (reference impl) — propose-only NL→send for the SIGIL wallet.

Two-mind, no key in the browser (runs server-side on Epsilon):
  PROPOSER = qwen3:0.6b  (local ollama :11434)  → parse NL into {to, amount, memo}
  VETOER   = DeepSeek-v4 (api.deepseek.com)     → approve / reject the parse

It NEVER sends. It returns a proposal the wallet pre-fills into SendModal;
the human presses Sign & Send. Address + amount are re-validated client-side.

Usage:  ai-send-parse.py "send 7.5 sgl to <addr> memo thanks"  [balance]
Exit 0 + JSON {to,amount,memo,verdict,reason} on stdout.
"""
import sys, json, re, urllib.request

OLLAMA = "http://localhost:11434/api/chat"
DEEPSEEK = "https://api.deepseek.com/chat/completions"
DS_KEY = "/root/.config/deepseek/api_key"
ADDR_RE = re.compile(r"^(sgl1[0-9a-z]{6,}|[0-9a-fA-F]{64})$")
# SAFETY: a 0.6B model must NEVER reproduce a 64-char address (one transposed
# char = funds to the wrong wallet). Extract it VERBATIM from the user's text.
ADDR_EXTRACT_RE = re.compile(r"\b(sgl1[0-9a-z]{6,}|[0-9a-fA-F]{64})\b")

def extract_address(nl):
    m = ADDR_EXTRACT_RE.search(nl or "")
    return m.group(1) if m else ""

def _post(url, body, headers, timeout=90):
    req = urllib.request.Request(url, json.dumps(body).encode(),
                                 {"Content-Type": "application/json", **headers})
    return json.load(urllib.request.urlopen(req, timeout=timeout))

def propose(nl):
    """qwen3:0.6b → strict {to, amount, memo}."""
    r = _post(OLLAMA, {
        "model": "qwen3:0.6b", "stream": False, "think": False, "format": "json",
        "messages": [
            {"role": "system", "content":
             "Extract a SIGIL wallet send into STRICT JSON keys to,amount,memo. "
             "'to' = recipient address (sgl1... or 64-hex) EXACTLY as written. "
             "amount = number in SGL. memo = short string or empty. Output ONLY the JSON."},
            {"role": "user", "content": nl},
        ]}, {})
    return json.loads(r["message"]["content"])

def veto(nl, parsed, balance):
    """DeepSeek-v4 adversarial gate. Returns (ok: bool, reason: str)."""
    key = open(DS_KEY).read().strip()
    # The recipient address FORMAT is already validated deterministically by ADDR_RE
    # (sgl1... OR 64-hex are BOTH valid) before this call — do NOT re-judge it here.
    q = (f"User said: {nl!r}\nParsed: {json.dumps(parsed)}\nBalance: {balance} SGL.\n"
         "The recipient address format is ALREADY verified valid — do NOT judge the address. "
         "Only check: (a) amount > 0 and <= balance, (b) the parsed amount/memo match the user's words. "
         "Reject ONLY on those. "
         'Answer STRICT JSON {"ok":bool,"reason":"<=12 words"}.')
    r = _post(DEEPSEEK, {
        "model": "deepseek-chat", "temperature": 0.0, "max_tokens": 120,
        "messages": [
            {"role": "system", "content": "Adversarial send-safety gate. Default ok=false if unsure. STRICT JSON only."},
            {"role": "user", "content": q}],
    }, {"Authorization": "Bearer " + key})
    v = json.loads(r["choices"][0]["message"]["content"])
    return bool(v.get("ok")), str(v.get("reason", ""))[:80]

def main():
    nl = sys.argv[1] if len(sys.argv) > 1 else ""
    balance = float(sys.argv[2]) if len(sys.argv) > 2 else 1e18
    out = {"to": "", "amount": 0, "memo": "", "verdict": "REJECT", "reason": ""}
    try:
        p = propose(nl)
        # Address comes from the USER'S LITERAL TEXT (regex), never the LLM — safety.
        # Fall back to the LLM's only if the user typed no address-shaped token.
        out["to"] = extract_address(nl) or str(p.get("to") or "")
        out["amount"] = p.get("amount") or 0
        out["memo"] = str(p.get("memo") or "")
        # client-side hard gate FIRST (cheap, deterministic) — never trust the model alone
        if not ADDR_RE.match(out["to"]):
            out["reason"] = "no valid recipient address parsed"
        else:
            ok, reason = veto(nl, p, balance)
            out["verdict"] = "OK" if ok else "REJECT"
            out["reason"] = reason or ("approved" if ok else "vetoed")
    except Exception as e:
        out["reason"] = f"error: {e}"
    print(json.dumps(out))
    return 0

if __name__ == "__main__":
    sys.exit(main())
