# ⬡ SigilGraph — Release Ledger

Commercial-grade releases. Every release is a **GPG-signed (Verified)** commit, and the released
code carries a **content-addressed flux-rev provenance hash** — re-snapshot the crate with
`flux-rev snapshot <crate>` and verify the hash matches. *Probatione, non fide.*

| Version | Date | Type | git (signed) | flux-rev provenance |
|---|---|---|---|---|
| **v0.0.10** | 2026-06-05 | 🐛 fix | `ec873b7` | — |
| **v0.0.9** | 2026-06-05 | ✨ feature | `d03672d` | sigil-state `1f5c55a01cfe3aa9…` |
| **v0.0.8** | 2026-06-05 | 🚀 initial public release (67 crates) | `b219791` | — |

## v0.0.10 — fix
- **fix(acc-scale-bench):** faithful O(1) update measurement — the loop passed a stale `old_value`, drifting the accumulator; it now flips one key between two values (each old = previous new), so the sum stays consistent and the timing is a true single-key O(1) update.

## v0.0.9 — feature
- **feat(sigil-state):** O(1) state-root scaling benchmark (`acc-scale-bench`) — scales the multiset accumulator 1M→100M accounts and shows `root()`/`update()` stay **flat (~200 ns)** while from-scratch grows O(N). Measured proof that a tiny light node verifies a multi-TB chain at constant cost.

## v0.0.8 — initial public release
- An experimental DagKnight chain on Flux: provenance-signed blocks (BLAKE3 × SQIsign), a 572 KB light client that verifies the whole chain in ~10 µs, 21M capped by construction.
