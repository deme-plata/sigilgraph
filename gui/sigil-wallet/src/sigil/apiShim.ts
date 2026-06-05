// SIGIL apiShim — replaces /api/v1/* responses with data from the FLUXFOOD-
// generated snapshot at /sigil-dashboard.json. Installed BEFORE the React
// app boots (see main.tsx) so every fetch the Quillon-derived code path
// makes returns SIGIL-flavoured data instead of falling through to the
// Quillon backend that q-flux proxies for /api.
//
// Pattern: monkey-patch window.fetch + window.EventSource once. The shim
// keeps the snapshot in a small in-memory cache and re-fetches it every
// `SNAP_TTL_MS`. Writes to the zustand store so any component that wants
// to render snapshot-derived state (banner, dashboard) can subscribe.

import { useSigilStore } from './store'
import { generateMnemonic } from '@scure/bip39'
import { wordlist as english } from '@scure/bip39/wordlists/english.js'

const SNAP_TTL_MS = 15_000
const SNAP_URL = '/sigil-dashboard.json'
// WIRING: the live SIGIL node (sigil-rpcd) over TLS. The logged-in wallet's real
// balance is fetched from here so the UI shows live on-chain state (e.g. the
// master-bank's mined balance) instead of the static snapshot's default.
// Override at runtime with window.__SIGIL_NODE if ever needed.
const LIVE_NODE = (typeof window !== 'undefined' && (window as any).__SIGIL_NODE)
  || 'https://sigilgraph.quillon.xyz:8843'

type Snapshot = {
  status: any
  recent_blocks: any[]
  address_balances: Record<string, any>
  default_balance: any
  generated_ms: number
}

let _snap: Snapshot | null = null
let _fetchedAt = 0
let _inflight: Promise<Snapshot | null> | null = null

async function getSnapshot(): Promise<Snapshot | null> {
  const now = Date.now()
  if (_snap && now - _fetchedAt < SNAP_TTL_MS) return _snap
  if (_inflight) return _inflight

  _inflight = (async () => {
    try {
      const res = await origFetch(SNAP_URL + '?t=' + now, { cache: 'no-store' })
      if (res.ok) {
        const s = (await res.json()) as Snapshot
        _snap = s
        _fetchedAt = now
        // sync into the zustand store
        const store = useSigilStore.getState()
        store.setStatus(s.status)
        store.setBlocks(s.recent_blocks)
        store.setBalances(s.address_balances, s.default_balance)
        return s
      }
    } catch (e) {
      // swallow — return null below so the response handlers fall back to
      // a neutral empty payload
      console.warn('[SIGIL shim] snapshot fetch failed', e)
    } finally {
      _inflight = null
    }
    return null
  })()
  return _inflight
}

// ── live DEX cache (pools + tokens) from the running sigil-rpcd node ──
// The wallet's DexScreen fetches GET /api/v1/liquidity/pools + /api/v1/dex/tokens.
// We answer those from the live node's flat /pools + /tokens routes so the DEX
// shows real on-chain liquidity instead of the old empty/static stubs.
type LiveDex = { pools: any[]; tokens: any[] }
let _dex: LiveDex | null = null
let _dexAt = 0
let _dexInflight: Promise<LiveDex | null> | null = null

async function getLiveDex(): Promise<LiveDex | null> {
  const now = Date.now()
  if (_dex && now - _dexAt < SNAP_TTL_MS) return _dex
  if (_dexInflight) return _dexInflight
  _dexInflight = (async () => {
    try {
      const [pr, tr] = await Promise.all([
        origFetch(`${LIVE_NODE}/api/v1/pools`, { cache: 'no-store' }),
        origFetch(`${LIVE_NODE}/api/v1/tokens`, { cache: 'no-store' }),
      ])
      const pj = pr.ok ? await pr.json() : null
      const tj = tr.ok ? await tr.json() : null
      if (pj?.ok || tj?.ok) {
        _dex = { pools: pj?.pools ?? [], tokens: tj?.tokens ?? [] }
        _dexAt = now
        return _dex
      }
    } catch (e) {
      console.warn('[SIGIL shim] live DEX fetch failed', e)
    } finally {
      _dexInflight = null
    }
    return null
  })()
  return _dexInflight
}

// Scale a plain integer reserve into the 24-decimal base units the Quillon
// DEX UI expects (DexScreen divides reserve0/reserve1 by 1e24 for display).
function scale24(n: number | string): string {
  try { return (BigInt(Math.trunc(Number(n))) * 10n ** 24n).toString() } catch { return '0' }
}
const TOKEN_DECIMALS: Record<string, number> = { USDS: 6 }
function tokDecimals(sym: string): number { return TOKEN_DECIMALS[(sym ?? '').toUpperCase()] ?? 18 }

function jsonRes(body: any, status = 200): Response {
  // Make every successful response self-identify as success so the Quillon
  // ApiResponse wrapper sees the boolean even if the api layer parses
  // `body.success` instead of relying on res.ok.
  if (body && typeof body === 'object' && !Array.isArray(body) && status < 400 && body.success === undefined) {
    body = { success: true, ...body }
  }
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

// Keep a reference to the real fetch BEFORE we install the shim — used for
// the snapshot fetch (must not loop back into the shim) + for fall-through.
const origFetch: typeof fetch = window.fetch.bind(window)

// Deterministic preview address from a seed string (no crypto deps; FNV-1a).
function previewAddressFromSeed(seed: string): string {
  let h1 = 0xcbf29ce484222325n
  let h2 = 0x84222325cbf29ce4n
  for (const ch of seed) {
    const c = BigInt(ch.charCodeAt(0))
    h1 ^= c
    h1 = (h1 * 0x100000001b3n) & 0xffffffffffffffffn
    h2 ^= c
    h2 = (h2 * 0xb3000000010000001n) & 0xffffffffffffffffn
  }
  const hex = h1.toString(16).padStart(16, '0') + h2.toString(16).padStart(16, '0')
  // 32 hex chars after the "sgl1" prefix — preview address, no checksum
  return `sgl1${hex}`
}

async function readBodyMnemonic(init?: RequestInit): Promise<string> {
  try {
    const body = (init?.body as any) ?? ''
    const txt = typeof body === 'string' ? body : new TextDecoder().decode(body)
    const parsed = JSON.parse(txt)
    return String(parsed?.mnemonic ?? '')
  } catch {
    return ''
  }
}

async function handleApi(path: string, _init?: RequestInit): Promise<Response> {
  const snap = (await getSnapshot()) ?? {
    status: {
      network_id: 'sigil-g0', height: 0, peers: 0, symbol: 'SGL',
      version: 'sigil-preview', note: 'snapshot not yet loaded', block_time_ms: 12000,
    },
    recent_blocks: [],
    address_balances: {},
    default_balance: {
      address: 'sgl1default', balance_sgl: '100.00',
      balance_raw: '100000000000000000000', tx_count: 0,
    },
    generated_ms: Date.now(),
  }

  useSigilStore.getState().notePath(path)

  // ── flux-error endpoints (POST to log, GET to inspect) ──
  if (path === '/api/v1/flux/error') {
    try {
      const body = (_init?.body as any) ?? ''
      const txt = typeof body === 'string' ? body : new TextDecoder().decode(body)
      const parsed = JSON.parse(txt || '{}')
      const W = window as any
      W.__SIGIL_ERRORS__ = W.__SIGIL_ERRORS__ || []
      W.__SIGIL_ERRORS__.push({ ...parsed, ts_ms: Date.now(), source: 'post' })
    } catch { /* ignore */ }
    return jsonRes({ data: { logged: true } })
  }
  if (path === '/api/v1/flux/errors') {
    const errs = (window as any).__SIGIL_ERRORS__ ?? []
    return jsonRes({ data: { errors: errs, count: errs.length } })
  }

  // ── Dashboard-specific endpoints (proactively stubbed) ──

  // Oracle / AMM price for any token symbol
  const oracleMatch = path.match(/^\/api\/v1\/oracle\/price\/([^\/?]+)/)
  if (oracleMatch) {
    const tok = decodeURIComponent(oracleMatch[1])
    const price_usd = tok === 'USDS' ? 1.00 : tok === 'SGL' ? 0.42 : 1.00
    return jsonRes({
      data: {
        token: tok,
        price_usd,
        source: 'sigil-preview',
        last_updated: Date.now(),
      },
    })
  }

  // Network miners — raw shape, not wrapped (see api.ts getNetworkMiners)
  if (path === '/api/v1/mining/miners') {
    return jsonRes({ success: true, total_miners: 0, total_hashrate: 0, miners: [] })
  }

  // Emission stats — only the fields the Dashboard renders
  if (path.startsWith('/api/v1/emission/stats')) {
    return jsonRes({
      data: {
        summary: {
          total_supply_qug: 1_000_000,
          total_supply_raw: '1000000000000000000000000',
          max_supply_qug: 21_000_000,
          pct_mined: 4.76,
          current_era: 0,
          annual_target_qug: 525_600,
          daily_target_qug: 1440,
          today_emitted_qug: 1432,
          today_blocks: 7200,
          today_deviation_pct: -0.56,
          block_rate_bps: 8333,
          days_tracked: 1,
          stock_to_flow: 14.0,
          inflation_rate_pct: 52.56,
          remaining_supply_qug: 20_000_000,
          reward_per_block_qug: 0.2,
          era_progress_pct: 0.0,
          genesis_timestamp: 1_780_137_000,
          elapsed_secs: 0,
        },
        daily_history: [],
        schedule: {
          era_0_annual: 525_600,
          era_0_daily: 1440,
          era_1_annual: 262_800,
          era_1_daily: 720,
          halving_interval_years: 4,
          total_eras: 32,
          total_emission_years: 128,
        },
      },
    })
  }

  // Email unread count (Dashboard polls this)
  if (path === '/api/v1/email/unread-count') {
    return jsonRes({ data: { count: 0 }, count: 0 })
  }

  // Wallet history — array of unified transaction entries
  const histMatch = path.match(/^\/api\/v1\/wallets\/([^\/]+)\/history$/)
  if (histMatch) {
    return jsonRes({ data: [] })
  }

  // Vault / stablecoin stats — covered by generic data:[] elsewhere

  // ── BIP39 mnemonic — generate locally so the "Generate new" button works
  // without any backend. 128-bit entropy → 12 words.
  if (path === '/api/v1/mnemonic') {
    try {
      const mnemonic = generateMnemonic(english, 128)
      return jsonRes({
        data: { mnemonic, word_count: 12, entropy_bits: 128, source: 'sigil-shim/scure-bip39' },
        mnemonic,
      })
    } catch (e: any) {
      return jsonRes({ success: false, error: String(e?.message ?? e) }, 500)
    }
  }
  if (path === '/api/v1/node/status') {
    return jsonRes({
      data: { ...snap.status, current_height: snap.status.height, network_health: 'good', consensus_status: 'sigil-preview' },
      ...snap.status,
    })
  }

  // ── wallet create / import — make the BIP39 → "create wallet" flow finish ──
  if (path === '/api/v1/wallets/create' || path === '/api/v1/wallets/import') {
    const mnemonic = await readBodyMnemonic(_init)
    const seed = mnemonic || `random-${Date.now()}-${Math.random()}`
    const address = previewAddressFromSeed(seed)
    const id = `sgl-preview-${address.slice(4, 16)}`
    // Both wrapped {success,data} and flat fields — covers both ApiResponse
    // wrappers and direct-body consumers.
    return jsonRes({
      success: true,
      data: {
        id,
        address_formatted: address,
        address,
        public_key: address.slice(4),
        network_id: snap.status.network_id,
        created_at: new Date().toISOString(),
        note: 'SIGIL preview wallet — derived locally by apiShim',
      },
      id,
      address_formatted: address,
    })
  }
  // wallet by id / address — useful when LoginScreen re-fetches after create
  const walletByIdPath = path.match(/^\/api\/v1\/wallets\/([^\/]+)$/)
  if (walletByIdPath) {
    const id_or_addr = walletByIdPath[1]
    const address = id_or_addr.startsWith('sgl1') ? id_or_addr : previewAddressFromSeed(id_or_addr)
    return jsonRes({
      success: true,
      data: { id: `sgl-preview-${address.slice(4, 16)}`, address_formatted: address, address },
    })
  }
  // wallet balance by id_or_address
  const walletBalPath = path.match(/^\/api\/v1\/wallets\/([^\/]+)\/balance$/)
  if (walletBalPath) {
    const ref = walletBalPath[1]
    // WIRING: try the LIVE node first. Address is qnk<64hex> (or sgl1…/raw hex);
    // the node's /balance wants the bare 64-hex pubkey.
    const hex = ref.startsWith('qnk') ? ref.slice(3) : ref.startsWith('sgl1') ? ref.slice(4) : ref
    if (/^[0-9a-fA-F]{64}$/.test(hex)) {
      try {
        const r = await origFetch(`${LIVE_NODE}/api/v1/balance?wallet=${hex.toLowerCase()}`, { cache: 'no-store' })
        if (r.ok) {
          const j = await r.json()
          if (j && j.ok && typeof j.balance === 'number') {
            const sgl = String(j.balance)
            return jsonRes({
              data: { address: ref, balance_sgl: j.balance, balance_qnk: j.balance,
                      balance_raw: (BigInt(j.balance) * 10n ** 18n).toString(), tx_count: 0, source: 'live-node' },
              balance_sgl: sgl,
            })
          }
        }
      } catch { /* node unreachable → fall back to snapshot below */ }
    }
    const bal = snap.address_balances[ref] ?? { ...snap.default_balance, address: ref }
    // Dashboard reads response.data.balance_qnk (legacy field name); include both.
    return jsonRes({
      data: { ...bal, balance_qnk: parseFloat(bal.balance_sgl), balance_sgl: parseFloat(bal.balance_sgl) },
      balance_sgl: bal.balance_sgl,
    })
  }
  // multi-token balance — ONLY SGL + USDS shown (user-visible filter)
  if (path === '/api/v1/wallet/tokens' || path === '/api/v1/wallets/tokens') {
    return jsonRes({
      success: true,
      data: {
        SGL:  { balance: '100.00', symbol: 'SGL',  decimals: 18, name: 'SIGIL' },
        USDS: { balance: '0.00',   symbol: 'USDS', decimals: 6,  name: 'usdSIGIL' },
      },
    })
  }
  // Custom tokens / bridges / external chains — hide everything except SGL+USDS
  if (path.startsWith('/api/v1/custom-tokens') ||
      path.startsWith('/api/v1/custom_tokens') ||
      path.startsWith('/api/v1/tokens/custom') ||
      path.startsWith('/api/v1/bitcoin/') ||
      path.startsWith('/api/v1/ethereum/') ||
      path.startsWith('/api/v1/bridge/') ||
      path.startsWith('/api/v1/lp/') ||
      path.startsWith('/api/v1/contracts/forge/') ||
      path.startsWith('/api/v1/contracts/vault/')) {
    return jsonRes({ data: [], hidden: true, note: 'SIGIL preview: only SGL + USDS surfaced' })
  }
  // Token list / dex tokens — LIVE from sigil-rpcd /tokens (7 on-chain tokens)
  if (path === '/api/v1/dex/tokens' || path === '/api/v1/tokens' || path === '/api/v1/tokens/list') {
    const dex = await getLiveDex()
    if (dex && dex.tokens.length) {
      const prices: Record<string, number> = { USDS: 1.0, SIGIL: 0.42 }
      const data = dex.tokens.map((t: any) => ({
        symbol: t.symbol,
        name: t.symbol === 'USDS' ? 'usdSIGIL' : t.symbol,
        decimals: tokDecimals(t.symbol),
        id: t.id, address: t.id,
        total_supply: '0',
        balance: '0.00',
        price_usd: prices[t.symbol] ?? 1.0,
        source: 'live-node',
      }))
      return jsonRes({ data })
    }
    // fallback when the node is unreachable
    return jsonRes({
      data: [
        { symbol: 'SIGIL', name: 'SIGIL',    decimals: 18, balance: '0.00', price_usd: 0.42 },
        { symbol: 'USDS',  name: 'usdSIGIL', decimals: 6,  balance: '0.00', price_usd: 1.00 },
      ],
    })
  }
  if (path === '/api/v1/wallets' || path === '/api/v1/wallets/list') {
    return jsonRes({ data: [] })
  }
  // Liquidity pools — LIVE from sigil-rpcd /pools, mapped to the Quillon
  // DexScreen shape {pool_id, token0, token1, reserve0, reserve1}. Reserves
  // are 24-decimal base units (the UI divides by 1e24 for display).
  if (path === '/api/v1/liquidity/pools' ||
      path.startsWith('/api/v1/dex/pools') ||
      path.startsWith('/api/v1/pools')) {
    const dex = await getLiveDex()
    const pools = (dex?.pools ?? []).map((p: any) => ({
      pool_id: p.id,
      token0: p.sym_a, token1: p.sym_b,
      token_a: p.token_a, token_b: p.token_b,
      reserve0: scale24(p.reserve_a), reserve1: scale24(p.reserve_b),
      reserve_a: p.reserve_a, reserve_b: p.reserve_b,
      lp_shares: p.lp_shares, fee_bps: p.fee_bps, fee: (p.fee_bps ?? 30) / 10000,
      label: p.label, source: 'live-node',
    }))
    return jsonRes({ data: pools, pools })
  }
  // dex / contracts / RWA / groups — every other list-style endpoint
  if (path.startsWith('/api/v1/dex/')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/contracts')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/rwa')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/groups')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/messages')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/bounties')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/tokens')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/loans')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/miners')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/mining/')) return jsonRes({ data: [] })

  // status / network — return both the raw shape AND a `data` envelope so
  // every form the Quillon Dashboard expects gets satisfied. The height is
  // computed live from the SIGIL g0 genesis epoch so the TopBar block-height
  // pill actually ticks up while the page stays open.
  if (path === '/api/v1/status') {
    const launchMs = 1_780_137_000_000 // 2026-05-30 09:50 UTC
    const liveHeight = Math.max(1, Math.floor((Date.now() - launchMs) / 12_000))
    const peers = snap.status.peers || 2
    const common = {
      ...snap.status,
      height: liveHeight,
      current_height: liveHeight,
      connected_peers: peers,
      peer_count: peers,
      dag_round: liveHeight,
      balance: 100,
      balance_qnk: 100,
      balance_sgl: 100,
      network_health: 'good',
      consensus_status: 'sigil-preview',
      tps_current: 0,
      tps_average: 0,
      uptime_formatted: '∞',
    }
    return jsonRes({ ...common, data: common })
  }
  // k-parameter gauge endpoint (TopBar fetches every 60s for the health dial)
  if (path === '/api/v1/k-parameter') {
    const data = {
      k_value: 0.62,
      k_enhanced: 0.68,
      phase: 'stable',
      target: 0.55,
      tolerance: 0.10,
      note: 'sigil-preview k-parameter — static; live oracle in Phase D',
    }
    return jsonRes({ ...data, data })
  }
  // health checks at every level
  if (path === '/api/v1/health' || path === '/api/v1/p2p/health') {
    return jsonRes({
      healthy: true, status: 'ok',
      data: { healthy: true, p2p_running: true, peer_count: snap.status.peers, bootstrap_peer_configured: true, gossipsub_topics: ['/sigil/g0/blocks', '/sigil/g0/peer-heights'] },
    })
  }
  // node info — Dashboard uses for hostname/version display
  if (path === '/api/v1/node/info' || path === '/api/v1/admin/node/info') {
    return jsonRes({
      data: {
        hostname: 'sigil-preview', version: snap.status.version,
        network_id: snap.status.network_id, height: snap.status.height,
        peers: snap.status.peers, uptime_secs: 0,
      },
    })
  }
  if (path === '/api/v1/admin/decentralization') return jsonRes({ enabled: false, note: snap.status.note })

  // blocks — `data` must be the array for `.filter` / `.map` callers
  if (path === '/api/v1/blocks' || path === '/api/v1/blocks/recent') {
    return jsonRes({ data: snap.recent_blocks, blocks: snap.recent_blocks, total: snap.recent_blocks.length })
  }
  const bH = path.match(/^\/api\/v1\/blocks\/(\d+)$/)
  if (bH) {
    const h = parseInt(bH[1], 10)
    const blk = snap.recent_blocks.find((b: any) => b.height === h)
    return blk
      ? jsonRes(blk)
      : jsonRes({ error: 'not found', height: h, note: 'block outside SIGIL preview window' }, 404)
  }

  // balance — the headline feature: dashboard MUST show a number
  const balPath =
    path.match(/^\/api\/v1\/addresses\/([^\/?]+)\/balance(?:\?.*)?$/) ||
    path.match(/^\/api\/v1\/balance\/([^\/?]+)$/)
  if (balPath) {
    const addr = balPath[1]
    return jsonRes(
      snap.address_balances[addr] ??
        { ...snap.default_balance, address: addr, note: 'SIGIL preview balance' },
    )
  }
  // bare addresses lookup (full account)
  const acctPath = path.match(/^\/api\/v1\/addresses\/([^\/?]+)\/?$/)
  if (acctPath) {
    const addr = acctPath[1]
    const bal = snap.address_balances[addr] ?? { ...snap.default_balance, address: addr }
    return jsonRes({ address: addr, balance: bal, transactions: [], pending: [] })
  }

  // peers / mempool / address book / transactions — wallet code calls
  // `response.data.filter(...)` / `.map(...)`, so `data` MUST be an array.
  if (path === '/api/v1/peers') return jsonRes({ data: [], count: snap.status.peers })
  if (path.startsWith('/api/v1/mempool')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/transactions')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/addressbook')) return jsonRes({ data: [] })
  if (path.startsWith('/api/v1/admin/is')) return jsonRes({ is_admin: false, data: { is_admin: false } })
  if (path.startsWith('/api/v1/admin/')) return jsonRes({ data: [], note: 'admin disabled in SIGIL preview' })

  // any /api/v1/ai/* → polite refusal so the AI panels show "offline"
  if (path.startsWith('/api/v1/ai/')) {
    return jsonRes({ error: 'AI features are offline in SIGIL preview' }, 503)
  }

  // default: when in doubt, return an empty array under `data` — most callers
  // do .filter / .map / .length on it. Object callers see `.length === 0`
  // and skip; that's strictly safer than an object that has no .filter.
  return jsonRes({ data: [], note: 'SIGIL preview shim — endpoint stubbed', path })
}

export function installSigilApiShim() {
  if ((window as any).__SIGIL_SHIM__) return
  ;(window as any).__SIGIL_SHIM__ = true

  // Clear stale failover state that could bypass the shim by pointing the
  // base URL at sigilgraph.com (DNS exists but not pointed at us) or a
  // direct host:port that's firewalled.
  try {
    const stored = localStorage.getItem('apiBaseURL')
    if (stored && (stored.includes('sigilgraph.com') || /:\d+$/.test(stored))) {
      localStorage.removeItem('apiBaseURL')
      localStorage.removeItem('failoverServer')
    }
  } catch {
    /* ignore */
  }

  // fetch interceptor
  window.fetch = (async (input: any, init?: RequestInit): Promise<Response> => {
    const url = typeof input === 'string' ? input : (input?.url ?? '')
    let pathname = url
    try {
      pathname = new URL(url, window.location.href).pathname
    } catch {
      /* keep raw */
    }
    if (!pathname.startsWith('/api/v1/')) {
      return origFetch(input, init)
    }
    return handleApi(pathname, init)
  }) as typeof fetch

  // EventSource interceptor: API SSE feeds → quiet stub
  const W = window as any
  if (W.EventSource) {
    const OrigES = W.EventSource
    W.EventSource = function (url: string, opts?: EventSourceInit) {
      if (typeof url === 'string' && url.startsWith('/api/')) {
        const stub: any = {
          url, readyState: 0, withCredentials: !!opts?.withCredentials,
          onopen: null, onmessage: null, onerror: null,
          close() { this.readyState = 2 },
          addEventListener() {},
          removeEventListener() {},
          dispatchEvent: () => true,
        }
        setTimeout(() => {
          stub.readyState = 1
          if (typeof stub.onopen === 'function') stub.onopen(new Event('open'))
        }, 50)
        return stub
      }
      return new OrigES(url, opts)
    }
  }

  // ── flux-error · window.error + unhandledrejection capture ──
  // Captures runtime errors and pushes them to (a) console with a [SIGIL-ERR]
  // prefix, (b) window.__SIGIL_ERRORS__ array, (c) localStorage `sigil:errors`
  // (capped at 50), and (d) navigator.sendBeacon to /sigil-error?... so the
  // q-flux access log captures the message + stack tail. Errors are also
  // POSTed to /api/v1/flux/error which my shim intercepts + mirrors so any
  // future panel can fetch them.
  const ERR_CAP = 50
  function pushError(e: any) {
    const rec = {
      ts_ms: Date.now(),
      message: String(e?.message ?? e?.reason?.message ?? e),
      stack: String(e?.error?.stack ?? e?.reason?.stack ?? e?.stack ?? ''),
      filename: String(e?.filename ?? ''),
      lineno: e?.lineno ?? 0,
      colno: e?.colno ?? 0,
      type: String(e?.type ?? 'error'),
      url: window.location.href,
    }
    const W = window as any
    W.__SIGIL_ERRORS__ = W.__SIGIL_ERRORS__ || []
    W.__SIGIL_ERRORS__.push(rec)
    if (W.__SIGIL_ERRORS__.length > ERR_CAP) W.__SIGIL_ERRORS__.shift()
    // localStorage mirror — cheap inspection from devtools or sigil-errors.html
    try {
      const arr = JSON.parse(localStorage.getItem('sigil:errors') || '[]')
      arr.push(rec)
      while (arr.length > ERR_CAP) arr.shift()
      localStorage.setItem('sigil:errors', JSON.stringify(arr))
    } catch { /* quota */ }
    // sendBeacon → q-flux access.log capture (URL-encoded msg + stack tail)
    try {
      const stackTail = rec.stack.split('\n').slice(0, 3).join(' | ').slice(0, 240)
      const u = `/sigil-error-log?msg=${encodeURIComponent(rec.message)}&at=${encodeURIComponent(rec.filename + ':' + rec.lineno + ':' + rec.colno)}&stack=${encodeURIComponent(stackTail)}&t=${rec.ts_ms}`
      navigator.sendBeacon?.(u)
    } catch { /* CSP */ }
    // Visible console marker
    // eslint-disable-next-line no-console
    console.error('[SIGIL-ERR]', rec.message, '@', rec.filename + ':' + rec.lineno + ':' + rec.colno, '\n', rec.stack.split('\n').slice(0, 6).join('\n'))
  }
  window.addEventListener('error', pushError)
  window.addEventListener('unhandledrejection', pushError)

  useSigilStore.getState().markShimInstalled()
  // Prime the snapshot so the first render already has data
  void getSnapshot()
  // eslint-disable-next-line no-console
  console.log('🌌 SIGIL apiShim installed — Quillon backend isolated · flux-error capture armed')
}
