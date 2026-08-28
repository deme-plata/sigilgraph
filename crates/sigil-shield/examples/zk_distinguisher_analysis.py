#!/usr/bin/env python3
"""Analysis pass for `zk_distinguisher` — the recovery game.

Reads the CSV that example writes and asks one question three ways: can an observer
name the hidden output amount from the proof bytes alone?

v4 is the POSITIVE CONTROL. It prints the amount ~85 times per proof, so every detector
must find it. A harness that cannot detect a known leak proves nothing about v5.

Critical values are MEASURED by permutation, not assumed. A first pass used a
hand-estimated Bonferroni threshold of 3.0 and flagged v5 as leaking; the empirical null
95th percentile is 3.66, so that was the threshold being wrong, not the proof.

Usage:  python3 zk_distinguisher_analysis.py [csv_path]
"""
import csv, random, math, sys
from collections import defaultdict

random.seed(11)
PATH = sys.argv[1] if len(sys.argv) > 1 else "/home/storage/zk-dist.csv"
AM = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
K = len(AM)

rows, cols, labels = [], {}, {}
for v in ("v4", "v5"):
    cols[v] = [[] for _ in range(256)]
    labels[v] = []
with open(PATH) as f:
    for d in csv.DictReader(f):
        v = d["version"]
        lab = int(d["label"])
        h = [float(d[f"h{i}"]) for i in range(256)]
        rows.append((v, lab, h, [int(d[f"hit{a}"]) for a in AM]))
        labels[v].append(lab)
        for i in range(256):
            cols[v][i].append(h[i])

n_per = len(labels["v5"])
chance = 100.0 / K
se = 100.0 * math.sqrt((1 / K) * (1 - 1 / K) / n_per)
print(f"{len(rows)} proofs | {n_per} per version | {K} classes")
print(f"chance {chance:.1f}%  1 s.e. {se:.2f}%  detection floor (chance+3se) {chance + 3*se:.1f}%\n")

print("=== D1: verbatim-pattern attack (v4's actual failure mode) ===")
for ver in ("v4", "v5"):
    sub = [r for r in rows if r[0] == ver]
    ok = tot = 0
    for _, lab, _, hits in sub:
        m = max(hits)
        if random.choice([i for i, h in enumerate(hits) if h == m]) == lab:
            ok += 1
        tot += hits[lab]
    print(f"  {ver}: {100.0*ok/len(sub):6.2f}%   mean occurrences of TRUE amount {tot/len(sub):.3f}")


def cv(sub, folds=5):
    idx = list(range(len(sub)))
    random.shuffle(idx)
    ok = 0
    for f in range(folds):
        test = {i for j, i in enumerate(idx) if j % folds == f}
        c, cnt = defaultdict(lambda: [0.0] * 256), defaultdict(int)
        for i, (_, lab, h, _) in enumerate(sub):
            if i in test:
                continue
            ch = c[lab]
            for k in range(256):
                ch[k] += h[k]
            cnt[lab] += 1
        for lab in c:
            for k in range(256):
                c[lab][k] /= cnt[lab]
        for i in test:
            _, lab, h, _ = sub[i]
            best = bd = None
            for lb, cvv in c.items():
                d = sum((h[k] - cvv[k]) ** 2 for k in range(256))
                if bd is None or d < bd:
                    bd, best = d, lb
            ok += best == lab
    return 100.0 * ok / len(sub)


print("\n=== D2: nearest-centroid on byte histogram, 5-fold CV ===")
for ver in ("v4", "v5"):
    print(f"  {ver}: {cv([r for r in rows if r[0]==ver]):6.2f}%")
print("\n=== NEGATIVE CONTROL (labels shuffled — must sit at chance) ===")
for ver in ("v4", "v5"):
    sub = [r for r in rows if r[0] == ver]
    labs = [r[1] for r in sub]
    random.shuffle(labs)
    print(f"  {ver}: {cv([(s[0], labs[i], s[2], s[3]) for i, s in enumerate(sub)]):6.2f}%")


def max_f(colset, labs):
    n = len(labs)
    best, bk = 0.0, None
    for k, col in enumerate(colset):
        s, c, tot = [0.0] * K, [0] * K, 0.0
        for x, l in zip(col, labs):
            s[l] += x
            c[l] += 1
            tot += x
        gm = tot / n
        b = sum(c[i] * (s[i] / c[i] - gm) ** 2 for i in range(K) if c[i]) / (K - 1)
        w = sum((x - s[l] / c[l]) ** 2 for x, l in zip(col, labs)) / (n - K)
        F = b / w if w > 1e-12 else 0.0
        if F > best:
            best, bk = F, k
    return best, bk


print("\n=== D3: strongest histogram bin, max-F with a PERMUTATION null ===")
NPERM = 200
for ver in ("v4", "v5"):
    obs, bk = max_f(cols[ver], labels[ver])
    labs = labels[ver][:]
    null = []
    for _ in range(NPERM):
        random.shuffle(labs)
        null.append(max_f(cols[ver], labs)[0])
    null.sort()
    p = (sum(1 for x in null if x >= obs) + 1) / (NPERM + 1)
    print(f"  {ver}: observed max-F {obs:8.2f} (bin {bk}) | null 95th pct {null[int(0.95*NPERM)]:6.2f}"
          f" | p = {p:.4f} -> {'SIGNAL' if p < 0.05 else 'no evidence of signal'}")
