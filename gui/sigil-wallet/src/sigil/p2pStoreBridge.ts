// SIGIL P2P → store bridge.
//
// "Update the zustand store with js-libp2p data, not through HTTP."
//
// The browser libp2p node (libp2p/node.ts) already SUBSCRIBES to TOPICS.BLOCKS /
// TRANSACTIONS, but nothing listened to the incoming gossip — only the HTTP
// apiShim snapshot fed the store. This bridge listens to the gossipsub `message`
// events and routes them straight into useSigilStore (status height + peers +
// recent_blocks + activity). No fetch, no polling — pure P2P.
//
// Wire-up: LibP2PProvider calls attachSigilP2PBridge(node) right after the node is
// created; the returned function detaches it on teardown.

import type { Libp2p } from 'libp2p'
import { toString as uint8ArrayToString } from 'uint8arrays/to-string'
import { TOPICS } from '../libp2p/config'
import { useSigilStore, type SigilBlock, type SigilStatus } from './store'

const MAX_RECENT = 12

function pubsubOf(node: any): any {
  return node?.services?.pubsub ?? null
}

function num(v: any, d = 0): number {
  const n = Number(v)
  return Number.isFinite(n) ? n : d
}

// Map a gossiped block (JSON, field names vary by node version) → SigilBlock.
function mapBlock(b: any): SigilBlock | null {
  if (!b || typeof b !== 'object') return null
  const height = num(b.height ?? b.block_height ?? b.number, NaN)
  if (!Number.isFinite(height)) return null
  const r = b.state_roots ?? b.roots ?? {}
  return {
    height,
    hash: String(b.hash ?? b.block_hash ?? b.id ?? ''),
    parent_hash: String(b.parent_hash ?? b.parent ?? b.prev_hash ?? ''),
    timestamp_ms: num(b.timestamp_ms ?? b.timestamp ?? b.time, Date.now()),
    tx_count: num(b.tx_count ?? (Array.isArray(b.transactions) ? b.transactions.length : 0)),
    miner: String(b.miner ?? b.coinbase ?? b.proposer ?? ''),
    state_roots: {
      wallet: String(b.wallet_state_root ?? r.wallet ?? ''),
      dex: String(b.dex_state_root ?? r.dex ?? ''),
      event_log: String(b.event_log_root ?? r.event_log ?? r.event ?? ''),
      contract: String(b.contract_state_root ?? r.contract ?? ''),
    },
  }
}

let attached = false

export function attachSigilP2PBridge(node: Libp2p): () => void {
  if (attached) return () => {}
  attached = true
  const get = useSigilStore.getState
  const pubsub = pubsubOf(node)

  const peerCount = () => {
    try { return node.getPeers().length } catch { return 0 }
  }

  // Patch status with current peer count (+ optional new height) without dropping
  // the other fields. Creates a neutral status the first time.
  const patchStatus = (height?: number) => {
    const cur = get().status
    const base: SigilStatus = cur ?? {
      network_id: 'sigil-g0', height: 0, peers: 0,
      symbol: 'SIGIL', version: 'p2p', note: 'live via js-libp2p', block_time_ms: 0,
    }
    get().setStatus({
      ...base,
      peers: peerCount(),
      height: height != null ? Math.max(height, base.height) : base.height,
    })
  }

  const onMessage = (evt: any) => {
    const msg = evt?.detail
    if (!msg || msg.topic !== TOPICS.BLOCKS || !msg.data) return
    let parsed: any
    try { parsed = JSON.parse(uint8ArrayToString(msg.data)) } catch { return }
    // payload may be a bare block, {block:…}, or {blocks:[…]}
    const raw = Array.isArray(parsed?.blocks) ? parsed.blocks
      : parsed?.block ? [parsed.block]
      : [parsed]
    const mapped = raw.map(mapBlock).filter(Boolean) as SigilBlock[]
    if (!mapped.length) return

    const s = get()
    // newest-first, dedup by height, capped
    const merged = [...mapped, ...s.recent_blocks].sort((a, b) => b.height - a.height)
    const seen = new Set<number>()
    const recent: SigilBlock[] = []
    for (const b of merged) {
      if (seen.has(b.height)) continue
      seen.add(b.height)
      recent.push(b)
      if (recent.length >= MAX_RECENT) break
    }
    s.setBlocks(recent)

    const tip = recent[0]
    patchStatus(tip.height)
    s.pushActivity({
      id: `blk-${tip.height}`,
      type: 'block',
      title: `Block ${tip.height.toLocaleString()}`,
      detail: tip.miner ? `miner ${tip.miner.slice(0, 10)}…` : (tip.hash.slice(0, 16) || 'new tip'),
      hash: tip.hash,
      height: tip.height,
      timestamp_ms: tip.timestamp_ms || Date.now(),
    })
  }

  pubsub?.addEventListener?.('message', onMessage)
  node.addEventListener('peer:connect', () => patchStatus())
  node.addEventListener('peer:disconnect', () => patchStatus())

  // mark "store is alive via P2P" + seed an initial status so the UI shows peers
  // immediately (height fills in on the first gossiped block).
  get().markShimInstalled()
  patchStatus()

  return () => {
    try { pubsub?.removeEventListener?.('message', onMessage) } catch {}
    attached = false
  }
}
