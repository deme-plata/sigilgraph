# SIGIL Operator Runbook — Phase 0

> Practical guide for running SIGIL nodes day-to-day. Copy-pasteable commands, visible failure modes, no architectural waffle (see `SIGIL_GENESIS_v0.md` for design).

**Audience:** humans + AI agents operating SIGIL nodes on Delta + Epsilon. Assumes you can SSH to the target host.

**Date stamp:** 2026-05-29. Phase 0 = "minimum lovable chain" — see *Known Phase 0 Limitations* at the bottom.

---

## 0. Topology cheat sheet

| Host | IP | Role | Ports | Profile |
|---|---|---|---|---|
| Delta | `5.79.79.158` | Dedicated SIGIL node | 9501 (P2P TCP), 8181 (API) | no resource caps |
| Epsilon | `89.149.241.126` | SIGIL co-located w/ Quillon prod | 9501 (P2P TCP), 8181 (API) | CPU=200%, MemoryMax=4G — protect Quillon |

SSH keyed from your dev box (Beta or wherever this repo lives) for both hosts.

**Never run SIGIL on Beta** — Beta is Quillon's source-of-truth dev node. Never deploy SIGIL with `Q_*` env vars set — those belong to Quillon and will collide.

---

## 1. First-time setup per host (one-shot)

Run once per host before any `release.sh` / `deploy.sh` ever fires.

```bash
cd /home/storage/deepseek-codewhale/sigil
scripts/host-setup.sh delta     # or epsilon
```

What this does:
1. `mkdir -p /home/orobit/sigil-data/{bin,db,staging}` (chowned to `orobit`)
2. scp's `sigil-updater` binary into `/home/orobit/sigil-data/bin/` (needed for verify+apply)
3. Renders `scripts/sigil-node.service.<host>` via `render-systemd.sh` (Delta = no caps; Epsilon = CPU/Mem caps to protect Quillon)
4. Installs the unit at `/etc/systemd/system/sigil-node.service`
5. `systemctl daemon-reload` (does NOT start — sigil-node binary lands on first `deploy.sh`)
6. Prints capacity (cores/RAM/disk) + firewall hints

**Verify after running:**
```bash
ssh root@<host> "/home/orobit/sigil-data/bin/sigil-updater version"
# → sigil-updater 0.0.1 (schema v0)

nc -zv <host-ip> 9501    # P2P port reachable
nc -zv <host-ip> 8181    # API port reachable
```

If the `nc` checks fail, open the ports in your provider panel (Hetzner etc.) AND add iptables rules:
```bash
ssh root@<host> "iptables -I INPUT -p tcp --dport 9501 -j ACCEPT -m comment --comment 'SIGIL P2P'"
ssh root@<host> "iptables -I INPUT -p tcp --dport 8181 -j ACCEPT -m comment --comment 'SIGIL API'"
ssh root@<host> "netfilter-persistent save"
```

---

## 2. Cutting a release

Done from your dev box (Epsilon or Beta — wherever this workspace lives).

```bash
cd /home/storage/deepseek-codewhale/sigil
scripts/release.sh 0.0.2 --note "Phase 0 patch — sync gate fixup"
```

5-step output:
1. Build sigil-node (release profile, ~95s cold, ~10s warm thanks to FLUXFOOD)
2. Copy to `releases/v0.0.2/sigil-node-v0.0.2`
3. Compute BLAKE3 → write provenance proof
4. Sign announcement with `keys/rocky-release.sk.hex`
5. Verify the roundtrip (sig + binary hash) before declaring success

**Output artifacts** in `releases/v0.0.2/`:
- `sigil-node-v0.0.2` — the 8.5 MB binary
- `sigil-node-v0.0.2.proof` — provenance JSON (sig is phase-0 placeholder; BLAKE3 is real)
- `sigil-node-v0.0.2.announcement.json` — signed ReleaseAnnouncement (~12 KB)

**Pre-flight checks before publishing:**
```bash
# Sanity-verify the announcement yourself
target/debug/sigil-updater verify \
    --announcement releases/v0.0.2/sigil-node-v0.0.2.announcement.json \
    --binary       releases/v0.0.2/sigil-node-v0.0.2
# Must print "announcement signature: OK" + "binary hash: OK"
```

If verify fails locally, your keypair or binary is corrupt — DO NOT deploy.

---

## 3. Deploying a release

Per host. Same flow Delta + Epsilon.

```bash
scripts/deploy.sh delta 0.0.2
scripts/deploy.sh epsilon 0.0.2
```

5-step deploy:
1. scp binary + announcement to `/home/orobit/sigil-data/staging/`
2. Remote verify (sig + hash) — fails fast if either is wrong
3. Remote `sigil-updater apply` — atomic `.new → target → .bak` swap
4. `systemctl restart sigil-node`
5. 10s journal health check — looks for `panic|fatal|preflight_fail`

**Auto-rollback fires** if step 5 sees any of those markers — `.bak` is restored, `systemctl restart` re-runs.

**After deploy, manually verify:**
```bash
ssh root@<host> "systemctl is-active sigil-node && journalctl -u sigil-node --no-pager -n 20"
# Look for: "peer connected" lines, no "Error" lines, no "preflight_fail"
```

If the auto-rollback fired, your release has a bug — fix it locally, cut a new version, retry. **Do not edit the deployed binary in place.**

---

## 4. Day-to-day monitoring

The minimum operator surface:

```bash
# Health snapshot
ssh root@<host> "systemctl is-active sigil-node && systemctl status sigil-node --no-pager | head -15"

# Live log tail (Ctrl-C to exit)
ssh root@<host> "journalctl -u sigil-node -f"

# Peer count + tip (once API ports are wired in P1)
curl -s http://<host-ip>:8181/api/v1/status | jq '{peers, tip_height, version}'
```

**What "healthy" looks like in the journal:**
- `Updated tip height to <N>` lines tick at the block cadence
- `peer connected /ip4/.../tcp/9501` after restarts
- No `ERROR` or `WARN` repeating
- Memory stable (check `systemctl show sigil-node -p MemoryCurrent` over time)

**What unhealthy looks like:**
| Symptom | Likely cause | Fix |
|---|---|---|
| `preflight_fail` + exit 78 | Boot-time state-root mismatch | Stop — DO NOT restart. Investigate DB corruption. (Phase 0: gate not implemented yet, so this won't fire.) |
| Repeated `peer disconnect` | Wrong `SIGIL_BOOTSTRAP_PEERS` or firewall | Check env in unit file + `nc -zv` to peer |
| OOM kill on Epsilon | SIGIL hitting the 4G cap | Investigate — Epsilon co-located, can't safely raise caps |
| `BLAKE3 mismatch` on apply | Binary corruption during scp | Re-run `deploy.sh`, scp retries the bytes |
| `gh auth` errors in logs | NOT a SIGIL error — Quillon's leakage | Confirm you didn't deploy with a Quillon env file |

---

## 5. Rollback

If a deployed release breaks something the auto-rollback didn't catch:

```bash
# Manual rollback on a single host
ssh root@<host> "\
    mv /home/orobit/sigil-data/bin/sigil-node /home/orobit/sigil-data/bin/sigil-node.broken && \
    mv /home/orobit/sigil-data/bin/sigil-node.bak /home/orobit/sigil-data/bin/sigil-node && \
    systemctl restart sigil-node && \
    systemctl is-active sigil-node \
"
```

The `.bak` is the version that was running before the last `deploy.sh`. If you've deployed twice since the bad version, `.bak` is the second-to-last good version, not the last. Cut a new release pointing at the known-good source tree instead of chaining rollbacks.

---

## 6. Manual SCP/systemd hack (preserved for forensics)

Before `release.sh` + `deploy.sh` existed, the operator (rocky) would manually:

```bash
# build
fluxc build --package sigil-node --release

# scp by hand
scp target/release/sigil-node root@<host>:/home/orobit/sigil-data/bin/sigil-node.new

# swap
ssh root@<host> "\
    mv /home/orobit/sigil-data/bin/sigil-node /home/orobit/sigil-data/bin/sigil-node.bak && \
    mv /home/orobit/sigil-data/bin/sigil-node.new /home/orobit/sigil-data/bin/sigil-node && \
    chmod 755 /home/orobit/sigil-data/bin/sigil-node && \
    systemctl restart sigil-node \
"
```

**Don't use this anymore** — no signature check, no provenance, no rollback, no audit trail. Kept here as a debugging aid for when `deploy.sh` itself breaks and you need a fallback path. If you find yourself running this, file an issue against `deploy.sh` — that's the bug.

---

## 7. Keypair management

The release-signing key lives at `keys/rocky-release.{sk,pk}.hex` in this workspace. **The secret key gates every release** — losing it = can't ship updates; leaking it = an attacker can ship malicious updates to every SIGIL node.

**Today (Phase 0):**
- Single key, single author (`rocky`)
- `.sk.hex` is chmod 600
- Stored in the workspace, NOT in git (verify: `cat .gitignore | grep keys`)

**Phase 1 (when validator set lands):**
- Multi-key — releases need M-of-N validator signatures
- Per-validator `.sk.hex` lives only on the validator's box
- `sigil-updater` verifies the threshold before applying

**If you need a new key today** (lost, rotated, or scaffolding a second author):
```bash
target/debug/sigil-updater keygen --out-prefix keys/rocky-release-v2
# Then update scripts/release.sh's SK_HEX/PK_HEX references — they're hardcoded.
```

**Never commit `.sk.hex` to git.** Verify before every `git add`:
```bash
git status keys/
# .sk.hex should NOT be in untracked or staged
```

---

## 8. Known Phase 0 limitations

These are deliberate cuts to ship the prototype fast. Each has a planned phase to fix.

| Limitation | Why deferred | Fix phase |
|---|---|---|
| Pre-flight state-root gate not enforced | sigil-state's hash_map was found buggy mid-session; gate lands AFTER bug fix shipped + verified | P1 |
| Release sig is SQIsign over the announcement, NOT a real fluxc .proof of the binary | `fluxc compile-native --provenance` doesn't yet support multi-module crates | P1 |
| Gossipsub release transport not wired | flux-p2p::NetworkManager::drain_events() just shipped (rocky-67); sigil-updater gossipsub integration is next claim | P1 |
| `activation_height` is verified but not enforced as a wait condition | sigil-node needs a chain-height source to gate the swap on | P1 |
| Single-author release signing | Multi-key validator set isn't designed yet | P2 |
| Pre-flight refuses on hash mismatch but no automatic recovery | Don't want to auto-resync on first failure — operator must look | P2 |
| API on `:8181` not wired beyond `/api/v1/status` | sigil-node's HTTP layer is stubbed | P1 |

---

## 9. Quick reference card

```bash
# First time per host
scripts/host-setup.sh   <delta|epsilon>

# Cut a release (dev box)
scripts/release.sh      <version>  --note "..."

# Deploy a release (one host at a time)
scripts/deploy.sh       <delta|epsilon>  <version>

# Monitor
ssh root@<host> "journalctl -u sigil-node -f"

# Rollback
ssh root@<host> "mv /home/orobit/sigil-data/bin/sigil-node{,.broken} && \
                 mv /home/orobit/sigil-data/bin/sigil-node{.bak,} && \
                 systemctl restart sigil-node"

# Key rotation
target/debug/sigil-updater keygen --out-prefix keys/rocky-release-vN
```

---

## 10. When to escalate (and to whom)

- **Sustained `peer disconnect` storms** → check both hosts' firewalls + bootstrap-peer env. If both are right and disconnects persist, ping `rocky` (network track) via `mcp__fluxc__flux_swarm_message`.
- **State-root mismatch in journal** → halt the affected node, ping `rocky-sigil` (state track). DO NOT auto-restart — that's what exit 78 prevents.
- **`sigil-updater` self-test fails** → ping `rocky-updater` (release track). Don't keep deploying.
- **Anything affecting Quillon production** (Epsilon resource starvation, port conflict) → STOP SIGIL, ping operator immediately. Quillon production is sacred, SIGIL is not.

— rocky-updater, 2026-05-29
