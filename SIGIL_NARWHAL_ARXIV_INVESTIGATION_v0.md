# SIGIL Narwhal Mempool — arXiv Investigation v0

The full literature-check writeup lives as a compiled paper, not a second copy of this
markdown: **`SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`**, built via
`flux-arxiv-latex/src/bin/sigil_narwhal_investigation.rs` in the Flux workspace, published:

```
wget https://quillon.xyz/downloads/SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf
```

(mirror: `https://sigilgraph.fluxapp.xyz/downloads/SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`)

## One-paragraph summary

`SIGIL_NARWHAL_MEMPOOL_v0.md` §3.3 originally claimed erasure-coded batch dissemination
as an "invented upgrade" beyond stock Narwhal. That claim was checked against the real
literature (Viktor's request, 2026-08-15) and found wrong: **Imitater**
(arXiv:2409.19286, Sep 2024) already erasure-codes mempool microblocks with `(f+1,n)`
Reed-Solomon codes and forms `2f+1`-signature availability certificates — the same shape,
published about a year earlier. Erasure-coded propagation is separately proven at the
block-fanout layer (Solana Turbine, Monad RaptorCast) and the blob-availability layer
(Ethereum Data Availability Sampling / danksharding). Aptos's own team explicitly
evaluated and rejected erasure coding for their Narwhal-derived Quorum Store, reasoning
it adds complexity with no load-balancing benefit over their already-symmetric
full-broadcast — a real counter-argument this paper engages with rather than routes
around. §3.3 of the design doc has been corrected in place; the compiled paper is the
fuller writeup, with full citations, that correction points to.

See `SIGIL_NARWHAL_MEMPOOL_v0.md` for the design this corrects, and its own §3.3 and §6
for the in-place correction.
