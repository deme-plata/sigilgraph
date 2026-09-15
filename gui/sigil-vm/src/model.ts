// model.ts — turns the node's raw answers into what an OpenSea-shaped page
// needs: collections, drops, movers, sales, tokens. The rule is the site's
// rule: a number is either LIVE (read from the node this poll), DERIVED (a
// calculation over live numbers, stated), or PRETEND (illustrative because
// no chain source exists yet — and the card says so with a chip). Nothing on
// the page pretends to be measured when it is not.

import { api, fmt, glyphsToSigil, hex, GLYPHS_PER_SIGIL, NATIVE_TOKEN } from './api'
import type { Miners, Anchor, Docket, Nation, Usds, Rocky, Gauge, Bridge, Cert, Recent, Pool, Supply, HashPoint, Earth } from './api'
import { coverSvg, tokenIconSvg, type Glyph } from './art'

export type Provenance = 'live' | 'derived' | 'pretend'

export interface Collection {
  id: string
  name: string
  glyph: Glyph
  verified: boolean
  cover: string
  blurb: string
  items: number | null
  owners: number | null
  floor: string        // human string, e.g. "1000 glyphs" / "—"
  floorChange: number | null
  volume: string
  volumeChange: number | null
  sales: number | null
  provenance: Provenance
  link: string
}

export interface Drop {
  id: string
  name: string
  glyph: Glyph
  cover: string
  status: 'live' | 'minting' | 'upcoming' | 'blocked'
  when: string          // "live since block 6,400,000" / "in ~2.1 h (81,418 blocks)"
  detail: string
  provenance: Provenance
  link: string
  progress: number | null // 0..1 for countdowns
}

export interface Mover {
  id: string
  name: string
  sub: string
  cover: string
  value: string
  change: number | null
  provenance: Provenance
  series?: number[]
}

export interface Sale {
  id: string
  name: string
  collection: string
  cover: string
  price: string
  when: string
  proof: string  // tx hash / leaf
  provenance: Provenance
}

export interface Token {
  id: string       // 64-hex token id
  symbol: string
  name: string
  decimals: number
  icon: string
  status: 'live' | 'gated' | 'dormant' | 'external'
  statusNote: string
  supply: number | null   // in whole units
  maxSupply: number | null
  holders: number | null
  ageBlocks: number | null
  price: number | null    // USD; null = no oracle
  change1h: number | null
  change24h: number | null
  change7d: number | null
  volume24h: number | null
  liquidity: number | null
  provenance: Provenance
  tags: string[]
}

export interface ChainHead {
  height: number
  blkPerSec: number | null
  netHps: number
  liveMiners: number
  finalityGate: string
  finalityHeight: number
  committee: number
  supplySigil: number
  maxSupplySigil: number
  mintedPct: number
  valueLocked: number
  notes: number
  nullifiers: number
  registered: number
  capacity: number
  treasurySigil: number
  ok: boolean
  lagBlocks: number
  hashChange: number | null
}

export interface Snapshot {
  at: number
  head: ChainHead
  collections: Collection[]
  featured: Collection[]
  drops: Drop[]
  movers: Mover[]
  sales: Sale[]
  tokens: Token[]
  pools: Pool[]
  miners: Miners | null
  docket: Docket | null
  recent: Recent | null
  earth: Earth | null
  rocky: Rocky | null
  usds: Usds | null
  offline: boolean
  hashHist: number[]
}

// ── rate memory: block rate + per-miner deltas survive reloads ──────────────
interface Mem { height?: number; at?: number; miners?: Record<string, { hps: number; at: number }>; netHps?: { hps: number; at: number }[]; series?: Record<string, number[]> }
const MEM_KEY = 'sigilvm-mem-v1'
function loadMem(): Mem { try { return JSON.parse(localStorage.getItem(MEM_KEY) || '{}') } catch { return {} } }
function saveMem(m: Mem) { try { localStorage.setItem(MEM_KEY, JSON.stringify(m)) } catch { /* ignore */ } }

let lastSnapshot: Snapshot | null = null
export const getLast = () => lastSnapshot

export async function buildSnapshot(): Promise<Snapshot> {
  const at = Date.now()
  const [supply, miners, anchor, docket, nation, usds, rocky, gauge, bridge, cert, recent, pools, hist, earth] = await Promise.all([
    api.supply(), api.miners(), api.anchor(), api.docket(), api.nation(), api.usds(), api.rocky(), api.gauge(),
    api.bridge(), api.cert(), api.recent(), api.pools(), api.hashHistory(), api.earth(),
  ])
  const offline = !supply && !miners && !anchor
  const mem = loadMem()

  const height = miners?.height ?? nation?.height ?? usds?.height ?? 0
  let blkPerSec: number | null = null
  if (mem.height && mem.at && height > mem.height && at > mem.at + 5000) {
    blkPerSec = (height - mem.height) / ((at - mem.at) / 1000)
  }
  if (height) { mem.height = height; mem.at = at }

  const hashHist = (hist?.history ?? []).map((p) => p.hashrate)
  const head = buildHead(supply, miners, anchor, nation, cert, blkPerSec, !offline, hashHist)
  const tokens = buildTokens(supply, usds, rocky, anchor, height, pools ?? [])
  const collections = buildCollections(miners, anchor, docket, recent, earth, nation, rocky, bridge, hist ?? null)
  const drops = buildDrops(usds, rocky, gauge, height, blkPerSec, bridge)
  const movers = buildMovers(miners, mem, at, hist ?? null)
  const sales = buildSales(earth, docket, usds, recent, miners)
  saveMem(mem)

  const featured = [...collections].sort((a, b) => (b.items ?? 0) - (a.items ?? 0)).slice(0, 6)
  lastSnapshot = { at, head, collections, featured, drops, movers, sales, tokens, pools: pools ?? [], miners, docket, recent, earth, rocky, usds, offline, hashHist }
  return lastSnapshot
}

function buildHead(supply: Supply | null, miners: Miners | null, anchor: Anchor | null, nation: Nation | null, cert: Cert | null, blkPerSec: number | null, ok: boolean, hashHist: number[] = []): ChainHead {
  const hashChange = hashHist.length > 1 && hashHist[0] > 0 ? ((hashHist[hashHist.length - 1] - hashHist[0]) / hashHist[0]) * 100 : null
  return {
    hashChange,
    height: miners?.height ?? nation?.height ?? cert?.certificate.height ?? 0,
    blkPerSec,
    netHps: miners?.net_hps ?? 0,
    liveMiners: miners?.live_miners ?? 0,
    finalityGate: cert?.certificate.gate ?? '—',
    finalityHeight: cert?.certificate.height ?? 0,
    committee: cert?.certificate.committee_size ?? 0,
    supplySigil: glyphsToSigil(supply?.native_supply),
    maxSupplySigil: glyphsToSigil(supply?.max_supply),
    mintedPct: supply?.minted_pct ?? 0,
    valueLocked: glyphsToSigil(anchor?.value_locked),
    notes: anchor?.notes ?? 0,
    nullifiers: anchor?.nullifiers ?? 0,
    registered: anchor?.registered ?? 0,
    capacity: anchor?.capacity ?? 0,
    treasurySigil: glyphsToSigil(nation?.treasury_glyphs),
    ok,
    lagBlocks: Math.max(0, (miners?.height ?? nation?.height ?? 0) - (cert?.certificate.height ?? 0)),
  }
}

function buildTokens(supply: Supply | null, usds: Usds | null, rocky: Rocky | null, anchor: Anchor | null, height: number, pools: Pool[]): Token[] {
  const liq = (tokenId: string): number | null => {
    const p = pools.filter((x) => [x.token0, x.token1, x.token_a, x.token_b].includes(tokenId))
    if (!p.length) return null
    return p.reduce((s, x) => s + glyphsToSigil(x.reserve0 ?? x.reserve_a) + glyphsToSigil(x.reserve1 ?? x.reserve_b), 0)
  }
  const t: Token[] = []
  t.push({
    id: NATIVE_TOKEN, symbol: 'SIGIL', name: 'SIGIL (native)', decimals: 10, icon: tokenIconSvg('SIGIL'),
    status: 'live', statusNote: 'native coin · 1 SIGIL = 10¹⁰ glyphs',
    supply: glyphsToSigil(supply?.native_supply), maxSupply: glyphsToSigil(supply?.max_supply),
    holders: anchor?.registered ?? null, ageBlocks: height || null,
    price: null, change1h: null, change24h: null, change7d: null, volume24h: null, liquidity: liq(NATIVE_TOKEN),
    provenance: supply ? 'live' : 'pretend', tags: ['NATIVE'],
  })
  if (usds) t.push({
    id: usds.token_id, symbol: 'USDS', name: 'SIGIL Dollar', decimals: usds.usds_decimals, icon: tokenIconSvg('USDS'),
    status: usds.live ? 'live' : 'gated',
    statusNote: usds.live ? `live since block ${fmt.int(usds.live_height)} · fee ${usds.fee_bps} bps` : `activates at block ${fmt.int(usds.live_height)}`,
    supply: glyphsToSigil(usds.usds_supply, usds.usds_decimals), maxSupply: null, holders: null, ageBlocks: Math.max(0, height - usds.live_height),
    price: usds.price_fresh ? Number(usds.price) / 1e8 : null, change1h: null, change24h: null, change7d: null, volume24h: null, liquidity: liq(usds.token_id),
    provenance: 'live', tags: ['STABLE', usds.price_fresh ? 'ORACLE' : 'NO ORACLE'],
  })
  if (rocky) t.push({
    id: rocky.token, symbol: rocky.symbol, name: rocky.name, decimals: rocky.decimals, icon: tokenIconSvg('ROCKY'),
    status: rocky.live ? 'live' : 'gated',
    statusNote: rocky.live ? (rocky.bootstrapped ? 'live · bootstrapped' : 'live · awaiting bootstrap') : 'gated behind GAUGE_LIVE_HEIGHT',
    supply: glyphsToSigil(rocky.total_supply, rocky.decimals), maxSupply: null, holders: null, ageBlocks: null,
    price: null, change1h: null, change24h: null, change7d: null, volume24h: null, liquidity: liq(rocky.token),
    provenance: 'live', tags: ['CONTRACT', 'COVER'],
  })
  t.push({
    id: '77534947494c3300'.padEnd(64, '0'), symbol: 'wSIGIL3', name: 'Wrapped SIGIL (Polygon)', decimals: 18, icon: tokenIconSvg('wSIGIL3'),
    status: 'external', statusNote: 'ERC-20 on Polygon · Uniswap pool · bridge relayer held',
    supply: null, maxSupply: null, holders: null, ageBlocks: null,
    price: null, change1h: null, change24h: null, change7d: null, volume24h: null, liquidity: null,
    provenance: 'pretend', tags: ['BRIDGE', 'POLYGON'],
  })
  t.push({
    id: '5353484152450000'.padEnd(64, '0'), symbol: 'SSHARE', name: 'SIGIL Share', decimals: 10, icon: tokenIconSvg('SSHARE'),
    status: 'dormant', statusNote: 'native contract in sigil-vm · no API route exposes it yet',
    supply: null, maxSupply: null, holders: null, ageBlocks: null,
    price: null, change1h: null, change24h: null, change7d: null, volume24h: null, liquidity: null,
    provenance: 'pretend', tags: ['CONTRACT'],
  })
  return t
}

function buildCollections(miners: Miners | null, anchor: Anchor | null, docket: Docket | null, recent: Recent | null, earth: Earth | null, nation: Nation | null, rocky: Rocky | null, bridge: Bridge | null, hist: { history: HashPoint[] } | null): Collection[] {
  const c: Collection[] = []
  const honours = docket?.histogram?.HonourConferred ?? null
  const justices = docket?.histogram?.JusticeAppointed ?? null
  const netChange = (() => {
    const h = hist?.history
    if (!h || h.length < 2) return null
    const last = h[h.length - 1].hashrate, first = h[0].hashrate
    return first > 0 ? ((last - first) / first) * 100 : null
  })()

  c.push({
    id: 'rigs', name: "Miners' Rigs", glyph: 'rig', verified: true, cover: coverSvg("Miners' Rigs", 'rig'),
    blurb: 'Every rig submitting shares to the braid right now. Rank = hashrate share.',
    items: miners?.live_miners ?? null, owners: miners ? new Set(miners.miners.map((m) => m.wallet)).size : null,
    floor: miners ? fmt.hps(Math.min(...miners.miners.map((m) => m.hash_rate))) : '—', floorChange: null,
    volume: miners ? fmt.hps(miners.net_hps) : '—', volumeChange: netChange, sales: miners?.blocks_accepted ?? null,
    provenance: miners ? 'live' : 'pretend', link: '/sigil-explorer.html',
  })
  c.push({
    id: 'honours', name: 'Elefantordenen', glyph: 'seal', verified: true, cover: coverSvg('Elefantordenen', 'seal'),
    blurb: 'Honours conferred by the SIGIL Supreme Court. Each is a leaf in a verified docket chain.',
    items: honours, owners: honours, floor: '1 honour', floorChange: null,
    volume: docket ? `${fmt.int(docket.total)} docket entries` : '—', volumeChange: null, sales: honours,
    provenance: docket ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html#court',
  })
  c.push({
    id: 'bench', name: 'Justices of the Bench', glyph: 'shield', verified: true, cover: coverSvg('Justices of the Bench', 'shield'),
    blurb: 'Appointed justices. The bench that hears cases, rules and seals precedents.',
    items: justices, owners: justices, floor: '1 seat', floorChange: null,
    volume: docket?.chain_verified ? 'chain verified ✓' : '—', volumeChange: null, sales: justices,
    provenance: docket ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html#court',
  })
  c.push({
    id: 'blocks', name: 'DagKnight Blocks', glyph: 'block', verified: true, cover: coverSvg('DagKnight Blocks', 'block'),
    blurb: 'The braid itself. Blue blocks ordered by blue score; every one names its producer.',
    items: miners?.height ?? null, owners: recent ? new Set(recent.blocks.map((b) => hex(b.producer))).size : null,
    floor: recent && recent.blocks.length ? `blue ${fmt.int(recent.blocks[0].blue_score)}` : '—', floorChange: null,
    volume: recent ? `${recent.blocks.length} recent` : '—', volumeChange: null, sales: recent?.blocks.filter((b) => b.is_blue).length ?? null,
    provenance: recent ? 'live' : 'pretend', link: '/sigil-explorer.html',
  })
  c.push({
    id: 'notes', name: 'Shielded Notes', glyph: 'eye', verified: true, cover: coverSvg('Shielded Notes', 'eye'),
    blurb: 'Sealed 32-byte notes in the shielded pool. First tap wins; a spent note is worth zero everywhere.',
    items: anchor?.notes ?? null, owners: anchor?.registered ?? null,
    floor: anchor ? `${fmt.int(anchor.capacity - anchor.notes)} free` : '—', floorChange: null,
    volume: anchor ? `${fmt.num(glyphsToSigil(anchor.value_locked))} SIGIL locked` : '—', volumeChange: null, sales: anchor?.nullifiers ?? null,
    provenance: anchor ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html',
  })
  const attest = earth?.attest_last?.anchor
  c.push({
    id: 'earth', name: 'Kristensen Earth K⊕', glyph: 'earth', verified: true, cover: coverSvg('Kristensen Earth', 'earth'),
    blurb: 'Earth-rotation attestations anchored on SIGIL from a wallet with a published viewing key.',
    items: earth ? (earth.row ?? (attest?.memo && /:(\d+):/.test(attest.memo) ? Number(RegExp.$1) : null)) : null, owners: 1,
    floor: attest?.amount ? `${attest.amount} glyphs` : '—', floorChange: null,
    volume: attest?.tx_hash ? `anchored ${fmt.short(attest.tx_hash, 6)}` : '—', volumeChange: null, sales: attest?.executed ? 1 : 0,
    provenance: earth ? 'live' : 'pretend', link: '/kristensen-earth.html',
  })
  c.push({
    id: 'coins', name: 'SIGIL Coins (NFC)', glyph: 'coin', verified: false, cover: coverSvg('SIGIL Coins', 'coin', { symbol: 'NTAG215' }),
    blurb: 'Physical coins: one shielded note on a 504-byte tag. No batch anchored yet — this collection is empty on chain.',
    items: 0, owners: 0, floor: '—', floorChange: null, volume: '0 batches anchored', volumeChange: null, sales: 0,
    provenance: 'live', link: '/sigil-wallet-tron-embedded.html',
  })
  c.push({
    id: 'treasury', name: 'Nation Treasury', glyph: 'token', verified: true, cover: coverSvg('Nation Treasury', 'token'),
    blurb: 'The welfare treasury financed by the mining dev-fee carve, paid out in USDS.',
    items: 1, owners: 1, floor: nation ? `${fmt.num(glyphsToSigil(nation.treasury_glyphs))} SIGIL` : '—', floorChange: null,
    volume: nation ? `${nation.welfare_bps} bps carve` : '—', volumeChange: null, sales: null,
    provenance: nation ? 'live' : 'pretend', link: '/sigil-nation-whitepaper.html',
  })
  c.push({
    id: 'rocky', name: rocky?.name ?? 'Rocky K⊕ Cover', glyph: 'shield', verified: true, cover: coverSvg('Rocky Cover', 'shield', { symbol: 'ROCKY' }),
    blurb: 'Cover policies underwritten by the ROCKY pool against K⊕ p99 excursions. First native contract that executes.',
    items: rocky?.policies.length ?? null, owners: null, floor: rocky ? `${rocky.fee_bps} bps fee` : '—', floorChange: null,
    volume: rocky ? `${fmt.num(glyphsToSigil(rocky.pool_balance, rocky.decimals))} ROCKY pooled` : '—', volumeChange: null, sales: rocky?.policies.length ?? null,
    provenance: rocky ? 'live' : 'pretend', link: '/kristensen-board.html',
  })
  c.push({
    id: 'bridge', name: 'Bridge Locks', glyph: 'braid', verified: true, cover: coverSvg('Bridge Locks', 'braid'),
    blurb: 'SIGIL locked for the Polygon leg. Relayer held; locks are real, mints are not yet.',
    items: bridge?.lock_count ?? null, owners: null, floor: bridge ? `${fmt.num(glyphsToSigil(bridge.vault_balance))} SIGIL vault` : '—', floorChange: null,
    volume: bridge ? (bridge.paused ? 'paused' : 'open') : '—', volumeChange: null, sales: bridge?.lock_count ?? null,
    provenance: bridge ? 'live' : 'pretend', link: '/bridge-slider.html',
  })
  c.push({
    id: 'book', name: 'Shadows in the Chain', glyph: 'book', verified: false, cover: coverSvg('Shadows in the Chain', 'book'),
    blurb: 'The SIGIL novel, chapter by chapter. Not an on-chain record — illustrative listing.',
    items: null, owners: null, floor: '—', floorChange: null, volume: '—', volumeChange: null, sales: null,
    provenance: 'pretend', link: '/downloads/',
  })
  return c
}

function buildDrops(usds: Usds | null, rocky: Rocky | null, gauge: Gauge | null, height: number, blkPerSec: number | null, bridge: Bridge | null): Drop[] {
  const d: Drop[] = []
  const rockyLive = Number(gauge?.live_height ?? 0)
  const left = rockyLive && height ? rockyLive - height : null
  d.push({
    id: 'rocky', name: 'ROCKY · K⊕ Cover token', glyph: 'shield', cover: coverSvg('ROCKY drop', 'shield', { symbol: 'ROCKY' }),
    status: rocky?.live ? (rocky.bootstrapped ? 'live' : 'minting') : 'upcoming',
    when: rocky?.live ? `live at block ${fmt.int(rockyLive)}` : left !== null ? (blkPerSec ? `${fmt.blocksToEta(left, blkPerSec)} · ${fmt.int(left)} blocks to ${fmt.int(rockyLive)}` : `${fmt.int(left)} blocks to ${fmt.int(rockyLive)} · measuring rate…`) : '—',
    detail: 'GaugePush + ROCKY activate together at GAUGE_LIVE_HEIGHT. Countdown is measured from the live height and block rate.',
    provenance: gauge ? 'live' : 'pretend', link: '/kristensen-board.html',
    progress: rockyLive && height ? Math.min(1, height / rockyLive) : null,
  })
  d.push({
    id: 'usds', name: 'USDS · SIGIL Dollar', glyph: 'token', cover: coverSvg('USDS drop', 'token', { symbol: 'USDS' }),
    status: usds?.live ? 'live' : 'upcoming',
    when: usds ? (usds.live ? `live since block ${fmt.int(usds.live_height)}` : `activates at ${fmt.int(usds.live_height)}`) : '—',
    detail: usds ? `Fixed-buffer stablecoin · mint buffer ${usds.mint_buffer_bps ?? '—'} bps · supply ${fmt.num(glyphsToSigil(usds.usds_supply, usds.usds_decimals))} USDS · oracle ${usds.price_fresh ? 'fresh' : 'not fed yet'}` : '',
    provenance: usds ? 'live' : 'pretend', link: '/api.html#usds', progress: usds?.live ? 1 : null,
  })
  d.push({
    id: 'coins', name: 'SIGIL Coins · first NFC batch', glyph: 'coin', cover: coverSvg('Coins drop', 'coin', { symbol: 'NTAG215' }),
    status: 'upcoming', when: 'after the hardware round trip', detail: '1–10 SIGIL per coin, first tap wins. Wallet 1.11+ can mint and claim; no tag has been written yet.',
    provenance: 'live', link: '/sigil-wallet-tron-embedded.html', progress: null,
  })
  d.push({
    id: 'wsigil3', name: 'wSIGIL3 · Polygon leg', glyph: 'braid', cover: coverSvg('wSIGIL3', 'braid', { symbol: 'POLYGON' }),
    status: bridge ? (bridge.paused ? 'blocked' : 'live') : 'live', when: 'ERC-20 + Uniswap pool live',
    detail: 'Token and pool exist on Polygon. Bridge mints wait on the relayer; locks on SIGIL are real.',
    provenance: 'pretend', link: '/bridge-slider.html', progress: null,
  })
  d.push({
    id: 'soneium', name: 'wSIGIL · Soneium leg', glyph: 'braid', cover: coverSvg('Soneium', 'braid', { symbol: 'SONEIUM' }),
    status: 'blocked', when: 'needs ETH on Soneium', detail: 'Compiled and verified; not deployed. Blocked on gas funding.',
    provenance: 'pretend', link: '/bridge-slider.html', progress: null,
  })
  return d
}

function buildMovers(miners: Miners | null, mem: Mem, at: number, hist: { history: HashPoint[] } | null): Mover[] {
  if (!miners) return []
  mem.miners = mem.miners || {}
  mem.series = mem.series || {}
  for (const m of miners.miners) { const arr = mem.series[m.wallet] || []; if (!arr.length || arr[arr.length - 1] !== m.hash_rate) arr.push(m.hash_rate); mem.series[m.wallet] = arr.slice(-48) }
  const out: Mover[] = miners.miners
    .slice()
    .sort((a, b) => b.hash_rate - a.hash_rate)
    .map((m) => {
      const prev = mem.miners![m.wallet]
      let change: number | null = null
      if (prev && prev.hps > 0 && at - prev.at > 30_000) change = ((m.hash_rate - prev.hps) / prev.hps) * 100
      // keep the oldest sample within 24 h as the reference, so "today" means today
      if (!prev || at - prev.at > 86_400_000) mem.miners![m.wallet] = { hps: m.hash_rate, at }
      return {
        id: m.wallet, name: m.rig || fmt.short(m.wallet, 6), sub: `${m.kind.toUpperCase()} · ${m.shielded ? 'shielded' : 'transparent'} · ${fmt.ago(m.last_seen_secs_ago)}`,
        cover: coverSvg(m.rig || m.wallet, 'rig'), value: fmt.hps(m.hash_rate), change, provenance: change === null ? 'live' : 'derived', series: mem.series![m.wallet],
      }
    })
  // net hashrate as the first mover when history exists
  const h = hist?.history
  if (h && h.length > 1) {
    const first = h[0], last = h[h.length - 1]
    out.unshift({
      id: 'net', name: 'Network hashrate', sub: `${last.miners} miners · ${fmt.ago((Date.now() / 1000) - first.timestamp)} window`,
      cover: coverSvg('Network hashrate', 'braid'), value: fmt.hps(last.hashrate),
      change: first.hashrate > 0 ? ((last.hashrate - first.hashrate) / first.hashrate) * 100 : null, provenance: 'derived', series: h.map((p) => p.hashrate),
    })
  }
  return out.slice(0, 8)
}

function buildSales(earth: Earth | null, docket: Docket | null, usds: Usds | null, recent: Recent | null, miners: Miners | null): Sale[] {
  const s: Sale[] = []
  const a = earth?.attest_last?.anchor
  if (a?.tx_hash) s.push({
    id: 'attest', name: `Attestation · ${a.memo?.split(':')[1] ? 'row #' + a.memo.split(':')[1] : 'latest'}`, collection: 'Kristensen Earth K⊕',
    cover: coverSvg('Kristensen Earth', 'earth'), price: `${a.amount ?? '—'} glyphs + ${a.fee ? fmt.int(a.fee) : '—'} fee`,
    when: a.ts ? new Date(a.ts).toLocaleString() : '—', proof: a.tx_hash, provenance: 'live',
  })
  for (const e of docket?.entries ?? []) {
    const ev = e.event as { order?: string; citation?: string; kind?: string; name?: string }
    s.push({
      id: `docket-${e.seq}`, name: e.kind === 'HonourConferred' ? `${ev.order ?? 'Honour'} conferred` : `${e.kind.replace(/([A-Z])/g, ' $1').trim()}`,
      collection: e.kind === 'HonourConferred' ? 'Elefantordenen' : 'Justices of the Bench',
      cover: coverSvg(e.kind + e.seq, e.kind === 'HonourConferred' ? 'seal' : 'shield'), price: `docket #${e.seq}`,
      when: e.height ? `block ${fmt.int(e.height)}` : 'genesis bench', proof: e.leaf, provenance: 'live',
    })
  }
  if (usds?.live) s.push({
    id: 'usds-live', name: 'USDS activation', collection: 'Drops', cover: coverSvg('USDS drop', 'token', { symbol: 'USDS' }),
    price: `block ${fmt.int(usds.live_height)}`, when: 'this week', proof: usds.token_id, provenance: 'live',
  })
  if (recent && recent.blocks.length) {
    const b = recent.blocks[0]
    s.push({
      id: 'block', name: `Block ${fmt.int(b.height)}`, collection: 'DagKnight Blocks', cover: coverSvg('block' + b.height, 'block'),
      price: `blue score ${fmt.int(b.blue_score)}`, when: 'just now', proof: hex(b.hash), provenance: 'live',
    })
  }
  if (miners && miners.miners.length) {
    const top = miners.miners.slice().sort((x, y) => y.hash_rate - x.hash_rate)[0]
    s.push({
      id: 'toprig', name: top.rig || fmt.short(top.wallet, 6), collection: "Miners' Rigs", cover: coverSvg(top.rig || top.wallet, 'rig'),
      price: fmt.hps(top.hash_rate), when: fmt.ago(top.last_seen_secs_ago), proof: top.wallet, provenance: 'live',
    })
  }
  return s.slice(0, 8)
}

// ── swap math (constant product, 0.3 % fee — same formula the Quillon UI uses) ─
export function quote(pools: Pool[], fromId: string, toId: string, amountIn: number, decimalsIn: number, decimalsOut: number): { out: number; impact: number; pool: Pool } | null {
  const p = pools.find((x) => {
    const a = x.token0 ?? x.token_a, b = x.token1 ?? x.token_b
    return (a === fromId && b === toId) || (a === toId && b === fromId)
  })
  if (!p) return null
  const a = p.token0 ?? p.token_a
  const fwd = a === fromId
  const rIn = glyphsToSigil(fwd ? (p.reserve0 ?? p.reserve_a) : (p.reserve1 ?? p.reserve_b), decimalsIn)
  const rOut = glyphsToSigil(fwd ? (p.reserve1 ?? p.reserve_b) : (p.reserve0 ?? p.reserve_a), decimalsOut)
  if (!(rIn > 0 && rOut > 0)) return null
  const fee = (p.fee_bps ?? 30) / 10_000
  const inNet = amountIn * (1 - fee)
  const out = (inNet * rOut) / (rIn + inNet)
  const mid = rOut / rIn
  const impact = mid > 0 ? (1 - (out / amountIn) / mid) * 100 : 0
  return { out, impact, pool: p }
}

export { GLYPHS_PER_SIGIL }
