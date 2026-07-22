#!/usr/bin/env python3
"""Analyze delivery-probe sweep results: test D = 1 - p^r on live data."""
import json, math, sys, glob, os
from collections import defaultdict

outdir = sys.argv[1]
conds = ["c0", "c10", "c25", "c40", "ge30"]

def wilson(k, n, z=1.96):
    if n == 0: return (0.0, 0.0)
    p = k / n
    d = 1 + z*z/n
    c = (p + z*z/(2*n)) / d
    h = z * math.sqrt(p*(1-p)/n + z*z/(4*n*n)) / d
    return (c - h, c + h)

print(f"{'cond':6} {'r':>2} {'trials':>6} {'att':>6} {'p̂':>8} {'D̂':>8} {'1-p̂^r':>8} {'resid pp':>9} {'D̂ 95% CI':>17} {'law in CI':>9}")
rows = {}
for cond in conds:
    # pooled p̂ per condition from ALL attempts across r runs
    att_ok, att_n = 0, 0
    per_peer = defaultdict(lambda: [0, 0])  # peer -> [fail, total]
    data = {}
    for r in (1, 2, 3):
        f = os.path.join(outdir, f"{cond}-r{r}.jsonl")
        if not os.path.exists(f): continue
        trials = [json.loads(l) for l in open(f) if l.strip()]
        data[r] = trials
        for t in trials:
            for a in t["attempts"]:
                att_n += 1
                att_ok += a["ok"]
                per_peer[a["peer"]][0] += (not a["ok"])
                per_peer[a["peer"]][1] += 1
    if not data: continue
    p_hat = 1 - att_ok / max(att_n, 1)
    rows[cond] = (p_hat, per_peer, data)
    for r, trials in sorted(data.items()):
        n = len(trials)
        k = sum(t["delivered"] for t in trials)
        d_hat = k / n
        d_law = 1 - p_hat ** r
        lo, hi = wilson(k, n)
        ok = "yes" if lo <= d_law <= hi else "NO"
        print(f"{cond:6} {r:>2} {n:>6} {sum(len(t['attempts']) for t in trials):>6} "
              f"{p_hat:>8.4f} {d_hat:>8.4f} {d_law:>8.4f} {(d_hat-d_law)*100:>+9.2f} "
              f"[{lo:.4f},{hi:.4f}] {ok:>9}")

print("\nPer-peer failure rates (independence check — clustering would break the law):")
for cond in conds:
    if cond not in rows: continue
    p_hat, per_peer, data = rows[cond]
    rates = sorted((f/t, peer[-6:], f, t) for peer, (f, t) in per_peer.items())
    spread = " ".join(f"{r:.3f}" for r, *_ in rates)
    print(f"  {cond:5} pooled p̂={p_hat:.4f}  per-peer: {spread}")

print("\nRefined law (per-peer product over each trial's actual peer choices):")
for cond in conds:
    if cond not in rows: continue
    p_hat, per_peer, data = rows[cond]
    pp = {peer: f / t for peer, (f, t) in per_peer.items() if t > 0}
    for r, trials in sorted(data.items()):
        if r == 1: continue
        pred = sum(1 - math.prod(pp.get(a["peer"], p_hat) for a in t["attempts"])
                   for t in trials) / len(trials)
        d_hat = sum(t["delivered"] for t in trials) / len(trials)
        print(f"  {cond:5} r={r}  D̂={d_hat:.4f}  refined={pred:.4f}  resid={(d_hat-pred)*100:+.2f}pp")

# RMS residual across all (cond, r) cells vs pooled-p̂ law
res = []
for cond in conds:
    if cond not in rows: continue
    p_hat, _, data = rows[cond]
    for r, trials in data.items():
        d_hat = sum(t["delivered"] for t in trials) / len(trials)
        res.append((d_hat - (1 - p_hat ** r)) * 100)
rms = math.sqrt(sum(x*x for x in res) / len(res)) if res else 0
print(f"\nRMS residual across {len(res)} cells: {rms:.3f} pp   max |resid|: {max(abs(x) for x in res):.3f} pp")
