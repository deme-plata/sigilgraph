// api.ts — every number on the page comes through here. Same-origin /v1 on
// sigilgraph.org (fluxc serve proxies /v1 → sigil-api :18181 and /v1/earth →
// sigil-earth :8460). Override for local shoots: ?api=http://127.0.0.1:18181
// or localStorage['sigilvm-api'].

export const GLYPHS_PER_SIGIL = 1e10 // g2: 10 decimals, base unit "glyph"
export const NATIVE_TOKEN = '0'.repeat(64)

function resolveBase(): string {
  try {
    const q = new URLSearchParams(location.search).get('api')
    if (q) { localStorage.setItem('sigilvm-api', q); return q.replace(/\/+$/, '') }
    const s = localStorage.getItem('sigilvm-api')
    if (s) return s.replace(/\/+$/, '')
  } catch { /* storage may be blocked */ }
  return ''
}
export const API_BASE = resolveBase()

async function getJson<T>(path: string, timeoutMs = 8000): Promise<T | null> {
  const ctl = new AbortController()
  const t = setTimeout(() => ctl.abort(), timeoutMs)
  try {
    const r = await fetch(API_BASE + path, { signal: ctl.signal, headers: { accept: 'application/json' } })
    const ct = r.headers.get('content-type') || ''
    if (!r.ok || !ct.includes('json')) return null // a 200 text/html is the SPA fallback, not data
    return (await r.json()) as T
  } catch {
    return null
  } finally {
    clearTimeout(t)
  }
}

// ── wire shapes (only the fields the page reads) ──────────────────────────
export interface Envelope<T> { ok: boolean; data: T; ts?: number }
export interface Supply { native_supply: string; max_supply: string; minted_pct: number }
export interface Miner { wallet: string; rig: string; hash_rate: number; last_seen_secs_ago: number; kind: string; shielded: boolean }
export interface Miners { height: number; net_hps: number; live_miners: number; blocks_accepted: number; shares_accepted: number; miners: Miner[] }
export interface Anchor { anchor: string; capacity: number; epoch: number; notes: number; nullifiers: number; registered: number; value_locked: string }
export interface DocketEntry { seq: number; height: number; kind: string; leaf: string; event: Record<string, unknown> }
export interface Docket { total: number; head: string; root: string; chain_verified: boolean; histogram: Record<string, number>; entries: DocketEntry[] }
export interface Nation { active: boolean; height: number; activation_height: number; treasury_glyphs: string; treasury_wallet: string; stipend_glyphs: string; payout_asset: string; usds_live_height: number; welfare_bps: number; claim_interval_blocks: number; authority_wallet: string }
export interface Usds { price: string; price_fresh: boolean; mint_buffer_bps?: number; usds_supply: string; vault_sigil: string; live: boolean; live_height: number; height: number; usds_decimals: number; fee_bps: number; token_id: string }
export interface Rocky { symbol: string; name: string; token: string; contract: string; decimals: number; live: boolean; bootstrapped: boolean; total_supply: string; circulating: string; pool_balance: string; fee_bps: number; policies: unknown[] }
export interface Gauge { height: number; live_height: string; live: boolean; feeds: { name: string; value: number | null; fresh: boolean }[] }
export interface Bridge { vault: string; vault_balance: string; relayer: string; paused: boolean; lock_count: number }
export interface Cert { certificate: { bft: boolean; committee_size: number; gate: string; height: number; order_hash: string; quorum: number } }
export interface DagBlock { hash: number[]; parent: number[]; height: number; producer: number[]; blue_score: number; is_blue: boolean }
export interface Recent { blocks: DagBlock[] }
export interface Pool { id?: string; pool_id?: string; token0?: string; token1?: string; token_a?: string; token_b?: string; reserve0?: string; reserve1?: string; reserve_a?: string; reserve_b?: string; fee_bps?: number }
export interface Balance { wallet: string; token: string; balance: string }
export interface HashPoint { timestamp: number; hashrate: number; miners: number }
export interface Earth { attest_last?: { anchor?: { amount?: string; executed?: boolean; tx_hash?: string; ts?: string; memo?: string; fee?: number; wallet?: string } }; alerts_recent?: unknown[]; k_resid?: number; k_p99?: number; row?: number; reading_date?: string }
export interface Challenge { height: number; net_hps: number; vdf_t: number }

const unwrap = <T,>(e: Envelope<T> | null): T | null => (e && e.ok ? e.data : null)

export const api = {
  supply: async () => unwrap(await getJson<Envelope<Supply>>('/v1/supply')),
  miners: async () => unwrap(await getJson<Envelope<Miners>>('/v1/mining/miners')),
  anchor: async () => await getJson<Anchor>('/v1/shielded/anchor'),
  docket: async () => unwrap(await getJson<Envelope<Docket>>('/v1/court/docket')),
  nation: async () => unwrap(await getJson<Envelope<Nation>>('/v1/nation/status')),
  usds: async () => unwrap(await getJson<Envelope<Usds>>('/v1/usds/status')),
  rocky: async () => unwrap(await getJson<Envelope<Rocky>>('/v1/rocky')),
  gauge: async () => unwrap(await getJson<Envelope<Gauge>>('/v1/gauge')),
  bridge: async () => unwrap(await getJson<Envelope<Bridge>>('/v1/bridge/status')),
  cert: async () => await getJson<Cert>('/v1/finality/certificate'),
  recent: async () => unwrap(await getJson<Envelope<Recent>>('/v1/dagknight/recent')),
  pools: async () => { const r = await getJson<{ ok: boolean; pools: Pool[] }>('/v1/pools'); return r && r.ok ? r.pools : null },
  hashHistory: async () => unwrap(await getJson<Envelope<{ history: HashPoint[] }>>('/v1/mining/hashrate/history')),
  earth: async () => await getJson<Earth>('/v1/earth/latest'),
  challenge: async () => await getJson<Challenge>('/v1/mining/challenge'),
  balance: async (wallet: string, token = NATIVE_TOKEN) =>
    unwrap(await getJson<Envelope<Balance>>(`/v1/balance?wallet=${wallet}&token=${token}`)),
}

// ── helpers ─────────────────────────────────────────────────────────────────
export const hex = (bytes: number[] | undefined): string =>
  (bytes || []).map((b) => b.toString(16).padStart(2, '0')).join('')

export function glyphsToSigil(g: string | number | undefined | null, decimals = 10): number {
  if (g === undefined || g === null) return 0
  const s = String(g)
  if (!/^\d+$/.test(s)) return Number(s) / 10 ** decimals || 0
  // exact for big strings: split integer / fraction
  const pad = s.padStart(decimals + 1, '0')
  const int = pad.slice(0, pad.length - decimals)
  const frac = pad.slice(pad.length - decimals)
  return Number(int) + Number('0.' + frac)
}

export const fmt = {
  num(n: number, d = 2): string {
    if (!isFinite(n)) return '—'
    const a = Math.abs(n)
    if (a >= 1e9) return (n / 1e9).toFixed(d) + 'B'
    if (a >= 1e6) return (n / 1e6).toFixed(d) + 'M'
    if (a >= 1e3) return (n / 1e3).toFixed(d) + 'K'
    return n.toFixed(a < 1 && a > 0 ? Math.max(d, 4) : d)
  },
  int(n: number): string { return isFinite(n) ? Math.round(n).toLocaleString('en-US') : '—' },
  /** count + noun with the right number: n(1,'rig') → '1 rig', n(4,'rig') → '4 rigs', n(NaN,'rig') → '— rigs' */
  n(count: number | null | undefined, one: string, many = one + 's'): string { const c = count ?? NaN; return `${fmt.int(c)} ${c === 1 ? one : many}` },
  pct(n: number | null | undefined, d = 1): string {
    if (n === null || n === undefined || !isFinite(n)) return '—'
    return (n > 0 ? '+' : '') + n.toFixed(d) + '%'
  },
  hps(h: number): string {
    if (!isFinite(h)) return '—'
    if (h >= 1e9) return (h / 1e9).toFixed(2) + ' GH/s'
    if (h >= 1e6) return (h / 1e6).toFixed(2) + ' MH/s'
    if (h >= 1e3) return (h / 1e3).toFixed(1) + ' kH/s'
    return h.toFixed(0) + ' H/s'
  },
  short(hexStr: string, n = 4): string { return hexStr && hexStr.length > 2 * n + 2 ? `${hexStr.slice(0, n)}…${hexStr.slice(-n)}` : hexStr || '—' },
  ago(secs: number): string {
    if (!isFinite(secs) || secs < 0) return '—'
    if (secs < 60) return `${Math.round(secs)}s ago`
    if (secs < 3600) return `${Math.round(secs / 60)}m ago`
    if (secs < 86400) return `${Math.round(secs / 3600)}h ago`
    return `${Math.round(secs / 86400)}d ago`
  },
  blocksToEta(blocks: number, blkPerSec: number): string {
    if (blocks <= 0) return 'live'
    if (!(blkPerSec > 0)) return `${fmt.int(blocks)} blocks`
    const s = blocks / blkPerSec
    if (s < 3600) return `~${Math.round(s / 60)} min`
    if (s < 86400) return `~${(s / 3600).toFixed(1)} h`
    return `~${(s / 86400).toFixed(1)} d`
  },
}
