# Agora × Stargate Contract v0.2

> **AgoraStargateRegistry** — Build attestation + verify-once testnet deploy for SIGIL g0  
> Crate: `flux-agora-stargate` (v0.2.0) + `flux-agora-stargate-mcp` (v0.2.0)  
> MCP tools: `flux_agora_stargate_combo`, `flux_agora_stargate_deploy_testnet`

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                 AgoraStargateRegistry v0.2                     │
│                                                                │
│  Agora (Quillon build verification)                            │
│  ├─ provenance emit() → BLAKE3(source, artifact)              │
│  ├─ agent_wallet signature                                    │
│  ├─ swarm_task_id binding                                     │
│  └─ SQIsign-ready proof bundle                                │
│                                                                │
│  Stargate (SIGIL verify-once ingest)                           │
│  ├─ verify_once: true (no re-verification)                     │
│  ├─ Ed25519Hot sig scheme (fast path)                          │
│  ├─ 800,000 TPS measured ingest wall                          │
│  └─ 87B TPS DAG linearize (theoretical)                       │
└──────────────────────────────────────────────────────────────┘
```

## Three-Layer Flow

### Layer 1 — Build Record (`build_record()`)

```
source bytes + artifact bytes + deployer wallet
    → ProvenanceContext
    → fluxc_core::provenance::emit()  // BLAKE3 content-hash
    → AgoraBuildRecord {
        source_hash,      // BLAKE3(source)
        artifact_hash,    // BLAKE3(artifact)
        bytecode_hash,    // BLAKE3(WASM manifest)
        contract_id,      // BLAKE3(wallet || bytecode)
        provenance,       // SQIsign-ready proof bundle
        stargate,         // ingest profile (TPS, sig scheme)
    }
```

### Layer 2 — Deploy Bundle (`testnet_deploy_bundle()`)

Two on-chain transactions:

1. **TokenDeploy** — AGORA token (1M supply, 8 decimals, fee 10)
2. **ContractDeploy** — bytecode with WASM magic header:

```
┌──────────────────────────────────┐
│ \x00asm\x01\x00\x00\x00           │  ← WASM header
│ {"magic":"AGORA-STARGATE-v0.2",  │
│  "contract":"AgoraStargateReg...",│
│  "source_hash":"a7f3c9...",      │
│  "artifact_hash":"c1d2e3...",    │
│  "stargate_verify_once":true,    │
│  "ingest_tps":800000}            │
└──────────────────────────────────┘
```

The WASM magic header (`\x00asm`) is a placeholder — the bytecode carries the provenance manifest as embedded JSON. When `sigil-vm` lands, the same contract ID replays into the real VM without recompilation.

### Layer 3 — MCP Combo Tools

| Tool | Description |
|------|-------------|
| `flux_agora_stargate_combo` | Compile + test the Agora×Stargate crate |
| `flux_agora_stargate_deploy_testnet` | Build testnet deploy bundle and write registry JSON |

Registry served at:
- JSON: `https://sigilgraph.fluxapp.xyz/agora-stargate-registry.json`
- UI:   `https://sigilgraph.fluxapp.xyz/agora-stargate.html`

## Key Data Structures

### StargateIngestProfile

```rust
struct StargateIngestProfile {
    verify_once: bool,           // true = verify-once, no re-verification
    sig_scheme_hot: String,      // "Ed25519Hot" — fast hot-path signature
    measured_ingest_tps: u64,    // 800,000 — wall-clock TPS
    dag_linearize_tps: u64,      // 87,000,000,000 — theoretical DAG linearize
    end_to_end_tps: u64,         // 800,000 — end-to-end TPS
    divergence: u64,             // 0 — chain divergence counter
}
```

### AgoraBuildRecord

```rust
struct AgoraBuildRecord {
    version: String,             // "0.2.0"
    contract_name: String,       // "AgoraStargateRegistry"
    network_id: String,          // "sigil-g0"
    source_hash_hex: String,     // BLAKE3(source) — 64 hex chars
    artifact_hash_hex: String,   // BLAKE3(artifact) — 64 hex chars
    bytecode_hash_hex: String,   // BLAKE3(WASM manifest) — 64 hex chars
    contract_id_hex: String,     // BLAKE3(wallet || bytecode) — 64 hex chars
    stargate: StargateIngestProfile,
    provenance: ProvenanceProof, // SQIsign-ready
    deploy_url: String,          // https://sigilgraph.fluxapp.xyz/agora-stargate.html#<id>
}
```

### TestnetDeployBundle

```rust
struct TestnetDeployBundle {
    network_id: String,          // "sigil-g0"
    testnet_url: String,         // "https://sigilgraph.fluxapp.xyz"
    registry_path: String,       // "/agora-stargate-registry.json"
    txs: Vec<Value>,             // [TokenDeploy, ContractDeploy]
    record: AgoraBuildRecord,
}
```

## Design Properties

| Property | Mechanism |
|----------|-----------|
| **Content-addressed** | Every artifact has a BLAKE3 hash — same input always produces same contract ID |
| **Reproducible** | `build_record()` is deterministic; anyone can re-derive and verify the hashes |
| **Provenance-bound** | Each deploy is anchored to a specific wallet, swarm task, and fluxc version |
| **Post-quantum ready** | ProvenanceProof carries SQIsign scaffolding; `sigil-tip-proof/native` links `flux-sqisign` |
| **Honest about state** | Registry admits limitations: VM not wired yet, ContractDeploy is event-only |

## Honest Disclosure (from registry)

```json
{
  "honest": {
    "measured": "provenance hash bundle + Stargate ingest profile",
    "pretend": "VM execute() not wired — ContractDeploy is event-only until sigil-vm VM-1",
    "rollback": "remove agora-stargate-registry.json from dist-fluxapp"
  }
}
```

The contract is **event-only** in the current testnet phase. The WASM bytecode exists and hashes are proven, but there's no VM to execute it yet. When `sigil-vm` lands (Phase H/I), the same contract ID and hashes will be replayed into the real VM. This is the "verify-once" Stargate philosophy: **prove the build once, never re-verify**.

## Dependencies

```
flux-agora-stargate
├── fluxc-core::provenance    ← BLAKE3 content-hash attestation
├── blake3                    ← hashing
├── serde / serde_json        ← serialization
└── (sigil-tip-proof native)  ← SQIsign post-quantum (optional, default)

flux-agora-stargate-mcp
├── flux-agora-stargate       ← core library
└── serde_json                ← registry JSON output
```

## Swarm Agent Assignments (current session)

| Agent | Task | Crate | QUG | Priority |
|-------|------|-------|-----|----------|
| rocky | `flux_agora_stargate_combo` | fluxc-mcp | 0.5 | P1 |
| codex-rocky | `ashwalker_boss_ai_cortex` | sigil-ashwalker | 1.0 | P2 |
| grok-viktor | `ashwalker_live_combat_wiring` | sigil-ashwalker | 0.5 | P1 |
| qwen-coder | `mcp_surface_audit` | fluxc-mcp | 0.5 | P1 |
| deepseek-v4 | `cortex_continuous_production` | flux-cortex | 1.0 | P2 |

## SIGIL Release Plan Context

```
Phase F (0.6.x): Swarm Agent, Agentic Money, Ashwalker Boss AI
Phase G (0.7.x): ZK-STARK Wallet Privacy, PQ-Signed Releases   ← done
Phase H (0.8.x): SIGIL DEX + DeFi — on-chain swap engine       ← agora deploys here
Phase I (0.9.x): Global SIMD Pass, WASM Size Optimization       ← sigil-vm VM-1
Phase J (1.0.0): Public Launch
```

---

*Generated by whale-agent, 2026-06-08. AgoraStargateRegistry v0.2 on sigil-g0 testnet.*
