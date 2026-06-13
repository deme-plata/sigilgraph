// SIGIL js-libp2p ↔ flux-p2p BRIDGE
// TCP side: dial the rust flux-p2p node (which can't be reached from a browser).
// WS side : accept browser js-libp2p clients.
// gossipsub auto-forwards /sigil/g0/blocks between the two meshes.
//
// Proves cross-implementation interop (rust-libp2p ↔ js-libp2p) and upgrades
// sigil-live.html from feed-polling to true gossipsub.

import { createLibp2p } from 'libp2p'
import { tcp } from '@libp2p/tcp'
import { webSockets } from '@libp2p/websockets'
import { noise } from '@chainsafe/libp2p-noise'
import { yamux } from '@chainsafe/libp2p-yamux'
import { gossipsub } from '@chainsafe/libp2p-gossipsub'
import { identify } from '@libp2p/identify'
import { multiaddr } from '@multiformats/multiaddr'
import { createEd25519PeerId, createFromProtobuf, exportToProtobuf } from '@libp2p/peer-id-factory'
import { readFileSync, writeFileSync } from 'node:fs'
import { initPq } from './pq.mjs' // LANE-AD: ML-KEM-1024 hybrid session layer (classical noise kept)

const RUST_NODE = process.env.SIGIL_RUST_NODE
  || '/ip4/127.0.0.1/tcp/9501/p2p/12D3KooWFi1Rpk14GCcmT9kES9Fys7tkEgZc6GbqLv2dcaTWYYp9'
const WS_PORT = process.env.SIGIL_BRIDGE_WS_PORT || '9610'
const TOPIC = '/sigil/g0/blocks'

// LANE-AB: PERSIST the bridge identity so /p2p/<id> is STABLE across restarts. It used to be
// random every start, so any wallet that pinned the bridge peer-id (or a stale announce file)
// could never complete the WSS handshake. Load the saved Ed25519 key; mint + save it once.
const KEY_FILE = process.env.SIGIL_BRIDGE_KEY || '/home/orobit/sigil-bridge/bridge-peer.proto'
let bridgePeerId
try {
  bridgePeerId = await createFromProtobuf(readFileSync(KEY_FILE))
  console.log('🔑 bridge identity LOADED (stable) from', KEY_FILE)
} catch {
  bridgePeerId = await createEd25519PeerId()
  writeFileSync(KEY_FILE, exportToProtobuf(bridgePeerId))
  console.log('🔑 bridge identity MINTED + saved →', KEY_FILE)
}

const node = await createLibp2p({
  peerId: bridgePeerId,
  addresses: { listen: [`/ip4/0.0.0.0/tcp/${WS_PORT}/ws`] },
  transports: [tcp(), webSockets()],
  connectionEncryption: [noise()],
  streamMuxers: [yamux()],
  services: {
    identify: identify(),
    // MUST match flux-p2p: ValidationMode::Strict + Signed messages.
    pubsub: gossipsub({
      globalSignaturePolicy: 'StrictSign',
      allowPublishToZeroTopicPeers: true,
      fallbackToFloodsub: true,
    }),
  },
})

await node.start()
await initPq(node) // LANE-AD: register /sigil/g0/pq-hybrid responder + publish bridge-pq.json
console.log('🌉 bridge peer:', node.peerId.toString())
console.log('🌉 WS listen :', node.getMultiaddrs().map(m => m.toString()).join('  '))

// LANE-AB: publish the current peer-id + dialable addrs so the browser wallet can build a
// host-relative dial (wss://<location.hostname>:9443/p2p/<peer>). Write to BOTH web roots so
// quillon.xyz (dist-final) AND sigilgraph.fluxapp.xyz (dist-fluxapp) serve the SAME current id.
try {
  const peer = node.peerId.toString()
  const pub = node.getMultiaddrs().map(m => m.toString()).find(a => a.includes('89.149.241.126'))
    || node.getMultiaddrs().map(m => m.toString())[0]
  // wss is TLS-terminated by q-flux :9443 → the bridge's :9610/ws (same peer-id). The wallet
  // ignores the dns name and uses location.hostname, so this works on either domain.
  const wss = `/dns4/quillon.xyz/tcp/9443/wss/p2p/${peer}`
  const payload = JSON.stringify({ addr: pub, wss, peer, wssPort: 9443, ts: Date.now() })
  for (const root of ['/home/orobit/q-narwhalknight/dist-final', '/home/orobit/q-narwhalknight/dist-fluxapp']) {
    try { writeFileSync(`${root}/bridge-addr.json`, payload) }
    catch (e) { console.log('⚠ announce write', root, '→', e?.message || e) }
  }
  console.log('🌉 published bridge addr (both roots) → peer', peer)
} catch (e) { console.log('⚠ addr publish failed:', e?.message || e) }

node.services.pubsub.subscribe(TOPIC)

let blocks = 0
node.services.pubsub.addEventListener('message', (evt) => {
  if (evt.detail.topic !== TOPIC) return
  blocks++
  try {
    const blk = JSON.parse(new TextDecoder().decode(evt.detail.data))
    if (blocks % 25 === 0 || blocks <= 3) {
      console.log(`🌉 RELAYED block #${blocks} H=${blk.header?.height ?? '?'} (rust flux-p2p → js gossipsub → browser peers)`)
    }
  } catch { /* non-block payload */ }
})

node.addEventListener('peer:connect', (e) => console.log('🔗 connected:', e.detail.toString().slice(0, 16), '· peers=', node.getPeers().length))

// dial the rust flux-p2p node over TCP (the interop link)
async function dialRust() {
  try {
    console.log('📡 dialing rust flux-p2p node over TCP:', RUST_NODE)
    await node.dial(multiaddr(RUST_NODE))
    console.log('✅ INTEROP: js-libp2p connected to rust flux-p2p ✓')
  } catch (e) {
    console.log('⚠ dial failed:', e?.message || e, '— retrying in 5s')
    setTimeout(dialRust, 5000)
  }
}
await dialRust()

setInterval(() => {
  console.log(`📊 bridge: peers=${node.getPeers().length} · blocks-relayed=${blocks} · topic-peers=${node.services.pubsub.getSubscribers(TOPIC).length}`)
}, 15000)

// ── LANE-AC: balance over P2P ────────────────────────────────────────────────
// The gossiped spine blocks are NOT the rpcd money chain — balance changes
// (coinbase/swap) originate in sigil-rpcd. The bridge polls rpcd's tx feed
// (1 server-side poll, however many browsers) and fans the deltas out on
// /sigil/g0/balance, so the browser's balance signal inherits the libp2p
// channel encryption (the LANE-AD point) instead of each wallet HTTP-polling.
const BAL_TOPIC = '/sigil/g0/balance'
const RPCD = process.env.SIGIL_RPCD_URL || 'http://127.0.0.1:8099'
node.services.pubsub.subscribe(BAL_TOPIC)
const seenCids = new Set()   // dedup by flux-history content-address
let balPublished = 0
let balPrimed = false        // first poll only PRIMES the seen-set (no replay flood)
async function pollBalanceEvents() {
  try {
    const r = await fetch(`${RPCD}/api/v1/recent?kind=tx&limit=12`)
    const j = await r.json()
    const evts = (j.results || []).filter(e => e.cid && !seenCids.has(e.cid))
    for (const e of evts) {
      seenCids.add(e.cid)
      if (seenCids.size > 500) { const it = seenCids.values(); seenCids.delete(it.next().value) }
    }
    if (!balPrimed) { balPrimed = true; return }
    for (const e of evts.sort((a, b) => a.ts - b.ts)) {
      // rpcd ingest titles: "dual-lane block #H reward R (halving)" /
      // "mine reward R SIGIL" / "swap A in → B out". prod = the affected wallet.
      const m = (e.title || '').match(/reward (\d+)/)
      const payload = {
        v: 1, kind: e.kind, wallet: (e.prod || '').toLowerCase(),
        delta: m ? m[1] : null, coinbase: e.kind === 'mine',
        height: e.h || 0, ts: e.ts, title: e.title || '',
      }
      await node.services.pubsub.publish(BAL_TOPIC, new TextEncoder().encode(JSON.stringify(payload)))
      balPublished++
      console.log(`💰 balance-delta → ${BAL_TOPIC}: ${payload.kind} ${payload.wallet.slice(0, 8)}… +${payload.delta ?? '?'} (#${balPublished})`)
    }
  } catch { /* rpcd briefly down — next tick retries */ }
}
setInterval(pollBalanceEvents, 1000)
