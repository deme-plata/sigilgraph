/**
 * SIGIL Tron Wallet — standalone js-libp2p entry (v0.9.1, 2026-06-09)
 *
 * The tron wallet (sigil-wallet-tron.html) is a hand-written static page that was
 * HTTP-only (fetch /api/v1/*) with NO peer-to-peer. This module bundles the SAME
 * libp2p stack the React SPA uses — including the newly re-enabled WebRTC transport
 * — into one ESM file the static page can load via <script type="module">.
 *
 * createBrowserNode() already: dials the bootstrap over wss, registers protocols,
 * and STARTS the browser-peer-discovery WebRTC mesh (node.ts:432-433). So here we
 * just start it, subscribe to the chain topics, and surface peer/browser-mesh
 * status to the page via window.SigilTronP2P + a 'sigil-p2p-status' CustomEvent.
 *
 * Browser-to-browser: two tron wallets meet over the Epsilon circuit relay
 * (signaling) and upgrade to a DIRECT WebRTC datachannel — Epsilon then drops out
 * of the A↔B path. ICE exposes each browser's public IP to its peer (accepted
 * tradeoff). Relay remains the fallback.
 */
import { createBrowserNode, getNodeStats, exposeDebugUtilities, stopNode } from './libp2p/node'
import { getBrowserPeerCount, getKnownBrowserPeers } from './libp2p/browserPeerDiscovery'
import { TOPICS } from './libp2p/config'
import type { Libp2p } from 'libp2p'

// LANE-AC: the bridge-relayed SIGIL mesh topics (distinct from the /qnk/*
// Quillon-era TOPICS in config.ts — the bridge only forwards /sigil/g0/*).
const SIGIL_BLOCKS = '/sigil/g0/blocks'
const SIGIL_BALANCE = '/sigil/g0/balance'

let node: Libp2p | null = null
let starting: Promise<Libp2p> | null = null

function snapshot() {
  if (!node) return { started: false, peerId: null, peers: 0, browserPeers: 0, directPeers: 0 }
  let directPeers = 0
  try {
    // A connection whose remoteAddr contains '/webrtc' is a DIRECT browser↔browser
    // datachannel; '/p2p-circuit' (without /webrtc) is relayed through Epsilon.
    for (const c of node.getConnections()) {
      const a = c.remoteAddr.toString()
      if (a.includes('/webrtc')) directPeers++
    }
  } catch { /* getConnections can throw mid-teardown */ }
  return {
    started: true,
    peerId: node.peerId.toString(),
    peers: node.getConnections().length,
    browserPeers: getBrowserPeerCount(),
    directPeers,
    knownBrowserPeers: getKnownBrowserPeers(),
  }
}

function emitStatus() {
  try {
    window.dispatchEvent(new CustomEvent('sigil-p2p-status', { detail: snapshot() }))
  } catch { /* SSR / no window */ }
}

async function start(): Promise<Libp2p> {
  if (node) return node
  if (starting) return starting
  starting = (async () => {
    console.log('🌐 [SIGIL TRON P2P] starting libp2p node (WebRTC browser-to-browser enabled)...')
    const n = await createBrowserNode() // dials bootstrap + starts WebRTC discovery mesh
    exposeDebugUtilities(n)
    try {
      const pubsub: any = (n.services as any).pubsub
      pubsub?.subscribe(TOPICS.BLOCKS)
      pubsub?.subscribe(TOPICS.TRANSACTIONS)
      pubsub?.subscribe(TOPICS.PEER_HEIGHTS)
      // LANE-AC: the SIGIL bridge relays the rust mesh on /sigil/g0/* — the
      // TOPICS above are the /qnk/* Quillon-fork legacy and never see bridge
      // traffic. Subscribe the sigil topics explicitly and re-dispatch as DOM
      // events so the page can go live-tip + live-balance without HTTP.
      pubsub?.subscribe(SIGIL_BLOCKS)
      pubsub?.subscribe(SIGIL_BALANCE)
      pubsub?.addEventListener('message', (evt: any) => {
        const t = evt?.detail?.topic
        if (t !== SIGIL_BLOCKS && t !== SIGIL_BALANCE) return
        let data: any
        try { data = JSON.parse(new TextDecoder().decode(evt.detail.data)) } catch { return }
        if (t === SIGIL_BLOCKS) {
          const h = data?.header?.height ?? data?.height
          if (h != null) window.dispatchEvent(new CustomEvent('sigil-p2p-block', { detail: { height: Number(h) } }))
        } else {
          window.dispatchEvent(new CustomEvent('sigil-p2p-balance', { detail: data }))
        }
      })
    } catch (e) { console.warn('[SIGIL TRON P2P] subscribe failed', e) }

    n.addEventListener('peer:connect', emitStatus)
    n.addEventListener('peer:disconnect', emitStatus)
    window.addEventListener('browser-mesh-peer-connected', emitStatus)
    setInterval(emitStatus, 4000)

    node = n
    emitStatus()
    console.log(`✅ [SIGIL TRON P2P] node up — peerId ${n.peerId.toString().slice(0, 16)}…`)
    return n
  })()
  return starting
}

async function stop(): Promise<void> {
  if (node) { try { await stopNode(node) } catch { /* ignore */ } node = null }
  starting = null
  emitStatus()
}

// Public surface for the tron page.
;(window as any).SigilTronP2P = {
  start,
  stop,
  node: () => node,
  status: snapshot,
  stats: () => (node ? getNodeStats(node) : null),
  peers: () => (node ? node.getConnections().length : 0),
  browserPeers: () => getBrowserPeerCount(),
  directPeers: () => snapshot().directPeers,
  knownBrowserPeers: () => getKnownBrowserPeers(),
}

// Auto-start on load (non-blocking; failures are logged, page still works HTTP-only).
start().catch((e) => console.error('[SIGIL TRON P2P] start failed', e))

export { start, stop, snapshot }
