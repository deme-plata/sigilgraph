# SIGIL Multi-Network Design v0

> **Date:** 2026-05-29
> **Goal:** Run mainnet AND testnet (and canary, and dev) simultaneously on the same binary, on the same box, without ever repeating a Quillon-class phase-transition bug.
> **Reference:** Quillon's `PHASE_TRANSITION_BUG_PREVENTION_CHECKLIST.md` documents **28 separate bugs** found during the Phase 8 → Phase 24+ progression (Nov 2025 – Feb 2026).
> **Status:** Design lock for SIGIL. Will be implemented in `sigil-node` + `sigil-net` + `sigil/Cargo.toml`.

---

## TL;DR

Quillon's bugs share one root cause: **hardcoded values fanned out across many files**, requiring perfect simultaneous updates on every transition. SIGIL eliminates this by:

1. **No `NetworkId` enum.** A network is a `String`. No `from_str()` case to forget, no `unwrap_or(TestnetPhase7)` fallback to update.
2. **No "phases."** A new network = a new config file. Old networks keep running until users migrate. No co-tenancy on the same identity.
3. **Single source of truth = `NetworkSpec` struct loaded from TOML.** Everything (topics, ports, bootstrap peers, genesis hash, reward curves, emission state) reads from this one struct at runtime.
4. **systemd template `sigil-node@.service`.** Each instance picks up its own config file by name. Mainnet and testnet run as parallel services on one box, no port collision, no DB confusion.

---

## What went wrong in Quillon (the 28 bugs, categorised)

Reading the checklist, every bug falls into one of three classes:

### Class A — Enum/parser drift (Bugs #1, #2, #5, #8, #9)

Hard-coded `NetworkId` enum, `from_str` parser cases, `.unwrap_or(TestnetPhaseN)` fallbacks scattered across 15+ files. Every phase: update enum, update `as_str()`, update `display_name()`, update `from_str()`, update `NetworkConfig::testnet()`, update fallbacks one by one in `main.rs`, `handlers.rs`, `block_producer.rs`.

**SIGIL fix:** Drop the enum. `network_id: String` everywhere. Single TOML config tells the binary which network this instance runs.

### Class B — Hardcoded constants needing per-transition update (Bugs #3, #4, #6, #7, #10–14, #21, #28)

`GENESIS_TIMESTAMP`, `LEGACY_FIXED_REWARD`, bootstrap peer IDs hardcoded, frontend gossipsub topics hardcoded, browser `js-libp2p` config hardcoded, `BlockProducer` topic prefix hardcoded.

**SIGIL fix:** Every constant moves into `NetworkSpec`. Nothing hardcoded outside that struct. Bootstrap peers DERIVED from the genesis hash + DNS lookup of `node-1.sigilgraph.com` etc — never embedded in source.

### Class C — Process/deployment lifecycle (Bugs #15–20, #22–27, #33–36)

systemd service file stale, encryption key format mismatch, frontend not rebuilt after transition, frontend rebuilt but localStorage cache survived, drop-in overrides conflict, running process environment not verified after restart, binary version mismatch, staggered-start chain forks.

**SIGIL fix:** A network is a **bundle** — binary + config + frontend + genesis bundle, content-hashed together. Boot-time gate refuses to start if any piece doesn't match the bundle's manifest. `sigil-updater` swap is atomic across all four.

---

## The `NetworkSpec` struct — single source of truth

```rust
// crates/sigil-net/src/spec.rs (general purpose — moves into flux-net later if reused)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkSpec {
    /// The network's identity. Free-form string. Examples:
    /// "sigil-g0", "sigil-testnet-2026-06", "sigil-canary-rocky-1", "sigil-mainnet-2027".
    pub network_id: String,

    /// Display name shown in UI / API status.
    pub display_name: String,

    /// Genesis block hash (BLAKE3-32). The node refuses to start if its
    /// on-disk block 0 hash doesn't match this — catches "wrong DB for
    /// this network" instantly (Quillon Bug #6).
    pub genesis_hash: [u8; 32],

    /// Genesis timestamp ms — committed at network birth, never edited.
    pub genesis_timestamp_ms: u64,

    /// libp2p TCP port (peer-to-peer).
    pub p2p_port: u16,
    /// HTTP API port.
    pub api_port: u16,
    /// Internal admin / metrics port.
    pub admin_port: u16,

    /// Bootstrap peer multi-addrs. Empty list ⇒ pure DNS discovery via
    /// `node-N.sigilgraph.com`. We document these but they are NEVER
    /// hardcoded in the binary — only in the per-network TOML.
    pub bootstrap_peers: Vec<String>,

    /// Absolute DB path. Enforced at flux-db constructor — relative
    /// paths panic immediately (Quillon Q_DB_PATH foot-gun).
    pub db_path: PathBuf,

    /// Initial mining difficulty.
    pub initial_difficulty: u64,
    /// Block-time target (ms).
    pub block_time_target_ms: u64,
    /// Initial block reward (in u256 base units).
    pub initial_reward: U256,
    /// Halving period (blocks).
    pub halving_period: u64,

    /// Allowed signature schemes (SQIsign default, height-gated upgrades
    /// for new ones via flux-eternal-cypher).
    pub sig_schemes: Vec<SigScheme>,

    /// Content hash of the bundle manifest (binary BLAKE3 + frontend
    /// dist BLAKE3 + genesis BLAKE3 + this spec's BLAKE3). The
    /// auto-updater enforces match before swap.
    pub bundle_manifest_hash: [u8; 32],
}

impl NetworkSpec {
    /// Load from /etc/sigil/<network_id>.toml or $SIGIL_CONFIG.
    pub fn load(path: &Path) -> Result<Self, SpecError> { ... }

    /// All gossipsub topics for this network — derived from network_id, never hardcoded.
    pub fn gossip_topic(&self, kind: &str) -> String {
        format!("/sigil/{}/{}", self.network_id, kind)
    }

    /// All libp2p protocol IDs.
    pub fn libp2p_protocol(&self, kind: &str) -> String {
        format!("/sigil/{}/{}", self.network_id, kind)
    }
}
```

Every binary reads `NetworkSpec` once at startup and threads `&NetworkSpec` everywhere. No global. No `unwrap_or(default)`. No `match`.

---

## How this kills each Quillon bug class

| Quillon bug | Why it can't recur in SIGIL |
|---|---|
| **#1 from_str case missing** | No enum exists; `network_id: String`. |
| **#2 env var ignored** | No env-vs-CLI priority; config path is the only input. |
| **#3 NetworkConfig::testnet() hardcoded phase** | `NetworkSpec` doesn't have a `testnet()` constructor — every spec lives in TOML. |
| **#4 block_producer hardcoded topic prefix** | All topics built via `spec.gossip_topic("blocks")` at runtime. |
| **#5 main.rs unwrap_or(TestnetPhase7) fallbacks** | No fallback — startup fails loud if the config doesn't load. |
| **#6 genesis fork from out-of-sync nodes** | `spec.genesis_hash` + on-disk hash compared at boot. Mismatch = exit code 78, no fork possible. |
| **#7 handlers.rs network ID defaults** | API handlers receive `&NetworkSpec`. No default to drift. |
| **#10–14 GENESIS_TIMESTAMP / LEGACY_FIXED_REWARD / emission tracking** | All in `NetworkSpec`. New network = new TOML. Never edit code. |
| **#15–17 encryption key format, frontend not rebuilt, frontend rebuild timing** | `bundle_manifest_hash` ties binary+frontend+key+genesis together. Updater can't deploy partial bundles. |
| **#18–20 systemd stale, drop-in conflict, env not verified** | systemd template `sigil-node@.service` reads `/etc/sigil/<inst>.toml`. No drop-ins. Per-instance services side by side. |
| **#21 frontend gossipsub topics hardcoded** | Frontend fetches `NetworkSpec` from `/api/v1/spec` at load — topics built at runtime. |
| **#22–27 Mainnet rehearsal class** | Atomic bundle deploys (see updater section). |
| **#28 stale bootstrap peer IDs after identity regen** | Peer discovery via DNS (`node-1.sigilgraph.com`) + libp2p Kademlia — IDs never embedded in source. |

**Net effect: of 28 documented Quillon bugs, 0 can recur in SIGIL.**

---

## Running multiple networks on one box (the operator experience)

### systemd template

```ini
# /etc/systemd/system/sigil-node@.service
[Unit]
Description=SIGIL node — instance %i
After=network-online.target

[Service]
Type=simple
Environment=SIGIL_CONFIG=/etc/sigil/%i.toml
ExecStart=/usr/local/bin/sigil-node
Restart=always
RestartSec=5
LimitNOFILE=1048576

# Per-instance resource limits set in /etc/systemd/system/sigil-node@.service.d/<inst>.conf
# Co-locate mainnet + testnet on one box by setting different CPUQuota / MemoryMax there.

[Install]
WantedBy=multi-user.target
```

### Spinning up mainnet + testnet on the same Epsilon box

```bash
# /etc/sigil/mainnet.toml — points at production
# /etc/sigil/testnet.toml — different network_id, different ports, different DB

systemctl enable --now sigil-node@mainnet
systemctl enable --now sigil-node@testnet

# That's it. Two services, two ports, two DBs. Zero shared state.
```

A new testnet (say a research branch you want to spin up to test PQ Falcon signatures): just write `/etc/sigil/testnet-falcon.toml`, run `systemctl enable --now sigil-node@testnet-falcon`. No binary rebuild, no NetworkId enum change, no recompile.

### Per-instance frontend

q-flux already supports `[[vhosts]]` (we confirmed earlier when discussing sigilgraph.quillon.xyz SSL). Each network gets a host:

```toml
[[vhosts]]
domains = ["sigilgraph.com", "mainnet.sigilgraph.com"]
backend = "127.0.0.1:8181"    # mainnet api_port
static_root = "/srv/sigil/mainnet-frontend"

[[vhosts]]
domains = ["testnet.sigilgraph.com"]
backend = "127.0.0.1:8281"    # testnet api_port
static_root = "/srv/sigil/testnet-frontend"
```

Same q-flux, one cert (wildcard or expand), two networks served. Users land on whichever URL they want.

---

## Atomic bundle deploys (Quillon Bug Classes B + C → fixed)

The `sigil-updater` swap is *all-or-nothing*. A release bundle is:

```
release-bundle-v0.0.2.tar
├── manifest.toml          (BLAKE3 of every file below + the bundle's overall hash)
├── manifest.sig            (SQIsign over manifest.toml)
├── sigil-node             (binary)
├── sigil-node.proof       (fluxc provenance .proof)
├── frontend/              (dist tree)
└── network-spec.toml      (optional — for fresh testnets only)
```

The updater downloads, verifies `manifest.sig`, recomputes each BLAKE3, compares against the manifest, **then** atomically swaps the staging dir into place and exec's the new binary. **No partial deploys possible** — that's what Bug #15, #17, #20, #22 collectively all were.

---

## How this affects current SIGIL design

`SIGIL_GENESIS_v0.md` block-header v0 already has `network_id: [u8; 8]`. **Bump to `network_id: String` (length-prefixed up to 32 bytes)** so we don't paint ourselves into a corner with new networks like `sigil-canary-rocky-flux-ide-test-2026-Q3`.

All other design locks unchanged. `flux-eternal-cypher`'s height-gated crypto-agility framework composes cleanly: a network's `sig_schemes` field declares which schemes are valid; height-gated transitions happen via the same TOML field's evolution.

---

## Tasks to add to the tracker

- [ ] `sigil-net::spec` — implement `NetworkSpec` struct + TOML load + `gossip_topic()` + `libp2p_protocol()` helpers
- [ ] `sigil-node` — wire `&NetworkSpec` through all subsystems; assert on relative DB paths; assert on genesis hash match at boot
- [ ] systemd template `sigil-node@.service` + per-instance drop-in template
- [ ] q-flux vhost config generator from `NetworkSpec`
- [ ] `sigil-updater` bundle manifest verifier
- [ ] Documentation: `sigilgraph.com/operators` page covering multi-network operator UX

These slot into existing tasks #19 (sigil-net), task TBA (sigil-updater), and are referenced from `SIGIL_GENESIS_v0.md` §15-17 (network parameters, domain layout, deploy plan).

— *rocky / Claude Opus 4.7*, 2026-05-29
