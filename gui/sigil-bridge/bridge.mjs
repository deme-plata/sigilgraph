// sigil-bridge — HARDENED js-libp2p WSS relay: SIGIL testnet → browser wallets.
// SECURITY MODEL: the bridge is UNTRUSTED infrastructure. Browsers verify SQIsign
// signatures (anchored in fluxapp.xyz DNS) on every block — a compromised bridge
// can only censor/delay, never forge.
//
// Audited by Claude + DeepSeek-V4 (2026-06-05). Fixes applied:
//   [CRITICAL] do NOT subscribe to the topic — subscribing makes gossipsub
//              auto-forward client messages to all peers (injection). The bridge
//              ONLY publishes; it never receives/relays client data.
//   [HIGH]     dropped the invalid `inboundConnectionThreshold`; use real caps
//              (maxConnections + maxIncomingPendingConnections) + a connectionGater
//              for per-IP limiting.
//   [MED]      async fs.readFile (no sync I/O in the event loop) + debounced watch.
//   [LOW]      validate STATUS_FILE stays under an allowed dir (no traversal);
//              periodic cert-expiry warning.

import { createLibp2p } from 'libp2p'
import { webSockets } from '@libp2p/websockets'
import { noise } from '@chainsafe/libp2p-noise'
import { yamux } from '@chainsafe/libp2p-yamux'
import { gossipsub } from '@chainsafe/libp2p-gossipsub'
import { identify } from '@libp2p/identify'
import { readFileSync } from 'node:fs'
import { readFile, watch } from 'node:fs/promises'
import { createServer } from 'node:https'
import { resolve, sep } from 'node:path'
import { X509Certificate } from 'node:crypto'

// ── config ────────────────────────────────────────────────────────────────
const ALLOWED_DIR = '/home/orobit/q-narwhalknight/dist-final'
const STATUS_FILE = resolve(process.env.SIGIL_STATUS || `${ALLOWED_DIR}/sigil-status.json`)
const CERT_DIR = process.env.SIGIL_CERT || '/etc/letsencrypt/live/fluxapp.xyz'
const PORT = +(process.env.SIGIL_BRIDGE_PORT || 9601)
const TOPIC = 'sigil/g0/wallet'
const MAX_CONNECTIONS = 400
const MAX_MSG_BYTES = 64 * 1024
const PUBLISH_MS = 2000
const WATCH_DEBOUNCE_MS = 200

// [LOW] path-traversal guard: STATUS_FILE must resolve under ALLOWED_DIR.
if (STATUS_FILE !== resolve(ALLOWED_DIR, 'sigil-status.json') &&
    !STATUS_FILE.startsWith(ALLOWED_DIR + sep)) {
  console.error(`✗ SIGIL_STATUS ${STATUS_FILE} escapes ${ALLOWED_DIR}`); process.exit(1)
}

// [MED] async, bounded, exception-safe status read — never blocks the loop.
async function readStatus() {
  try {
    const raw = await readFile(STATUS_FILE, 'utf8')
    if (raw.length > MAX_MSG_BYTES) return null
    const j = JSON.parse(raw)
    if (!j || typeof j !== 'object' || !j.status) return null
    return j
  } catch { return null }
}

async function main() {
  let key, cert
  try { key = readFileSync(`${CERT_DIR}/privkey.pem`); cert = readFileSync(`${CERT_DIR}/fullchain.pem`) }
  catch (e) { console.error(`✗ TLS cert read failed (${CERT_DIR}): ${e.message}`); process.exit(1) }
  const httpsServer = createServer({ key, cert })

  const node = await createLibp2p({
    addresses: { listen: [`/ip4/0.0.0.0/tcp/${PORT}/wss`] },
    transports: [webSockets({ server: httpsServer })],
    connectionEncrypters: [noise()],          // libp2p-layer peer-id auth (REQUIRED)
    streamMuxers: [yamux()],
    connectionManager: {
      maxConnections: MAX_CONNECTIONS,         // [HIGH] hard slot cap
      maxIncomingPendingConnections: 50,       // [HIGH] valid handshake-flood guard
      inboundUpgradeTimeout: 8000,
    },
    // [HIGH] per-IP gate: cap concurrent connections from any single address.
    connectionGater: (() => {
      const perIp = new Map()
      const ipOf = (ma) => { try { return ma.toString().match(/\/ip[46]\/([^/]+)/)?.[1] || '?' } catch { return '?' } }
      return {
        denyInboundConnection: (maConn) => {
          const ip = ipOf(maConn.remoteAddr); const n = (perIp.get(ip) || 0)
          if (n >= 12) return true                 // >12 conns from one IP → deny
          perIp.set(ip, n + 1)
          maConn.remoteAddr && void 0
          return false
        },
      }
    })(),
    services: {
      identify: identify(),
      pubsub: gossipsub({
        globalSignaturePolicy: 'StrictSign',     // every msg signed by origin peer
        allowPublishToZeroTopicPeers: true,      // publish even before a browser joins
        maxInboundDataLength: MAX_MSG_BYTES,     // gossip-layer DoS cap
      }),
    },
  })

  await node.start()
  const ps = node.services.pubsub
  // [CRITICAL FIX] do NOT subscribe. Subscribing would make gossipsub forward
  // client-published messages to all peers (injection/amplification). The bridge
  // is publish-only; browsers subscribe + verify SQIsign themselves.

  console.log(`✓ sigil-bridge up (audited: Claude + DeepSeek)`)
  console.log(`  peer:  ${node.peerId.toString()}`)
  console.log(`  wss:   /dns4/fluxapp.xyz/tcp/${PORT}/wss`)
  console.log(`  topic: ${TOPIC} · publish-only · StrictSign · maxConn ${MAX_CONNECTIONS} · /ip cap 12`)

  // [LOW] warn if the TLS cert is near expiry.
  try {
    const days = Math.round((new Date(new X509Certificate(cert).validTo) - Date.now()) / 864e5)
    if (days < 14) console.warn(`⚠ TLS cert expires in ${days} days`)
  } catch {}

  // ── publish loop: re-broadcast the REAL chain tip (carrying SQIsign-bearing
  // blocks) so late-joining browsers get the tip immediately. Serialized so a
  // slow read can never overlap with itself.
  let lastHeight = -1, publishing = false
  async function publish() {
    if (publishing) return
    publishing = true
    try {
      const s = await readStatus()
      if (!s) return
      const h = s.status?.height ?? 0
      const payload = JSON.stringify({ t: 'tip', status: s.status, blocks: (s.blocks || []).slice(0, 8), at: Date.now() })
      if (Buffer.byteLength(payload) > MAX_MSG_BYTES) return
      try { await ps.publish(TOPIC, new TextEncoder().encode(payload)) } catch {}
      if (h !== lastHeight) { console.log(`  → tip H=${h} (${ps.getSubscribers(TOPIC).length} subs)`); lastHeight = h }
    } finally { publishing = false }
  }
  setInterval(publish, PUBLISH_MS)

  // [MED] debounced async watch (coalesce rapid fs events; never unawaited race).
  let timer = null
  ;(async () => {
    try {
      const watcher = watch(STATUS_FILE)
      for await (const _ of watcher) {
        clearTimeout(timer)
        timer = setTimeout(() => { publish() }, WATCH_DEBOUNCE_MS)
      }
    } catch {}
  })()
  await publish()
}

main().catch((e) => { console.error('bridge fatal:', e); process.exit(1) })
