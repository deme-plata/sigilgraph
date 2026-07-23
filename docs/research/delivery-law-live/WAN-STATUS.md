# WAN-Scale Live-Mesh Measurement — Status Record (2026-07-23)

The netem promotion experiment (MEASUREMENT.md) left one open extension: measure
delivery on the WAN-scale live mesh. This record documents the attempt, what it
found, and what it is blocked on. No production state was changed.

## Blocked: no accessible WAN vantage point

- **Delta (5.79.79.158)**: back online after its month-long outage — but freshly
  reinstalled: **SSH host key changed** AND our key is no longer authorized
  (`Permission denied (publickey)`). Operator must re-key it (and should confirm
  the reinstall with the provider, since a changed host key is also the MITM
  signature). Until then, no access.
- **Gamma (109.205.176.60)**: down (100% packet loss).
- **Beta**: decommissioned 2026-07-02.
- Renting a cloud vantage (Vast) is possible but is operator-gated spend
  (propose-only).

## Staged and proven: the WAN kit

- `delivery-probe` hardened for live meshes: **peer pinning** (trials only ever
  target the /p2p/ ids passed in `--peers` — kad/identify discovery on a live mesh
  otherwise pollutes the peer set; this bit us), **quiet mode** (no subscription to
  the live block firehose), **range planning** (`--base/--span/--window` — requests
  must land inside the served range), early out-file creation.
- **Portable binary**: bullseye + zig-cc `x86_64-linux-gnu.2.27` build (the HiveOS
  recipe) → 5.9 MB release, glibc ceiling **2.25**, verified running on debian:12.
  Build: `scripts/dp-portable-inner.sh` via `rust:1-bullseye` (see git history for
  the docker invocation). Binary at `/home/storage/tmp/dp-target-bullseye/release/`.
- End-to-end validated against a throwaway sigil-node (same release binary as the
  live producer): requests served, real zstd header responses.

## Found along the way: live-producer serving to NEW clients is flaky

While validating against the live producer (`…MSof`, :9501, chain ≈31.6M), from the
local (loopback) vantage, 2026-07-22 16:20 → 2026-07-23 03:30:

- The two established clients are served continuously (…B8Qj ≈2.1 MB/s header
  chunks, ≈95% serve-cache hit; the H7 follower's tip polls).
- An **external fresh client (…Ac9SdC9wN) successfully bootstrapped from genesis**
  at 20:00–20:05+ (32,769-header / ≈2.3 MB chunks, from height 0 upward) — so
  new-client serving is not dead.
- **My probe clients were served ≈2 times out of ≈35 attempts** across six hours
  (journal-verified serves that my client nevertheless timed out on), and a real
  local sigil-node client (valid sync-auth handshake, connected, 60 s) received
  zero serves and zero peer-heights gossip. flux-p2p's internal request timeout
  (30 s) fires with no response and no OutboundFailure.
- Eliminated: request shape (throwaway serves it), ranges (exact serve-cache-key
  requests also unanswered), handshake enforcement (env unset; log-only; no
  refusals logged), stale binary (`/proc/exe` = on-disk Jul 19 build, same as
  throwaway), gossip-firehose interference (quiet probe unchanged).
- Not yet isolated: loopback-vs-public-interface vantage (the one success case,
  …Ac9, came in over the public interface), event-queue contention on the busy
  producer, response-path loss into short-lived clients. Root-causing needs either
  a public-vantage probe (blocked, above) or producer-side instrumentation/restart
  (operator call — it is the sole producer).

Delivery-law relevance, stated carefully: from this vantage, the per-request
failure rate for a NEW client against the live producer over ~35 attempts was
p̂ ≈ 0.94 — not a property of the gossip/redundancy law (these are point-to-point
serves), but a live-mesh reliability datum the WAN measurement will have to
account for. It is also exactly the class of fact the measure-the-live-path rule
exists to surface.

## To run the WAN measurement once a vantage exists

1. scp the portable binary to the vantage box.
2. Cell W1 (live mesh, gentle): `delivery-probe probe --peers
   /ip4/89.149.241.126/tcp/9501/p2p/<producer-id> --r 1 --trials 600
   --concurrency 4 --base <tip-200k> --span 8192 --window 100000` (producer id:
   `/tmp/sigil-node-g0.peerid` on Epsilon is stale — take it from the unit's
   ExecStartPost stamp, currently `…MSof`, or the serve log).
3. Cell W2 (composition over a real WAN path): start 3 `delivery-probe serve`
   responders on Epsilon public ports, probe r=1,2,3 from the vantage. Scope note
   for the write-up: all three share one physical route — cross-peer independence
   is not multi-path at fleet scale.
4. Analyze with `scripts/dp-analyze.py`; grade with scopes attached.
