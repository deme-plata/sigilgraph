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
  by?: string
  cat: 'mining' | 'court' | 'shielded' | 'chain' | 'tokens' | 'bridge' | 'science' | 'physical' | 'story'
}

export interface Drop {
  id: string
  name: string
  glyph: Glyph
  cover: string
  status: 'live' | 'minting' | 'upcoming' | 'blocked' | 'external' // external: live on another chain, read from nowhere on this page
  when: string          // "live since block 6,400,000" / "in ~2.1 h (81,418 blocks)"
  detail: string
  provenance: Provenance
  link: string
  progress: number | null // 0..1 for countdowns
  countdown?: { target: number; height: number; blkPerSec: number | null; at: number }
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
  share?: number
  idle?: boolean
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
  wallet?: string // whose record it is (64-hex) — the page marks the connected wallet's own
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
  supplySigil: number      // minted = transparent wallets + value sealed in the shielded pool (NaN until both routes answer)
  transparentSigil: number // what /v1/supply calls native_supply: the sum of transparent balances — it FALLS when coins are shielded
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
  poolsRead: boolean   // false = /v1/pools did not answer this poll; pools is then [] but means nothing
  miners: Miners | null
  docket: Docket | null
  recent: Recent | null
  earth: Earth | null
  rocky: Rocky | null
  usds: Usds | null
  bridge: Bridge | null
  nation: Nation | null
  anchor: Anchor | null
  offline: boolean
  hashHist: number[]
  changes: Record<string, Record<string, number | null>>
  sampleAgeMin: number
  gaugeFresh: number | null
  gaugeFeeds: number
}

// ── rate memory: block rate + per-miner deltas survive reloads ──────────────
interface Mem { height?: number; at?: number; miners?: Record<string, { hps: number; at: number }>; netHps?: { hps: number; at: number }[]; series?: Record<string, number[]> }
const MEM_KEY = 'sigilvm-mem-v1'
function loadMem(): Mem { try { return JSON.parse(localStorage.getItem(MEM_KEY) || '{}') } catch { return {} } }
function saveMem(m: Mem) { try { localStorage.setItem(MEM_KEY, JSON.stringify(m)) } catch { /* ignore */ } }

// ── per-collection sample series (items over time) so the 1h/6h/24h/7d tabs measure something ──
interface Sample { t: number; v: number }
const SER_KEY = 'sigilvm-series-v1'
const WINDOWS: Record<string, number> = { '1h': 3600e3, '6h': 6 * 3600e3, '24h': 86400e3, '7d': 7 * 86400e3 }
function loadSeries(): Record<string, Sample[]> { try { return JSON.parse(localStorage.getItem(SER_KEY) || '{}') } catch { return {} } }
function saveSeries(x: Record<string, Sample[]>) { try { localStorage.setItem(SER_KEY, JSON.stringify(x)) } catch { /* quota */ } }
function pushSample(ser: Record<string, Sample[]>, id: string, v: number | null, at: number) {
  if (v === null || !isFinite(v)) return
  const arr = ser[id] || (ser[id] = [])
  const last = arr[arr.length - 1]
  if (last && at - last.t < 60e3) return                  // one sample a minute at most
  arr.push({ t: at, v })
  // compaction: full resolution for 24 h, then one sample an hour, nothing older than 7 d
  const cut24 = at - 86400e3, cut7 = at - 7 * 86400e3
  const out: Sample[] = []
  let lastHour = -1
  for (const smp of arr) {
    if (smp.t < cut7) continue
    if (smp.t >= cut24) { out.push(smp); continue }
    const hr = Math.floor(smp.t / 3600e3)
    if (hr !== lastHour) { out.push(smp); lastHour = hr }
  }
  ser[id] = out
}
function windowChange(arr: Sample[] | undefined, win: string, at: number): number | null {
  if (!arr || arr.length < 2) return null
  const start = at - WINDOWS[win]
  // the oldest sample inside the window, but only if it is at least 40 % of the window old (else "—")
  const ref = arr.find((x) => x.t >= start)
  if (!ref) return null
  const now = arr[arr.length - 1]
  if (now.t - ref.t < WINDOWS[win] * 0.4) return null
  return ref.v > 0 ? ((now.v - ref.v) / ref.v) * 100 : null
}

let lastSnapshot: Snapshot | null = null
let heavyAt = 0
let heavyCache: { recent: Recent | null; hist: { history: HashPoint[] } | null } = { recent: null, hist: null }
export const getLast = () => lastSnapshot

/** First paint may pass `soft`: routes that have not answered within `ms` count as unread for THIS render (dash, never
 *  a zero) and the page paints with what arrived; when the stragglers settle, `onLate` fires so the caller re-polls.
 *  If NOTHING answered by the deadline the render waits for the full fan-out instead — a slow link must not paint a
 *  false "node unreachable". */
export async function buildSnapshot(soft?: { ms: number; onLate: (full: Snapshot) => void }): Promise<Snapshot> {
  const at = Date.now()
  // the two heavy routes (~90 KB each, the site proxy does not gzip them) refresh every 60 s; everything else every poll
  const heavyDue = at - heavyAt > 60_000 || !heavyCache.recent
  const reqs = [
    api.supply(), api.miners(), api.anchor(), api.docket(), api.nation(), api.usds(), api.rocky(), api.gauge(),
    api.bridge(), api.cert(), heavyDue ? api.recent() : Promise.resolve(heavyCache.recent), api.pools(), heavyDue ? api.hashHistory() : Promise.resolve(heavyCache.hist), api.earth(),
  ] as const
  let results: Results
  if (soft) {
    const LATE = Symbol('late'); let late = false
    const timer = new Promise<typeof LATE>((r) => setTimeout(() => r(LATE), soft.ms))
    results = (await Promise.all(reqs.map((p) => Promise.race([p, timer]).then((v) => (v === LATE ? ((late = true), null) : v))))) as unknown as Results
    if (late && results.every((v) => v === null)) results = (await Promise.all(reqs)) as unknown as Results // nothing yet: wait, do not paint offline
    else if (late) Promise.all(reqs).then((full) => soft.onLate(assemble(full as unknown as Results, at, heavyDue))).catch(() => { /* the regular poll will catch up */ })
  } else results = (await Promise.all(reqs)) as unknown as Results
  return assemble(results, at, heavyDue)
}

type Results = [Supply | null, Miners | null, Anchor | null, Docket | null, Nation | null, Usds | null, Rocky | null, Gauge | null, Bridge | null, Cert | null, Recent | null, Pool[] | null, { history: HashPoint[] } | null, Earth | null]
function assemble(results: Results, at: number, heavyDue: boolean): Snapshot {
  const [supply, miners, anchor, docket, nation, usds, rocky, gauge, bridge, cert, recentNew, pools, histNew, earth] = results
  if (heavyDue && (recentNew || histNew)) { heavyAt = at; heavyCache = { recent: recentNew, hist: histNew } }
  const recent = recentNew, hist = histNew
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

  const ser = loadSeries()
  for (const c of collections) pushSample(ser, c.id, c.items, at)
  pushSample(ser, 'net_hps', miners?.net_hps ?? null, at)
  saveSeries(ser)
  const changes: Record<string, Record<string, number | null>> = {}
  for (const c of collections) { changes[c.id] = {}; for (const w of Object.keys(WINDOWS)) changes[c.id][w] = windowChange(ser[c.id], w, at) }
  for (const c of collections) { if (c.id === 'rigs' && head.hashChange !== null) { c.volumeChange = head.hashChange } c.floorChange = changes[c.id]['24h'] }
  const oldest = Math.min(...Object.values(ser).flat().map((x) => x.t).concat([at]))
  const sampleAgeMin = Math.round((at - oldest) / 60e3)
  const featured = [...collections].sort((a, b) => (b.items ?? 0) - (a.items ?? 0)).slice(0, 6)
  lastSnapshot = { at, head, collections, featured, drops, movers, sales, tokens, pools: pools ?? [], poolsRead: pools !== null, miners, docket, recent, earth, rocky, usds, bridge, nation, anchor, offline, hashHist, changes, sampleAgeMin, gaugeFresh: gauge ? gauge.feeds.filter((f) => f.fresh).length : null, gaugeFeeds: gauge?.feeds.length ?? 0 }
  return lastSnapshot
}

function buildHead(supply: Supply | null, miners: Miners | null, anchor: Anchor | null, nation: Nation | null, cert: Cert | null, blkPerSec: number | null, ok: boolean, hashHist: number[] = []): ChainHead {
  const hashChange = hashHist.length > 1 && hashHist[0] > 0 ? ((hashHist[hashHist.length - 1] - hashHist[0]) / hashHist[0]) * 100 : null
  return {
    hashChange,
    height: miners?.height ?? nation?.height ?? cert?.certificate.height ?? 0,
    blkPerSec,
    netHps: miners ? miners.net_hps : NaN,       // NaN = the mining route did not answer; every formatter prints — for it
    liveMiners: miners ? miners.live_miners : NaN,
    finalityGate: cert?.certificate.gate ?? '—',
    finalityHeight: cert ? cert.certificate.height : NaN,
    committee: cert ? cert.certificate.committee_size : NaN,
    // 09-17: native_supply dropped 179K → 150K while the pool's value_locked rose by the same 29K — shielding moves coins out
    // of the wallet map, so 'supply' on its own undercounts what was minted. Minted = transparent + shielded.
    supplySigil: supply && anchor ? glyphsToSigil(supply.native_supply) + glyphsToSigil(anchor.value_locked) : NaN,
    transparentSigil: supply ? glyphsToSigil(supply.native_supply) : NaN,
    maxSupplySigil: supply ? glyphsToSigil(supply.max_supply) : NaN,
    mintedPct: supply && anchor ? (glyphsToSigil(supply.native_supply) + glyphsToSigil(anchor.value_locked)) / glyphsToSigil(supply.max_supply) * 100 : NaN,
    valueLocked: anchor ? glyphsToSigil(anchor.value_locked) : NaN,
    notes: anchor ? anchor.notes : NaN,
    nullifiers: anchor ? anchor.nullifiers : NaN,
    registered: anchor ? anchor.registered : NaN,
    capacity: anchor ? anchor.capacity : NaN,
    treasurySigil: nation ? glyphsToSigil(nation.treasury_glyphs) : NaN,
    ok,
    lagBlocks: cert ? Math.max(0, (miners?.height ?? nation?.height ?? cert.certificate.height) - cert.certificate.height) : NaN, // no certificate read → unknown, never 'N blocks behind'
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
    supply: supply && anchor ? glyphsToSigil(supply.native_supply) + glyphsToSigil(anchor.value_locked) : null, maxSupply: glyphsToSigil(supply?.max_supply), // minted = transparent + shielded
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
    cat: 'mining', id: 'rigs', by: miners ? `by ${fmt.n(miners.live_miners, 'rig')}` : undefined, name: "Miners' Rigs", glyph: 'rig', verified: true, cover: coverSvg("Miners' Rigs", 'rig'),
    blurb: 'Every rig submitting shares to the braid right now. Rank = hashrate share.',
    items: miners?.live_miners ?? null, owners: miners ? new Set(miners.miners.map((m) => m.wallet)).size : null,
    floor: miners && miners.miners.length ? fmt.hps(Math.min(...miners.miners.filter((m) => m.hash_rate > 0).map((m) => m.hash_rate).concat([Infinity]))).replace('Infinity H/s', '—') : '—', floorChange: null,
    volume: miners ? fmt.hps(miners.net_hps) : '—', volumeChange: netChange, sales: miners?.blocks_accepted ?? null,
    provenance: miners ? 'live' : 'pretend', link: '/sigil-explorer.html',
  })
  c.push({
    cat: 'court', id: 'honours', by: 'by the SIGIL Supreme Court', name: 'Elefantordenen', glyph: 'seal', verified: true, cover: coverSvg('Elefantordenen', 'seal'),
    blurb: 'Honours conferred by the SIGIL Supreme Court. Each is a leaf in a verified docket chain.',
    items: honours, owners: honours, floor: docket ? '1 honour' : '—', floorChange: null,
    volume: docket ? `${fmt.n(docket.total, 'docket entry', 'docket entries')}` : '—', volumeChange: null, sales: honours,
    provenance: docket ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html#court',
  })
  c.push({
    cat: 'court', id: 'bench', by: 'by the constitution', name: 'Justices of the Bench', glyph: 'shield', verified: true, cover: coverSvg('Justices of the Bench', 'shield'),
    blurb: 'Appointed justices. The bench that hears cases, rules and seals precedents.',
    items: justices, owners: justices, floor: docket ? '1 seat' : '—', floorChange: null,
    volume: docket?.chain_verified ? 'chain verified ✓' : '—', volumeChange: null, sales: justices,
    provenance: docket ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html#court',
  })
  c.push({
    cat: 'chain', id: 'blocks', by: recent ? `by ${new Set(recent.blocks.map((b) => hex(b.producer))).size} producer${new Set(recent.blocks.map((b) => hex(b.producer))).size === 1 ? '' : 's'}` : undefined, name: 'DagKnight Blocks', glyph: 'block', verified: true, cover: coverSvg('DagKnight Blocks', 'block'),
    blurb: 'The braid itself. Blue blocks ordered by blue score; every one names its producer.',
    items: miners?.height ?? null, owners: recent ? new Set(recent.blocks.map((b) => hex(b.producer))).size : null,
    floor: recent && recent.blocks.length ? `blue ${fmt.int(recent.blocks[0].blue_score)}` : '—', floorChange: null,
    volume: recent ? `${recent.blocks.length} recent` : '—', volumeChange: null, sales: recent?.blocks.filter((b) => b.is_blue).length ?? null,
    provenance: recent ? 'live' : 'pretend', link: '/sigil-explorer.html',
  })
  c.push({
    cat: 'shielded', id: 'notes', by: anchor ? `by ${fmt.n(anchor.registered, 'registered wallet')}` : undefined, name: 'Shielded Notes', glyph: 'eye', verified: true, cover: coverSvg('Shielded Notes', 'eye'),
    blurb: 'Sealed 32-byte notes in the shielded pool. First tap wins; a spent note is worth zero everywhere.',
    items: anchor?.notes ?? null, owners: anchor?.registered ?? null,
    floor: anchor ? `${fmt.int(anchor.capacity - anchor.notes)} free` : '—', floorChange: null,
    volume: anchor ? `${fmt.num(glyphsToSigil(anchor.value_locked))} SIGIL locked` : '—', volumeChange: null, sales: anchor?.nullifiers ?? null,
    provenance: anchor ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html',
  })
  const attest = earth?.attest_last?.anchor
  c.push({
    cat: 'science', id: 'earth', by: 'by sigil-earth · IERS + GFZ', name: 'Kristensen Earth K⊕', glyph: 'earth', verified: true, cover: coverSvg('Kristensen Earth', 'earth'),
    blurb: 'Earth-rotation attestations anchored on SIGIL from a wallet with a published viewing key.',
    items: earth ? (earth.row ?? (attest?.memo && /:(\d+):/.test(attest.memo) ? Number(RegExp.$1) : null)) : null, owners: earth ? 1 : null, // one attester wallet — but only a fact when the feed answered
    floor: attest?.amount ? `${attest.amount} glyphs` : '—', floorChange: null,
    volume: attest?.tx_hash ? `anchored ${fmt.short(attest.tx_hash, 6)}` : '—', volumeChange: null, sales: attest?.executed ? 1 : 0,
    provenance: earth ? 'live' : 'pretend', link: '/kristensen-earth.html',
  })
  c.push({
    cat: 'physical', id: 'coins', by: 'by the living room', name: 'SIGIL Coins (NFC)', glyph: 'coin', verified: false, cover: coverSvg('SIGIL Coins', 'coin', { symbol: 'NTAG215' }),
    blurb: 'Physical coins: one shielded note on a 504-byte tag. No batch anchored yet — this collection is empty on chain.',
    items: 0, owners: 0, floor: '—', floorChange: null, volume: '0 batches anchored', volumeChange: null, sales: 0,
    provenance: 'live', link: '/sigil-wallet-tron-embedded.html',
  })
  c.push({
    cat: 'tokens', id: 'treasury', by: 'by the mining dev-fee carve', name: 'Nation Treasury', glyph: 'token', verified: true, cover: coverSvg('Nation Treasury', 'token'),
    blurb: 'The welfare treasury financed by the mining dev-fee carve, paid out in USDS.',
    items: 1, owners: 1, floor: nation ? `${fmt.num(glyphsToSigil(nation.treasury_glyphs))} SIGIL` : '—', floorChange: null,
    volume: nation ? `${nation.welfare_bps} bps carve` : '—', volumeChange: null, sales: null,
    provenance: nation ? 'live' : 'pretend', link: '/sigil-nation-whitepaper.html',
  })
  c.push({
    cat: 'tokens', id: 'rocky', by: 'by Rocky', name: rocky?.name ?? 'Rocky K⊕ Cover', glyph: 'shield', verified: true, cover: coverSvg('Rocky Cover', 'shield', { symbol: 'ROCKY' }),
    blurb: 'Cover policies underwritten by the ROCKY pool against K⊕ p99 excursions. First native contract that executes.',
    items: rocky?.policies.length ?? null, owners: null, floor: rocky ? `${rocky.fee_bps} bps fee` : '—', floorChange: null,
    volume: rocky ? `${fmt.num(glyphsToSigil(rocky.pool_balance, rocky.decimals))} ROCKY pooled` : '—', volumeChange: null, sales: rocky?.policies.length ?? null,
    provenance: rocky ? 'live' : 'pretend', link: '/v1/rocky', // no site page documents ROCKY yet — the node's own record is the honest source
  })
  c.push({
    cat: 'bridge', id: 'bridge', by: 'by the relayer', name: 'Bridge Locks', glyph: 'braid', verified: true, cover: coverSvg('Bridge Locks', 'braid'),
    blurb: 'SIGIL locked for the Polygon leg. Relayer held; locks are real, mints are not yet.',
    items: bridge?.lock_count ?? null, owners: null, floor: bridge ? `${fmt.num(glyphsToSigil(bridge.vault_balance))} SIGIL vault` : '—', floorChange: null,
    volume: bridge ? (bridge.paused ? 'paused' : 'open') : '—', volumeChange: null, sales: bridge?.lock_count ?? null,
    provenance: bridge ? 'live' : 'pretend', link: '/bridge-slider.html',
  })
  c.push({
    cat: 'story', id: 'book', by: 'by the sigil-book crate', name: 'Shadows in the Chain', glyph: 'book', verified: false, cover: coverSvg('Shadows in the Chain', 'book'),
    blurb: 'The SIGIL novel, chapter by chapter. A published PDF, not an on-chain record — the one collection here that is a story.',
    items: null, owners: null, floor: 'free PDF', floorChange: null, volume: 'v28', volumeChange: null, sales: null,
    provenance: 'pretend', link: '/downloads/shadows-in-the-chain.pdf',
  })
  return c
}

function buildDrops(usds: Usds | null, rocky: Rocky | null, gauge: Gauge | null, height: number, blkPerSec: number | null, bridge: Bridge | null): Drop[] {
  const d: Drop[] = []
  const rockyLive = Number(gauge?.live_height ?? 0)
  const left = rockyLive && height ? rockyLive - height : null
  const rockyUnread = rocky === null && (left === null || left <= 0) // the gate has passed (or is unknown) and the rocky route did not answer — no countdown can be honest
  d.push({
    id: 'rocky', name: 'ROCKY · K⊕ Cover token', glyph: 'shield', cover: coverSvg('ROCKY drop', 'shield', { symbol: 'ROCKY' }),
    status: rocky?.live ? (rocky.bootstrapped ? 'live' : 'minting') : 'upcoming',
    when: rockyUnread ? 'state unread this poll — the rocky route did not answer' : rocky?.live ? `live at block ${fmt.int(rockyLive)}` : left !== null ? (blkPerSec ? `${fmt.blocksToEta(left, blkPerSec)} · ${fmt.int(left)} blocks to ${fmt.int(rockyLive)}` : `${fmt.int(left)} blocks to ${fmt.int(rockyLive)} · ETA after the next poll`) : '—',
    detail: 'GaugePush + ROCKY activate together at GAUGE_LIVE_HEIGHT. Countdown is measured from the live height and block rate.',
    provenance: gauge && !rockyUnread ? 'live' : 'pretend', link: '/kristensen-board.html',
    progress: rockyLive && height && !rockyUnread ? Math.min(1, height / rockyLive) : null,
    countdown: rockyLive && height && !rocky?.live && !rockyUnread ? { target: rockyLive, height, blkPerSec, at: Date.now() } : undefined,
  })
  d.push({
    id: 'usds', name: 'USDS · SIGIL Dollar', glyph: 'token', cover: coverSvg('USDS drop', 'token', { symbol: 'USDS' }),
    status: usds?.live ? 'live' : 'upcoming',
    when: usds ? (usds.live ? `live since block ${fmt.int(usds.live_height)}` : `activates at ${fmt.int(usds.live_height)}`) : '—',
    detail: usds ? `Fixed-buffer stablecoin · mint buffer ${usds.mint_buffer_bps ?? '—'} bps · supply ${fmt.num(glyphsToSigil(usds.usds_supply, usds.usds_decimals))} USDS · oracle ${usds.price_fresh ? 'fresh' : 'not fed yet'}` : '',
    provenance: usds ? 'live' : 'pretend', link: '/sigil-wallet-tron-embedded.html', progress: usds?.live ? 1 : null,
  })
  d.push({
    id: 'coins', name: 'SIGIL Coins · first NFC batch', glyph: 'coin', cover: coverSvg('Coins drop', 'coin', { symbol: 'NTAG215' }),
    status: 'upcoming', when: 'after the hardware round trip', detail: '1–10 SIGIL per coin, first tap wins. Wallet 1.11+ can mint and claim; no tag has been written yet.',
    provenance: 'live', link: '/sigil-wallet-tron-embedded.html', progress: null,
  })
  d.push({
    id: 'wsigil3', name: 'wSIGIL3 · Polygon leg', glyph: 'braid', cover: coverSvg('wSIGIL3', 'braid', { symbol: 'POLYGON' }),
    status: bridge?.paused ? 'blocked' : 'external', when: 'ERC-20 + Uniswap pool live on Polygon', // 'PRETEND … LIVE' on one card read as a contradiction: the leg is live on Polygon, but nothing here reads it
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
        share: miners.net_hps > 0 ? (m.hash_rate / miners.net_hps) * 100 : undefined,
        idle: m.hash_rate <= 0,
      }
    })
  // net hashrate as the first mover when history exists
  const h = hist?.history
  if (h && h.length > 1) {
    const first = h[0], last = h[h.length - 1]
    out.unshift({
      id: 'net', name: 'Network hashrate', sub: `${fmt.n(last.miners, 'miner')} · ${fmt.ago((Date.now() / 1000) - first.timestamp).replace(' ago', '')} window · node history`,
      // the figure is the live net_hps the rig shares are computed against (the history's last point is up to 60 s old
      // and disagreed with the shares by ~5 %); the window change and the sparkline stay on the history
      cover: coverSvg('Network hashrate', 'braid'), value: fmt.hps(miners ? miners.net_hps : last.hashrate),
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
    when: a.ts ? new Date(a.ts).toLocaleString() : '—', proof: a.tx_hash, provenance: 'live', wallet: (a.wallet || '').toLowerCase() || undefined,
  })
  for (const e of docket?.entries ?? []) {
    const ev = e.event as { order?: string; citation?: string; rank?: string; justice?: number[]; recipient?: number[]; approvals?: number; operator_cosigned?: boolean }
    const who = hex(ev.justice ?? ev.recipient)
    const rank = (ev.rank || '').replace(/([A-Z])/g, ' $1').trim()
    s.push({
      id: `docket-${e.seq}`,
      name: e.kind === 'HonourConferred' ? `${ev.order ?? 'Honour'} · ${fmt.short(who, 4)}` : `${rank || 'Justice'} · ${fmt.short(who, 4)}`,
      collection: e.kind === 'HonourConferred' ? `Elefantordenen · ${fmt.n(ev.approvals ?? null, 'approval')}${ev.operator_cosigned ? ' · co-signed' : ''}` : 'Justices of the Bench',
      cover: coverSvg(who || e.kind + e.seq, e.kind === 'HonourConferred' ? 'seal' : 'shield'), price: `docket #${e.seq}`, wallet: who || undefined,
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
