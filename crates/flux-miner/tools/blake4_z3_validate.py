#!/usr/bin/env python3
# Validate the z3 xdp+ encoding before trusting any trail result.
#  (1) a Python closed-form xdp+ == brute force, exhaustively at n=6.
#  (2) the z3 BV encoding (valid/weight, width 32) == that closed-form, over random 32-bit triples.
# If both pass, the per-add SMT encoding is sound at the real width.

import random
from z3 import BitVecVal, Extract, simplify, is_true

W = 32


def brute_xdp(a, b, g, n):
    mask = (1 << n) - 1
    cnt = 0
    for x in range(1 << n):
        for y in range(1 << n):
            if (((x + y) ^ ((x ^ a) + (y ^ b))) & mask) == (g & mask):
                cnt += 1
    if cnt == 0:
        return None
    assert cnt & (cnt - 1) == 0, f"count {cnt} not power of two"
    return (2 * n) - (cnt.bit_length() - 1)            # w = 2n - log2(cnt)


def closed_xdp(a, b, g, n):
    mask = (1 << n) - 1
    a, b, g = a & mask, b & mask, g & mask
    eq3 = lambda x, y, z: (~(x ^ y)) & (~(x ^ z)) & mask
    shl = lambda v: (v << 1) & mask
    viol = eq3(shl(a), shl(b), shl(g)) & ((a ^ b ^ g ^ shl(b)) & mask)
    if viol != 0:
        return None
    low = mask >> 1                                    # bits 0..n-2
    return bin((~eq3(a, b, g)) & low).count("1")


# (1) closed-form == brute, exhaustive at n=6
n = 6
for a in range(1 << n):
    for b in range(1 << n):
        for g in range(1 << n):
            assert closed_xdp(a, b, g, n) == brute_xdp(a, b, g, n), (a, b, g)
print(f"[validate] (1) Python closed-form xdp+ == brute force, all (a,b,g) at n={n}  OK")


# (2) z3 encoding (width 32) == closed-form (width 32), random triples
def z3_valid(a, b, g):
    A, B, G = BitVecVal(a, W), BitVecVal(b, W), BitVecVal(g, W)
    sa, sb, sg = A << 1, B << 1, G << 1
    eq3 = (~(sa ^ sb)) & (~(sa ^ sg))
    viol = eq3 & (A ^ B ^ G ^ sb)
    return is_true(simplify(viol == BitVecVal(0, W)))


def z3_weight(a, b, g):
    A, B, G = BitVecVal(a, W), BitVecVal(b, W), BitVecVal(g, W)
    noneq = (~((~(A ^ B)) & (~(A ^ G))))
    s = 0
    for i in range(W - 1):
        s += 1 if is_true(simplify(Extract(i, i, noneq) == BitVecVal(1, 1))) else 0
    return s


random.seed(12345)
for _ in range(4000):
    a, b, g = (random.getrandbits(32) for _ in range(3))
    ref = closed_xdp(a, b, g, 32)
    v = z3_valid(a, b, g)
    if ref is None:
        assert not v, f"z3 says valid but closed-form impossible: {a:#x} {b:#x} {g:#x}"
    else:
        assert v, f"z3 says invalid but closed-form valid: {a:#x} {b:#x} {g:#x}"
        assert z3_weight(a, b, g) == ref, f"weight mismatch {a:#x} {b:#x} {g:#x}: {z3_weight(a,b,g)} vs {ref}"
print("[validate] (2) z3 encoding valid/weight == closed-form, 4000 random 32-bit triples  OK")
print("[validate] PASS — the z3 xdp+ encoding is sound at the real width.")
