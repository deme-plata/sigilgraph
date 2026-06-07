# ⚡ FLUX LOCAL WALLET — flux:// Protocol Architecture
## For SIGIL Node Operators & The Rocky Surprise

**Author:** rocky (Claude Opus 4.7, Epsilon)
**Date:** 2026-06-07
**Status:** Working — tested on Epsilon

---

## 0. What We Built

A **local wallet server** that SIGIL node operators access via two protocols:

| Protocol | URI | What Opens |
|----------|-----|------------|
| `flux://` | `flux://wallet` | Full SIGIL wallet UI |
| `flux://` | `flux://sigil-top` | SIGIL-top cockpit |
| `flux://` | `flux://explorer` | Block explorer |
| `flux://` | `flux://bridge` | Bridge status |
| `https://` | `https://localhost:8443/` | sigilgraph login |
| `https://` | `https://localhost:8443/wallet/` | SIGIL wallet |
| `https://` | `https://localhost:8443/sigil-top/` | SIGIL-top cockpit |
| `https://` | `https://localhost:8443/bridge-status` | JSON status |

**The killer feature:** Type `flux://wallet` in any browser, and it opens your local SIGIL
wallet. No internet needed — it connects to your local `sigil-node` on `:8181`.

---

## 1. Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                   NODE OPERATOR'S MACHINE                     │
│                                                               │
│  Browser                     flux-local-wallet (:8443)        │
│  ┌─────────┐    flux://     ┌──────────────────────────┐     │
│  │ flux://  │──────────────→│  HTTPS Server (TLS)      │     │
│  │ wallet   │               │                           │     │
│  └─────────┘               │  ┌─────────────────────┐  │     │
│                              │  │ sigilgraph-login   │  │     │
│  ┌─────────┐    https://    │  │ (mnemonic + auth)  │  │     │
│  │https:// │──────────────→│  └───────┬─────────────┘  │     │
│  │localhost│               │          │ redirect       │     │
│  └─────────┘               │  ┌───────▼─────────────┐  │     │
│                              │  │ sigil-wallet UI    │  │     │
│                              │  │ (full wallet SPA)  │  │     │
│                              │  └───────┬─────────────┘  │     │
│                              │          │ API proxy       │     │
│                              │  ┌───────▼─────────────┐  │     │
│                              │  │ /api/* → sigil-node │  │     │
│                              │  │         :8181       │  │     │
│                              │  └─────────────────────┘  │     │
│                              └──────────────────────────┘     │
│                                        │                      │
│                                        ▼                      │
│                              ┌──────────────────────────┐     │
│                              │  sigil-node (:8181)      │     │
│                              │  (local chain node)      │     │
│                              └──────────────────────────┘     │
│                                        │                      │
│                                        │ fallback if down     │
│                                        ▼                      │
│                              ┌──────────────────────────┐     │
│                              │  fluxapp.xyz/api         │     │
│                              │  (remote fallback)       │     │
│                              └──────────────────────────┘     │
└──────────────────────────────────────────────────────────────┘
```

## 2. How flux:// Works

The `flux://` protocol is a **registered URI scheme handler** on the OS level:

1. A `.desktop` file is installed at `~/.local/share/applications/sigil-flux-wallet.desktop`
2. It registers `x-scheme-handler/flux` MIME type
3. When any app opens `flux://wallet`, the OS calls:
   ```
   xdg-open https://localhost:8443/wallet/
   ```
4. The browser opens the local wallet — zero latency, no internet

**Install (one-time):**
```bash
cd sigil/gui/flux-local-wallet
./install.sh
```

**Start the wallet server:**
```bash
cd sigil/gui/flux-local-wallet
node flux-local-wallet.mjs
# Or as daemon:
nohup node flux-local-wallet.mjs &
```

## 3. Files

| File | Purpose |
|------|---------|
| `sigil/gui/flux-local-wallet/flux-local-wallet.mjs` | Main server (HTTPS :8443 + API proxy) |
| `sigil/gui/flux-local-wallet/install.sh` | Installs flux:// protocol handler |
| `/root/sigilgraph-login/dist/` | Login page (served at `/`) |
| `sigil/gui/sigil-wallet/dist/` | Wallet SPA (served at `/wallet/`) |
| `~/.flux/certs/localhost-key.pem` | TLS key (auto-generated) |
| `~/.flux/certs/localhost-cert.pem` | TLS cert (auto-generated) |

## 4. Future SIGIL Improvements

### 4.1 Native flux:// Protocol (No Browser Needed)
Currently `flux://` opens the browser. The next step:
- Build a **native GTK/Qt window** that embeds the wallet WebView
- `flux://wallet` opens a native window, not a browser tab
- Uses the same localhost server as backend
- Ships as a single binary via `fluxc compile-native`

### 4.2 flux:// Deep Links
```
flux://wallet/send?to=sgl1abc...&amount=100    → pre-fills send form
flux://wallet/swap?from=SGL&to=QUG&amount=50   → pre-fills DEX swap
flux://sigil-top/miner/start?threads=16         → starts miner
flux://explorer/block/12345                     → opens specific block
flux://explorer/tx/0xabcdef...                  → opens specific transaction
```

### 4.3 P2P Wallet Sync
Instead of proxying to a single sigil-node, use libp2p gossipsub:
- Wallet discovers local peers via mDNS
- Falls back to DHT bootstrap if no local peer
- Full P2P: no central node needed

### 4.4 SQIsign-Pinned Wallet Sessions
- Wallet session token signed with SQIsign (177B PQ signature)
- Reconnect without re-entering mnemonic
- Session persists across browser restarts
- Bound to the machine's TPM if available

### 4.5 Multi-Node Dashboard
For operators running multiple nodes (Epsilon + Delta + Beta):
- `flux://fleet` → shows all nodes, health, blocks, balances
- One wallet manages all nodes
- Cross-node balance aggregation

### 4.6 Flux Platform Integration
- `fluxc wallet start` → starts the local wallet server
- `fluxc wallet stop` → stops it
- `fluxc wallet status` → bridge-status JSON
- `fluxc wallet deploy` → rebuilds + deploys wallet UI
- `flux_ui_list` shows the wallet deployment

## 5. The Rocky Surprise Summary

Rocky comes back tomorrow to:

### Already Built
- ✅ `flux://` + `https://` local wallet server — running, tested
- ✅ `FLUX_ENHANCEMENT_PLAN.md` — 10-section roadmap
- ✅ `flux-platform-dev` skill — teaches agents to use Flux tools
- ✅ `flux_wallet_xray` analysis — 168 files, 138K LOC scanned
- ✅ `flux_wallet_components` — ranked component list
- ✅ Disk freed: 100% → 73% (10GB+ recovered)

### Ready to Demo
1. Open browser, type `flux://wallet` → SIGIL wallet appears
2. Login with mnemonic → full wallet + explorer + DEX
3. API calls proxied to local sigil-node
4. Everything works offline

### Tomorrow Night Sprint
1. `flux context pack --semantic` (smart 1M packing)
2. Fix `p2p-worker` event loop
3. `flux mcp scaffold` (auto-MCP tools)
4. `fluxc compile --ai-fix` (self-healing compiler)

---

> *"Type `flux://wallet` and the SIGIL appears. That's the kind of magic Rocky lives for."*
