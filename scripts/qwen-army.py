#!/usr/bin/env python3
"""
qwen-army — an LLM agent army that runs SIGIL node-control + mining and AUTO-HEALS.

Each "soldier" owns one node. Per tick it hands the node's live state to a Qwen
model over ollama/vLLM `/api/chat` with TOOL-CALLING (proper function-calling, not
prompt-scraping). The model decides which tool to fire; we execute it; we feed the
result back; repeat until the model declares the node STABLE+MINING or we hit a step
cap. If a node is DOWN, the model picks `node_resuscitate` → the node self-heals.

Brain  : Qwen via ollama /api/chat (tools=...). qwen2.5:72b / qwen3:32b both support tools.
Hands  : ssh/curl/systemctl ops mapped 1:1 onto the flux/sigil node-control surface
         (flux_node_resuscitate, flux_sigil_node_restart, sigil mining).
Reports : appends each action to a JSONL log + (optional) fires a webhook "for science".

Usage:
  QWEN_ENDPOINT=http://HOST:PORT QWEN_MODEL=qwen2.5:72b \
  python3 qwen-army.py --nodes delta=5.79.79.158 --webhook http://... [--dry-run] [--once]
"""
import argparse, json, os, subprocess, sys, time, urllib.request

ENDPOINT = os.environ.get("QWEN_ENDPOINT", "http://4.155.208.186:16162")
MODEL    = os.environ.get("QWEN_MODEL", "qwen2.5:72b")
SSH = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10"]
LOG = os.environ.get("QWEN_ARMY_LOG", "/tmp/qwen-army.jsonl")

# ── hands: real node ops (the tool implementations) ────────────────────────────
def sh(cmd, timeout=30):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout.strip()
    except Exception as e:
        return f"ERR:{e}"

def t_node_status(host, **_):
    """sigil-node alive? p2p :9501 listening? chain height?"""
    active = sh(SSH + [f"root@{host}", "systemctl is-active sigil-node 2>/dev/null || echo unknown"])
    p9501 = sh(SSH + [f"root@{host}", "ss -ltn 2>/dev/null | grep -c ':9501' || echo 0"])
    height = sh(SSH + [f"root@{host}", "curl -s --max-time 4 http://127.0.0.1:8181/api/v1/status 2>/dev/null | head -c 200 || echo none"])
    return {"service": active, "p2p_9501_listeners": p9501, "status_probe": height[:160]}

def t_node_resuscitate(host, dry_run=False, **_):
    """AUTO-FIX: restart sigil-node via systemd (the self-heal action)."""
    if dry_run:
        return {"action": "would restart sigil-node", "executed": False}
    out = sh(SSH + [f"root@{host}", "systemctl restart sigil-node 2>&1; sleep 2; systemctl is-active sigil-node"])
    return {"action": "restarted sigil-node", "executed": True, "now": out}

def t_mining_status(host, **_):
    running = sh(SSH + [f"root@{host}", "pgrep -c -f 'sigil.*mine|flux-miner' 2>/dev/null || echo 0"])
    return {"miner_procs": running}

def t_start_mining(host, dry_run=False, **_):
    if dry_run:
        return {"action": "would start miner", "executed": False}
    out = sh(SSH + [f"root@{host}", "systemctl start sigil-miner 2>&1 || echo 'no sigil-miner unit'; pgrep -c -f 'sigil.*mine|flux-miner' || echo 0"])
    return {"action": "start_mining", "executed": True, "result": out[:160]}

TOOLS_IMPL = {
    "node_status": t_node_status, "node_resuscitate": t_node_resuscitate,
    "mining_status": t_mining_status, "start_mining": t_start_mining,
}
TOOLS_SPEC = [
    {"type": "function", "function": {"name": n, "description": d,
      "parameters": {"type": "object", "properties": {"host": {"type": "string"}}, "required": ["host"]}}}
    for n, d in [
        ("node_status", "Inspect a SIGIL node: service state, p2p :9501 listeners, chain status probe."),
        ("node_resuscitate", "AUTO-FIX a down/stalled SIGIL node by restarting sigil-node (self-heal)."),
        ("mining_status", "Count running SIGIL miner processes on the node."),
        ("start_mining", "Start SIGIL mining on the node if it isn't already mining."),
    ]
]

def report(rec, webhook):
    rec["ts"] = time.time()
    with open(LOG, "a") as f: f.write(json.dumps(rec) + "\n")
    if webhook:
        try:
            urllib.request.urlopen(urllib.request.Request(webhook,
                data=json.dumps(rec).encode(), headers={"Content-Type": "application/json"}), timeout=6)
        except Exception:
            pass

def chat(messages):
    body = json.dumps({"model": MODEL, "messages": messages, "tools": TOOLS_SPEC, "stream": False}).encode()
    req = urllib.request.Request(ENDPOINT + "/api/chat", data=body, headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=300))["message"]

# ── one soldier: drive a node to healthy+mining via tool-calling ReAct ─────────
def tend_node(name, host, dry_run, webhook, max_steps=6):
    sysmsg = ("You are a SIGIL node-ops agent. GOAL: keep node '%s' (host %s) HEALTHY and MINING. "
              "Inspect with tools first. If sigil-node is not 'active' or :9501 has 0 listeners, call "
              "node_resuscitate to self-heal. If miner_procs is 0, call start_mining. When the node is "
              "active, :9501 listening, and mining, reply with a one-line STABLE summary and stop." % (name, host))
    messages = [{"role": "system", "content": sysmsg},
                {"role": "user", "content": f"Tend node {name} (host={host}). dry_run={dry_run}."}]
    for step in range(max_steps):
        msg = chat(messages)
        messages.append(msg)
        calls = msg.get("tool_calls") or []
        if not calls:
            report({"node": name, "verdict": msg.get("content", "")[:300]}, webhook)
            return msg.get("content", "")
        for c in calls:
            fn = c["function"]["name"]; args = c["function"].get("arguments", {}) or {}
            if isinstance(args, str):
                try: args = json.loads(args)
                except Exception: args = {}
            args.setdefault("host", host); args["dry_run"] = dry_run
            impl = TOOLS_IMPL.get(fn)
            result = impl(**args) if impl else {"error": f"unknown tool {fn}"}
            report({"node": name, "step": step, "tool": fn, "args": {k: v for k, v in args.items() if k != "dry_run"}, "result": result}, webhook)
            messages.append({"role": "tool", "content": json.dumps(result)})
    return "step cap reached"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nodes", required=True, help="comma list name=host, e.g. delta=5.79.79.158")
    ap.add_argument("--webhook", default=os.environ.get("QWEN_ARMY_WEBHOOK"))
    ap.add_argument("--interval", type=int, default=120)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--once", action="store_true")
    a = ap.parse_args()
    nodes = [tuple(n.split("=", 1)) for n in a.nodes.split(",") if "=" in n]
    print(f"qwen-army · brain={MODEL}@{ENDPOINT} · {len(nodes)} soldier(s) · dry_run={a.dry_run}")
    while True:
        for name, host in nodes:
            try:
                v = tend_node(name, host, a.dry_run, a.webhook)
                print(f"[{name}] {v[:200]}")
            except Exception as e:
                print(f"[{name}] AGENT ERROR: {e}")
        if a.once: break
        time.sleep(a.interval)

if __name__ == "__main__":
    main()
