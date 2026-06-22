#!/usr/bin/env python3
# BLAKE4 differential-trail LOWER BOUND via z3 (SMT, bit-vector theory).
#
# The exact analogue of the verified Rust `diff_search` engine, but SOLVED instead of
# greedily walked: each modular addition gets a FREE output-difference variable constrained
# by the Lipmaa-Moriai XOR-differential validity condition; the per-add weight is the popcount
# of the non-equal bits (−log2 prob); z3 minimizes the TOTAL weight over R rounds of a nonzero
# message difference (chaining value identical). The minimum it returns is the lower bound:
# no differential trail is cheaper. If min > 64, no trail helps a miner beat the 64-bit PoW
# window at that round count.
#
# Round structure = byte-identical to the verified Rust round (G indices, rotations 16/12/8/7,
# message permutation) — that wiring was already MC-validated against the real BLAKE4 round.
#
# Usage: blake4_diff_z3.py <rounds> [max_weight]
#   no max_weight -> minimize the trail weight (exact lower bound), print z3 lower/upper bounds.
#   max_weight=W  -> ask "does a trail of weight <= W exist?"  UNSAT proves min > W (secure).

import sys, time
from z3 import (BitVec, BitVecVal, Extract, RotateRight, Or, If, Sum, Optimize, Solver,
                sat, unsat, set_param)

import os
ROUNDS = int(sys.argv[1])
MAXW = int(sys.argv[2]) if len(sys.argv) > 2 else None
Z3_TIMEOUT_MS = int(os.environ.get("Z3_TIMEOUT_MS", "1200000"))   # default 20 min
W = 32
MSG_PERMUTATION = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8]
COLS = [(0, 4, 8, 12), (1, 5, 9, 13), (2, 6, 10, 14), (3, 7, 11, 15)]
DIAGS = [(0, 5, 10, 15), (1, 6, 11, 12), (2, 7, 8, 13), (3, 4, 9, 14)]

cons = []          # constraints
wterms = []        # per-add weight (Int) terms
_fresh = [0]


def fresh():
    _fresh[0] += 1
    return BitVec(f"g{_fresh[0]}", W)


def eq3(x, y, z):                       # bits where x, y, z all agree
    return (~(x ^ y)) & (~(x ^ z))


def valid(a, b, g):                     # Lipmaa-Moriai: differential (a,b->g) is possible
    sa, sb, sg = a << 1, b << 1, g << 1
    viol = eq3(sa, sb, sg) & (a ^ b ^ g ^ sb)
    return viol == BitVecVal(0, W)


def weight(a, b, g):                    # -log2 prob = #non-eq bits in positions 0..30 (MSB free)
    noneq = (~eq3(a, b, g))
    return Sum([If(Extract(i, i, noneq) == BitVecVal(1, 1), 1, 0) for i in range(W - 1)])


def add(a, b):                          # one modular add on differences -> fresh output diff
    g = fresh()
    cons.append(valid(a, b, g))
    wterms.append(weight(a, b, g))
    return g


def g_diff(sd, a, b, c, d, dmx, dmy):
    t1 = add(sd[a], sd[b]); a1 = add(t1, dmx)
    d1 = RotateRight(sd[d] ^ a1, 16)
    c1 = add(sd[c], d1)
    b1 = RotateRight(sd[b] ^ c1, 12)
    t2 = add(a1, b1); a2 = add(t2, dmy)
    d2 = RotateRight(d1 ^ a2, 8)
    c2 = add(c1, d2)
    b2 = RotateRight(b1 ^ c2, 7)
    sd[a], sd[b], sd[c], sd[d] = a2, b2, c2, d2


# state difference starts at 0 (identical chaining value); difference lives in the message.
sd = [BitVecVal(0, W) for _ in range(16)]
dm = [BitVec(f"dm{i}", W) for i in range(16)]
# 40-byte PoW input = message words 0..9; words 10..15 are zero padding -> no difference.
for i in range(10, 16):
    cons.append(dm[i] == 0)
cons.append(Or([dm[i] != BitVecVal(0, W) for i in range(10)]))   # nonzero input difference

m = list(dm)
for _ in range(ROUNDS):
    gs = COLS + DIAGS
    for i, (a, b, c, d) in enumerate(gs):
        g_diff(sd, a, b, c, d, m[2 * i], m[2 * i + 1])
    m = [m[MSG_PERMUTATION[i]] for i in range(16)]

total = Sum(wterms)
nadds = len(wterms)
print(f"[z3] BLAKE4 R={ROUNDS}: {nadds} modular adds, {_fresh[0]} diff vars, "
      f"{'minimize' if MAXW is None else f'threshold<= {MAXW}'}", flush=True)

t0 = time.time()
if MAXW is None:
    opt = Optimize()
    opt.set("timeout", Z3_TIMEOUT_MS)
    for c in cons:
        opt.add(c)
    h = opt.minimize(total)
    res = opt.check()
    dt = time.time() - t0
    print(f"[z3] check={res} in {dt:.1f}s  lower_bound={opt.lower(h)}  upper_bound={opt.upper(h)}",
          flush=True)
    if res == sat:
        print(f"[z3] EXACT MINIMUM TRAIL WEIGHT (R={ROUNDS}) = {opt.upper(h)}  "
              f"(vs 64-bit PoW window)", flush=True)
else:
    s = Solver()
    s.set("timeout", Z3_TIMEOUT_MS)
    for c in cons:
        s.add(c)
    s.add(total <= MAXW)
    res = s.check()
    dt = time.time() - t0
    print(f"[z3] trail weight <= {MAXW} ?  {res}  in {dt:.1f}s", flush=True)
    if res == unsat:
        print(f"[z3] PROVEN: no R={ROUNDS} trail of weight <= {MAXW} exists  "
              f"=> minimum > {MAXW}", flush=True)
    elif res == sat:
        print(f"[z3] a trail of weight <= {MAXW} EXISTS (R={ROUNDS}) — attack/witness found",
              flush=True)
