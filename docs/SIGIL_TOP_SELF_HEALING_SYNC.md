# SIGIL-TOP — The Self-Healing Sync Triad

> Iteration #1 of the "built-up-power-release" series.
> Forged 2026-06-17, in the heat of a live Quillon-node debug. Every law below
> is the *scar tissue* of a real wedge we watched a production chain suffer —
> turned into a property sigil-top is **born immune to**.

The camera is rolling. Three bugs nearly ate a fresh node alive today. Watch us
turn each one into an invention.

---

## AHA #1 — Cap-Negotiated Chunks (death of the INCOMPLETE-discard)

**The scar:** the Quillon node requested 500-block packs; the serving peer capped
responses at 50. Every chunk returned `INCOMPLETE (got 50, expected N)` and was
*discarded* — contiguous height never left 0. A silent, infinite stall.

**The invention:** sigil-top must never *guess* a peer's pack ceiling. The
handshake already exchanges a `HandshakeMessage`; we add one `u32`:

```rust
// flux-sigil-net handshake extension
pub struct SigilHandshake {
    pub network_id:   [u8; 8],     // b"sigil-g0"
    pub genesis_root: [u8; 32],
    pub proto:        ProtocolVersion,
    pub max_pack:     u32,         // <-- NEW: this peer's block-pack ceiling
}
```

block_sync then clamps every request to `min(our_window, peer.max_pack)`. A short
response is no longer "incomplete" — it's *exactly the contract*. Discards become
mathematically impossible. The 10K genesis-window stays; it just lands in
peer-honest slices.

## AHA #2 — Genesis-Anchor-or-Refuse (death of the self-minted fork)

**The scar:** a fresh node auto-minted its *own* genesis, which didn't byte-match
the canonical chain. Every handshake to a real peer died on `GenesisMismatch` →
`BrokenPipe`. The node sat at height 0 forever, proudly producing a chain of one,
talking to no one.

**The invention:** sigil-top already commits `BLAKE3(SIGIL_GENESIS_v0.md)` into
block 0. We make it a *gate*, not a hope:

```rust
// sigil-node boot, before binding ports
match adopt_or_verify_genesis(&peers).await {
    GenesisDecision::Adopted(root) => store.anchor(root),   // fresh node: take canonical
    GenesisDecision::Matches      => {}                     // restart: already correct
    GenesisDecision::Conflict     => std::process::exit(78) // Q_PREFLIGHT_FAIL — refuse to fork
}
```

A node with no chain **adopts** the network's genesis root from a tip-proof before
it is *allowed* to produce a single block. A node that disagrees with the network
**refuses to run**. There is no third state where it quietly forks itself into a
lonely island. The dead-end is deleted by construction.

## AHA #3 — Relentless Warmup, tuned by the delivery law

**The scar:** the node's warmup dialed for 21 seconds, connected to 0 peers,
logged *"network unreachable from this host,"* and **gave up** — then idled on
HTTP height-discovery forever, knowing the tip but never fetching a block.

**The invention:** warmup must obey the law chronos already proved this session.
Unique-contact probability after `r` independent dials at drop `p` is **1 − pʳ**.
We measured it dead-on: p=0.3, r=3 → 97.3%. So warmup never "surrenders" — it
*spends redundancy until the math says it's connected*:

```rust
// relentless warmup: re-dial until 1 - p^r crosses the confidence floor
let mut r = 0;
while connected_peers() == 0 {
    redial_all(&bootstrap);            // each peer, fresh circuit
    r += 1;
    let p_fail = observed_drop_prob(); // EMA of recent dial outcomes
    if 1.0 - p_fail.powi(r) > 0.999 && connected_peers() == 0 {
        // 99.9% of the time we'd be connected by now → the peers are truly down,
        // not flaky. Back off to slow-probe instead of hot-spinning OR lying "unreachable".
        slow_probe_with_dht_reseed();
    }
    sleep(backoff(r));
}
```

The node stops conflating *"my dials are flaky"* (keep trying — the law says you'll
win) with *"the peers are genuinely dead"* (reseed the DHT, slow-probe, but never
declare false defeat). Empty-DHT cold-start — the classic new-node trap — heals
itself.

---

## Why this is a *power release*, not a patch

Each fix is a **property**, enforced at a chokepoint, not a band-aid on a symptom:

| Scar (Quillon, live) | sigil-top property | Enforced at |
|---|---|---|
| INCOMPLETE chunks discarded → stall@0 | chunks clamp to negotiated cap | handshake + block_sync |
| self-mint → GenesisMismatch → BrokenPipe | adopt-or-refuse genesis | sigil-node preflight (exit 78) |
| warmup surrenders → idle-forever | 1−pʳ relentless warmup | flux-sigil-net warmup |

A chain that *cannot* silently stall, *cannot* fork itself alone, and *cannot*
falsely declare the network dead. We didn't read these in a paper — we earned them
at 06:00 watching a real node refuse to climb off zero.

**Next iterations queued:** wire AHA#1 into `crates/sigil-top/src/block_sync.rs`
(needs Delta online to `fluxc build`), add the `max_pack` field to the
`flux-sigil-net` handshake, and a chronos CI gate asserting contiguity==100% under
drop≤0.4 with cap-negotiation on.

*Roll credits on the stall. The node climbs now.*

---

# Iteration #2 — The code talks back (and corrects us)

> Forged minutes later, by *reading sigil-top's real `block_sync.rs`* instead of
> theorizing. The best aha is the one the code hands you when you finally look.

**Plot twist — AHA#1 was already won, by a better mechanism.** sigil-top does not
request fixed-count packs. It sends **open-ended `[frontier, u64::MAX]`** ranges;
the *server* clamps `hi = req.to.min(top)`, and the **store is the reorder buffer**
(`block_sync.rs:304` — chunks from different peers land in any order). There is no
"expected N, got M → discard." The Quillon INCOMPLETE-stall **cannot occur here**.
Cap-negotiation (`max_pack`) is therefore *unnecessary* — sigil-top picked a
stronger design. We strike AHA#1 and bank the lesson: read before you invent.

**But the same monster wears a different mask — and it already bit once.**
`zstd_decompress_body` (block_sync.rs:118) caps decode output to guard against a
zstd-bomb. History (in the code's own words):

> v0.39 cut the cap to 12 MiB on the assumption "a real chunk is ≤ ~8 MB". That
> assumption was WRONG for SIGIL — every header carries a StarkProof + Wesolowski
> VDF + two SQIsign sigs + a fluxc ProofBundle (~8 KB mature). At the first height
> with real STARK proofs, every chunk exceeded 12 MiB → `None` → **got=0 → the peer
> was wrongly benched → the frontier never advanced.** A silent full-archive stall.

v0.95 *restored* a 64 MiB cap (4096 items × 16 KiB headroom) — which **fixes today
but is still a static guess**, coupled to an assumption about proof size that has
*already been wrong once*. When post-quantum proofs grow, 64 MiB becomes the next
12 MiB.

## AHA #2.1 — A bomb-guard must never be mistaken for a bad peer

The root flaw isn't the number — it's that **`Option<Vec>` conflates three
outcomes**: good chunk, malicious bomb, and *"honest chunk merely larger than my
cap."* The third gets punished like the second. Invention:

```rust
enum Decoded { Ok(Vec<u8>), Bomb, TooBig { seen: u64 } } // TooBig != malicious

fn zstd_decompress_body(body: &[u8], cap: u64) -> Decoded { /* … */ }
```

And the caller stops benching innocents — it **adapts the window** instead:

```rust
match decode(chunk, cap) {
    Decoded::Ok(raw)        => store.put_blocks_batch(parse(raw)),   // advance frontier
    Decoded::Bomb           => bench_peer(p),                        // genuinely hostile
    Decoded::TooBig { .. }  => { window /= 2; retry_same_peer(p); }  // shrink, NEVER bench
}
```

**The property:** the frontier can *never* stall on a cap mismatch, for *any* proof
size, forever — because an over-cap chunk shrinks the next request instead of
killing the (innocent) peer. The static cap stops being a latent landmine and
becomes just a bomb-ceiling. We don't pick a bigger guess; we **delete the class of
bug** the guess belongs to.

| What v0.95 did | What 2.1 does |
|---|---|
| Restore a bigger static cap (64 MiB) | Make cap-overflow a *retry-smaller* signal, not a bench |
| Survives until proofs grow again | Survives any proof size — window self-tunes |
| `None` punishes honest big chunks | `TooBig` distinguished from `Bomb` |

**Next (iteration #3 candidate):** thread `Decoded::TooBig` through the
frontier scheduler's bench/advance decision in `block_sync.rs`, add a regression
test that feeds an over-cap chunk and asserts *frontier still advances + peer not
benched*, and gate it in chronos CI. (Needs Delta online for `fluxc build`.)

*The code corrected the director. We kept rolling — and found the deeper shot.*

