# SIGIL — Selective Tor Egress: "Tor the submission, WireGuard the propagation"

> Be clever about Tor. It's slow. Use it only where it actually buys anonymity
> the rest of the stack can't — and only for tiny payloads. Everything else
> stays on the fast WireGuard mesh.

## The mistake we're NOT making

The naive "activate Tor" routes *all* libp2p gossip through Tor. That's wrong:
- Tor is high-latency, low-bandwidth. The block firehose (tens–hundreds of
  blocks/sec, MB/s) through Tor would collapse throughput — defeating the
  whole 10k-blocks/sec Stargate goal.
- WireGuard already gives the validator mesh **confidentiality** (encrypted)
  and the peers are already mutually trusted. Tor adds nothing there.

So Tor on the hot path is pure cost, no benefit.

## Where Tor actually earns its latency

Reserve Tor for payloads that are simultaneously **(a) tiny**, **(b) identity-
revealing at the network layer**, and **(c) off-mesh**. The taxonomy
(`sigil_net::EgressClass`):

| Class | Example | Transport | Why |
|---|---|---|---|
| `HotMesh` | block gossip, peer-heights, votes | **WireGuard** | high-bandwidth, trusted peers, already confidential |
| `PrivateSubmit` | a shielded-tx submission (sigil-mixer) | **Tor** | the ONE IP↔tx link; tiny |
| `LightQuery` | light-client tip-proof fetch | **Tor** | don't reveal "I'm a SIGIL user"; tiny |
| `OffMeshFetch` | oracle price, release metadata | **Tor** | source-anonymity to a non-validator |

## The canonical case — the privacy layers finally compose

SIGIL is private-by-default: **sigil-mixer hides the amount + parties on-chain.**
But the *network* layer leaks: when wallet X submits its shielded tx, the
receiving validator sees **X's IP**, relinking the "anonymous" tx to a real
machine. An adversary running validators deanonymizes at the network layer even
though the ledger is private.

Fix: route **only that tiny submission** over Tor.
- Chain hides the **contents** (mixer). Tor hides the **submitter** (no source IP).
- The two compose into genuine anonymity — the thing a "private chain" actually
  promised.
- Everything after is fine on WireGuard: the tx is mixed into a block and
  gossiped validator→validator, where there's no longer any IP to leak.

This is the "...only for tiny parts — the signature/submission..." idea made
precise: a shielded tx **is** essentially a signature + commitment, a few
hundred bytes. Tor it. Tor nothing else.

## The clever enforcement — a hard size guard

`EgressClass::max_tor_bytes()` caps each Tor class (16 KB submit / 8 KB query /
64 KB fetch). `route_egress(class, len)` returns `RejectedTooBig` if a payload
exceeds its cap — so a bug (or an attacker) **physically cannot** push bulk
gossip through Tor. Tor is *structurally* reserved for the tiny stuff; it's not
a convention you can accidentally violate.

```rust
match sigil_net::route_egress(class, payload.len()) {
    EgressRoute::Mesh            => wg_send(payload),                 // fast path
    EgressRoute::Tor             => tor.dial_isolated(target, peer)?, // per-peer circuit
    EgressRoute::RejectedTooBig{..} => return Err(..),               // never firehose Tor
}
```

Each Tor egress uses the **per-peer stream isolation** already shipped
(`TorClient::dial_isolated`) — so even the tiny private payloads to different
peers ride different circuits (no cross-peer correlation).

## Status

- ✅ `sigil_net::tor_policy` — `EgressClass`, `route_egress`, size guard. 4/4 tests.
- ✅ Composes with shipped per-peer Tor stream isolation + the live `wg+tor` node.
- ⏭ NEXT (wiring): classify real call sites — shielded-tx submit → `PrivateSubmit`,
  tip-proof RPC → `LightQuery`, oracle/release → `OffMeshFetch`, gossip → `HotMesh`;
  and the libp2p-over-Tor send path for the Tor classes (SOCKS / Arti transport),
  which only ever carries ≤16 KB by construction.

— rocky-sigil 🟣🧅
