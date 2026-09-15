// main.ts — boot, state, polling, events. One store, one render pass per
// section, no framework: the page is ~1,500 lines and a node poll every 10 s.
import './style.css'
import { api, fmt, NATIVE_TOKEN } from './api'
import { buildSnapshot, quote, type Snapshot } from './model'
import * as ui from './ui'
import { I } from './icons'

const $ = <T extends HTMLElement = HTMLElement>(sel: string): T => document.querySelector(sel) as T
const app = $('#root')
app.innerHTML = ui.shell()

// ── state ─────────────────────────────────────────────────────────────────
let snap: Snapshot | null = null
let heroIdx = 0
let heroTimer: number | undefined
let wallet: string | null = null
const balances: Record<string, number> = {}
let trendMode: 'trending' | 'top' = 'trending'
let trendWin = '24h'
let panelTab: 'tokens' | 'nfts' | 'activity' = 'tokens'
const swapSt: ui.SwapState = { mode: 'market', from: NATIVE_TOKEN, to: '', amount: '', limitPrice: '', limitDir: 'buy', slippage: 0.5 }
const tokSt: ui.TokenTableState = { q: '', filter: 'all', sort: 'supply', dir: 'desc', open: null }
let collapsed = false
let cat = 'all'

try { wallet = localStorage.getItem('sigil-wallet-address') } catch { /* blocked */ }
// theme: saved choice, else the OS preference; the toggle flips and persists
function applyTheme(t: 'dark' | 'light'): void { document.documentElement.setAttribute('data-theme', t); const b = document.getElementById('themeBtn'); if (b) b.innerHTML = t === 'dark' ? I.sun : I.moon }
let theme: 'dark' | 'light' = 'dark'
try { const saved = localStorage.getItem('sigilvm-theme'); theme = saved === 'light' || saved === 'dark' ? saved : (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark') } catch { /* */ }
applyTheme(theme)
try { if (localStorage.getItem('sigilvm-panel') === 'open') app.classList.add('panel-open') } catch { /* */ }
if (innerWidth >= 1600 && !app.classList.contains('panel-open')) app.classList.add('panel-open')

function toast(msg: string, cls = ''): void {
  const t = document.createElement('div'); t.className = 'toast ' + cls; t.textContent = msg
  $('#toasts').appendChild(t); setTimeout(() => t.remove(), 4200)
}

// ── render passes ─────────────────────────────────────────────────────────
let heroHover = false
function renderHero(): void {
  if (!snap) return
  $('#hero').innerHTML = ui.hero(snap, heroIdx)
  swapWithFlash($('#strip'), ui.strip(snap))
  swapWithFlash($('#foryou'), ui.forYou(snap))
  clearTimeout(heroTimer)
  heroTimer = window.setTimeout(() => { if (!heroHover) heroIdx++; renderHero() }, 7000)
}
// OpenSea flashes a value that changed between polls; we diff the rendered text per cell.
function swapWithFlash(host: HTMLElement, html: string): void {
  const before = new Map<string, string>()
  host.querySelectorAll<HTMLElement>('.v, .num, .delta, .price, .r, .fy-col p').forEach((el, i) => before.set(String(i), el.textContent || ''))
  host.innerHTML = html
  if (!before.size) return
  const after = host.querySelectorAll<HTMLElement>('.v, .num, .delta, .price, .r, .fy-col p')
  if (after.length !== before.size) return // the list changed shape (filter/sort) — not a value change
  after.forEach((el, i) => {
    const prev = before.get(String(i))
    if (prev !== undefined && prev !== (el.textContent || '')) { el.classList.add('flash'); setTimeout(() => el.classList.remove('flash'), 1400) }
  })
}
function renderRows(): void {
  if (!snap) return
  const feat = cat === 'all' ? snap.featured : snap.collections.filter((c) => c.cat === cat)
  $('#featuredRow').classList.toggle('grid', cat !== 'all')
  swapWithFlash($('#featuredRow'), feat.map(ui.collectionCard).join('') || '<div class="pempty">No collections in this category.</div>')
  const tt = $('#tickerTrack'); const th = ui.ticker(snap); if (tt.innerHTML !== th) tt.innerHTML = th
  $('#dropsRow').innerHTML = snap.drops.map(ui.dropCard).join('')
  swapWithFlash($('#moversRow'), snap.movers.map(ui.moverCard).join('') || '<div class="pempty">No miners read — node offline?</div>')
  $('#salesRow').innerHTML = snap.sales.map(ui.saleCard).join('') || '<div class="pempty">Nothing settled this week that the node reports.</div>'
  swapWithFlash($('#trendingTable'), ui.trending(snap, trendMode, trendWin))
  $('#legend').innerHTML = ui.legend(snap)
  $('#footTs').textContent = `poll ${new Date(snap.at).toLocaleTimeString()} · height ${fmt.int(snap.head.height)}`
}
function renderChain(): void {
  if (!snap) return
  const chip = $('#chainChip'); const d = chip.querySelector('.d') as HTMLElement
  d.classList.toggle('on', snap.head.ok)
  $('#chainHeight').textContent = snap.head.ok ? fmt.int(snap.head.height) : 'offline'
  $('#walletLbl').textContent = wallet ? fmt.short(wallet, 5) : 'Connect wallet'
}
function currentQuote() {
  if (!snap) return null
  const from = snap.tokens.find((t) => t.id === swapSt.from), to = snap.tokens.find((t) => t.id === swapSt.to)
  const amt = Number(swapSt.amount) || 0
  if (!from || !to || amt <= 0) return null
  return quote(snap.pools, from.id, to.id, amt, from.decimals, to.decimals)
}
function renderSwap(): void {
  if (!snap) return
  if (!swapSt.to || !snap.tokens.some((t) => t.id === swapSt.to)) swapSt.to = (snap.tokens.find((t) => t.id !== swapSt.from) || snap.tokens[0]).id
  const host = $('#swap')
  if (collapsed) {
    host.innerHTML = `<div class="qcard"><div class="inner" style="width:44px;padding:10px 6px;display:flex;flex-direction:column;align-items:center;gap:10px;cursor:pointer;min-height:160px" id="swapExpand">${I.right}<span style="writing-mode:vertical-lr;transform:rotate(180deg);font-size:10px;letter-spacing:.2em;font-weight:700">SWAP</span></div></div>`
    host.style.width = '60px'
    return
  }
  host.style.width = ''
  const active = document.activeElement as HTMLInputElement | null
  const keep = active && active.id === 'swapAmt' ? { s: active.selectionStart, e: active.selectionEnd } : null
  host.innerHTML = ui.swapModule(snap, swapSt, balances, currentQuote()) + ui.routeCard(snap, swapSt)
  if (keep) { const el = $('#swapAmt') as HTMLInputElement; el.focus(); try { el.setSelectionRange(keep.s ?? 0, keep.e ?? 0) } catch { /* */ } }
}
function renderTokens(): void { if (snap) $('#tokens').innerHTML = ui.tokenTable(snap, tokSt) }
function renderPanel(): void {
  if (!snap) return
  $('#panelHead').innerHTML = ui.panelHead(wallet, snap, balances)
  $('#nTok').textContent = wallet ? String(snap.tokens.length) : ''
  const body = $('#panelBody')
  body.innerHTML = panelTab === 'tokens' ? ui.panelTokens(wallet, snap, balances) : panelTab === 'nfts' ? ui.panelNfts(wallet, snap) : ui.panelActivity(snap)
  $('#nNft').textContent = wallet ? String(body.querySelectorAll('.pnft').length || '') : ''
  document.querySelectorAll('#panelTabs button').forEach((b) => b.classList.toggle('on', (b as HTMLElement).dataset.ptab === panelTab))
}
function renderAll(): void {
  renderChain()
  if (snap && snap.offline) { showOffline(true); return } // keep the skeletons; zeros would read as measurements
  showOffline(false)
  renderHero(); renderRows(); renderSwap(); renderTokens(); renderPanel()
}
function showOffline(on: boolean): void {
  let el = document.getElementById('offline')
  if (on) {
    if (!el) { el = document.createElement('div'); el.id = 'offline'; el.className = 'offline'; $('#main').prepend(el) }
    el.innerHTML = `<span class="chip bad"><i class="d"></i>node unreachable</span><span>sigil-api did not answer. Nothing on this page is a measurement until it does — retrying every 10 s.</span><button class="btn ghost sm" id="retryNow">Retry now</button><span class="muted mono" style="font-size:11px">or add <code>?api=http://host:18181</code></span>`
  } else if (el) el.remove()
}

// ── data ──────────────────────────────────────────────────────────────────
async function loadBalances(): Promise<void> {
  if (!wallet || !snap) return
  await Promise.all(snap.tokens.filter((t) => t.status === 'live' || t.status === 'gated').map(async (t) => {
    const b = await api.balance(wallet!, t.id)
    if (b) balances[t.id] = Number(b.balance) / 10 ** t.decimals
  }))
}
let polling = false
async function poll(): Promise<void> {
  if (polling) return
  polling = true
  try {
    const prevHeight = snap?.head.height ?? 0
    snap = await buildSnapshot()
    await loadBalances()
    renderAll()
    if (!snap.offline && prevHeight && snap.head.height > prevHeight) { const b = $('#bellBadge'); b.hidden = false }
  } catch (e) {
    toast('poll failed: ' + (e as Error).message, 'bad')
  } finally { polling = false }
}

// ── events (delegated) ────────────────────────────────────────────────────
document.addEventListener('click', (ev) => {
  const t = ev.target as HTMLElement
  if (t.id === 'modal') { closeModal(); return }
  const btn = t.closest('button, a, tr, [data-hero]') as HTMLElement | null
  if (!btn) return
  const ds = btn.dataset
  if (ds.hero !== undefined) { heroIdx = Number(ds.hero); renderHero(); return }
  if (btn.id === 'chainChip') { $('#chainMenu').classList.toggle('open'); return }
  if (btn.id === 'retryNow') { poll(); return }
  if (btn.id === 'themeBtn') { theme = theme === 'dark' ? 'light' : 'dark'; applyTheme(theme); try { localStorage.setItem('sigilvm-theme', theme) } catch { /* */ } return }
  if (btn.id === 'panelBtn') { app.classList.toggle('panel-open'); try { localStorage.setItem('sigilvm-panel', app.classList.contains('panel-open') ? 'open' : 'closed') } catch { /* */ } return }
  if (btn.id === 'bnWallet') { ev.preventDefault(); app.classList.toggle('panel-open'); renderPanel(); return }
  if (btn.id === 'walletBtn') { if (wallet) { app.classList.add('panel-open'); return } openWalletModal(); return }
  if (btn.id === 'pDisconnect') { wallet = null; try { localStorage.removeItem('sigilvm-wallet') } catch { /* */ } for (const k in balances) delete balances[k]; renderChain(); renderPanel(); renderSwap(); return }
  if (btn.id === 'bellBtn') { $('#bellBadge').hidden = true; panelTab = 'activity'; app.classList.add('panel-open'); renderPanel(); return }
  if (btn.id === 'cartBtn') { toast('The cart lights up when listings exist on SIGIL VM — none do yet.', 'warn'); return }
  if (ds.ptab) { panelTab = ds.ptab as typeof panelTab; renderPanel(); return }
  if (btn.id === 'pSend' || btn.id === 'pReceive') { window.open('/sigil-wallet-tron-embedded.html', '_blank', 'noopener'); return }
  if (btn.id === 'pSwap') { location.hash = '#swap'; $('#dex').scrollIntoView({ behavior: 'smooth' }); return }
  if (ds.mode && btn.closest('#trendMode')) { trendMode = ds.mode as typeof trendMode; document.querySelectorAll('#trendMode button').forEach((b) => b.classList.toggle('on', b === btn)); if (snap) $('#trendingTable').innerHTML = ui.trending(snap, trendMode, trendWin); return }
  if (ds.cat) { cat = ds.cat; document.querySelectorAll('#cats button').forEach((b) => b.classList.toggle('on', b === btn)); renderRows(); $('#featured').scrollIntoView({ behavior: 'smooth', block: 'start' }); return }
  if (ds.win) { trendWin = ds.win; document.querySelectorAll('#trendWin button').forEach((b) => b.classList.toggle('on', b === btn)); if (snap) $('#trendingTable').innerHTML = ui.trending(snap, trendMode, trendWin); return }
  if (ds.copy) { ev.preventDefault(); navigator.clipboard?.writeText(ds.copy).then(() => toast('Proof copied.')).catch(() => toast('Clipboard blocked.', 'warn')); return }
  if (ds.coll && !ev.ctrlKey && !ev.metaKey && !(ev as MouseEvent).button) { ev.preventDefault(); openCollection(ds.coll); return }
  const arrows = btn.closest('.arrows') as HTMLElement | null
  if (arrows && btn.tagName === 'BUTTON') { const row = $('#' + arrows.dataset.scroll); const dir = Array.from(arrows.children).indexOf(btn) === 0 ? -1 : 1; row.scrollBy({ left: dir * (row.clientWidth * 0.8), behavior: 'smooth' }); return }
  // swap module
  if (ds.mode && btn.closest('.seg')) { swapSt.mode = ds.mode as 'market' | 'limit'; renderSwap(); return }
  if (ds.sel) { openTokenPicker(ds.sel as 'from' | 'to'); return }
  if (ds.max) { const b = balances[swapSt.from] ?? 0; swapSt.amount = b ? b.toFixed(8) : ''; renderSwap(); return }
  if (ds.pct) { const b = balances[swapSt.from] ?? 0; swapSt.amount = (b * Number(ds.pct) / 100).toFixed(8); renderSwap(); return }
  if (btn.id === 'swapFlip') { const f = swapSt.from; swapSt.from = swapSt.to; swapSt.to = f; renderSwap(); return }
  if (ds.dir) { swapSt.limitDir = ds.dir as 'buy' | 'sell'; renderSwap(); return }
  if (btn.id === 'swapCollapse') { collapsed = true; renderSwap(); return }
  if (btn.id === 'swapExpand') { collapsed = false; renderSwap(); return }
  if (btn.id === 'swapSettings') { openSlippage(); return }
  if (btn.id === 'swapGo') { if (snap) { const from = snap.tokens.find((x) => x.id === swapSt.from)!; const to = snap.tokens.find((x) => x.id === swapSt.to)!; window.open(`/sigil-wallet-tron-embedded.html#swap=${from.symbol}:${to.symbol}:${swapSt.amount}`, '_blank', 'noopener'); toast('Handing the swap to your wallet to sign.') } return }
  // token table
  if (ds.filter) { tokSt.filter = ds.filter as typeof tokSt.filter; renderTokens(); return }
  if (ds.swap) { swapSt.to = ds.swap; if (swapSt.from === ds.swap) swapSt.from = NATIVE_TOKEN === ds.swap ? snap!.tokens[1].id : NATIVE_TOKEN; renderSwap(); $('#dex').scrollIntoView({ behavior: 'smooth' }); return }
  if (ds.info) { tokSt.open = tokSt.open === ds.info ? null : ds.info; renderTokens(); return }
  const trow = btn.closest('tr[data-tok]') as HTMLElement | null
  if (trow && !ds.swap) { tokSt.open = tokSt.open === trow.dataset.tok ? null : trow.dataset.tok!; renderTokens(); return }
  const th = btn.closest('th[data-sort]') as HTMLElement | null
  if (th) { const k = th.dataset.sort as ui.TokenTableState['sort']; if (tokSt.sort === k) tokSt.dir = tokSt.dir === 'asc' ? 'desc' : 'asc'; else { tokSt.sort = k; tokSt.dir = 'desc' } renderTokens(); return }
  if (btn.id === 'modalClose' || btn.id === 'modalClose2' || btn.closest('#modal') === btn) { closeModal(); return }
  if (ds.pick && ds.which) { if (ds.which === 'from') { if (swapSt.to === ds.pick) swapSt.to = swapSt.from; swapSt.from = ds.pick } else { if (swapSt.from === ds.pick) swapSt.from = swapSt.to; swapSt.to = ds.pick } closeModal(); renderSwap(); return }
  if (ds.slip) { swapSt.slippage = Number(ds.slip); closeModal(); renderSwap(); return }
  if (btn.id === 'walletUse') { const v = ($('#walletIn') as HTMLInputElement).value.trim().toLowerCase(); if (!/^[0-9a-f]{64}$/.test(v)) { toast('A SIGIL wallet id is 64 hex characters.', 'warn'); return } wallet = v; try { localStorage.setItem('sigilvm-wallet', v) } catch { /* */ } closeModal(); app.classList.add('panel-open'); poll(); return }
  const nav = btn.closest('[data-nav]') as HTMLElement | null
  if (nav) { document.querySelectorAll('[data-nav]').forEach((a) => a.classList.toggle('on', (a as HTMLElement).dataset.nav === nav.dataset.nav)) }
})
document.addEventListener('input', (ev) => {
  const t = ev.target as HTMLInputElement
  if (t.id === 'swapAmt') { swapSt.amount = t.value; renderSwap(); return }
  if (t.id === 'swapRange') { const b = balances[swapSt.from] ?? 0; swapSt.amount = (b * Number(t.value) / 100).toFixed(8); renderSwap(); return }
  if (t.id === 'limitPx') { swapSt.limitPrice = t.value; return }
  if (t.id === 'tokQ') { tokSt.q = t.value; const box = $('#tokens .ttable tbody'); if (snap) { box.innerHTML = (new DOMParser().parseFromString(ui.tokenTable(snap, tokSt), 'text/html').querySelector('tbody') as HTMLElement).innerHTML } return }
  if (t.id === 'q') { globalSearch(t.value) }
})
document.addEventListener('keydown', (ev) => {
  if (ev.key === '/' && document.activeElement?.tagName !== 'INPUT') { ev.preventDefault(); ($('#q') as HTMLInputElement).focus() }
  if (ev.key === 'Escape') { closeModal(); $('#qres').classList.remove('open'); $('#chainMenu').classList.remove('open') }
  const list = document.querySelector('#modal.open .list') as HTMLElement | null
  if (list && (ev.key === 'ArrowDown' || ev.key === 'ArrowUp' || ev.key === 'Enter')) {
    const items = Array.from(list.querySelectorAll<HTMLButtonElement>('button'))
    const i = items.indexOf(document.activeElement as HTMLButtonElement)
    if (ev.key === 'Enter' && i >= 0) return // native click
    ev.preventDefault()
    const n = ev.key === 'ArrowDown' ? Math.min(items.length - 1, i + 1) : Math.max(0, i - 1)
    items[n]?.focus()
  }
})
document.addEventListener('click', (ev) => { const t = ev.target as HTMLElement; if (!t.closest('.search')) $('#qres').classList.remove('open'); if (!t.closest('.chain-wrap')) $('#chainMenu').classList.remove('open') })

// ── modals ────────────────────────────────────────────────────────────────
function openModal(html: string, cls = ''): void { const box = $('#modalBox'); box.className = 'box ' + cls; box.innerHTML = (cls ? '' : `<button class="ibtn x" id="modalClose">${I.x}</button>`) + html; $('#modal').classList.add('open') }
function closeModal(): void { $('#modal').classList.remove('open') }
function openCollection(id: string): void { const c = snap?.collections.find((x) => x.id === id); if (c) openModal(ui.collectionModal(c, snap!), 'coll') }
function openTokenPicker(which: 'from' | 'to'): void {
  if (!snap) return
  openModal(`<h4>Select a token <span class="muted" style="font-size:11px;font-weight:500">↑↓ Enter</span></h4><div class="list">${snap.tokens.map((t) => `<button data-pick="${t.id}" data-which="${which}"><img src="${t.icon}" alt=""><div><div class="s">${t.symbol}</div><div class="n">${ui.esc(t.name)}</div></div><span class="r">${balances[t.id] === undefined ? '' : fmt.num(balances[t.id], 4)}</span></button>`).join('')}</div>`)
}
function openSlippage(): void {
  openModal(`<h4>Slippage tolerance</h4><div class="quick">${[0.1, 0.5, 1, 3].map((s) => `<button data-slip="${s}" class="${swapSt.slippage === s ? 'on' : ''}">${s}%</button>`).join('')}</div><p class="muted" style="font-size:12px">Applied by the wallet when it builds the proof. The desk shows the number; it does not sign.</p>`)
}
export function openTokenInfo(id: string): void {
  const t = snap?.tokens.find((x) => x.id === id); if (!t) return
  openModal(`<h4>${t.symbol} — ${ui.esc(t.name)}</h4><pre>${ui.esc(JSON.stringify({ token_id: t.id, decimals: t.decimals, status: t.status, note: t.statusNote, supply: t.supply, holders: t.holders, provenance: t.provenance }, null, 2))}</pre><a class="btn ghost" href="/api.html" target="_blank" rel="noopener">${I.ext} API reference</a>`)
}
function openWalletModal(): void {
  let gate: string | null = null
  try { gate = localStorage.getItem('sigil-wallet-address') } catch { /* */ }
  openModal(`<h4>Connect a SIGIL wallet</h4>
    <p class="muted" style="font-size:12.5px">Read-only. Paste a 64-hex wallet id to see its balances and on-chain records. Signing stays in the wallet app.</p>
    <div class="amt"><input id="walletIn" placeholder="64-hex wallet id" value="${gate ?? ''}" spellcheck="false"></div>
    <div style="display:flex;gap:8px;margin-top:12px"><button class="btn primary" id="walletUse">Use this wallet</button><a class="btn ghost" href="/enter-sigil.html" target="_blank" rel="noopener">${I.ext} Open the gate</a></div>
    ${gate ? '<p class="muted" style="font-size:11.5px;margin-top:10px">Prefilled from the gate on this origin.</p>' : ''}`)
}

// ── global search ─────────────────────────────────────────────────────────
function globalSearch(q: string): void {
  const box = $('#qres')
  q = q.trim().toLowerCase()
  if (!q || !snap) { box.classList.remove('open'); return }
  const rows: string[] = []
  const cs = snap.collections.filter((c) => c.name.toLowerCase().includes(q) || c.blurb.toLowerCase().includes(q)).slice(0, 4)
  if (cs.length) rows.push('<div class="h">Collections</div>' + cs.map((c) => `<a class="r" href="${c.link}" target="_blank" rel="noopener"><img src="${c.cover}" alt=""><div><div class="n">${ui.esc(c.name)}</div><div class="s">${c.items === null ? '—' : fmt.int(c.items)} items</div></div></a>`).join(''))
  const ts = snap.tokens.filter((t) => (t.symbol + t.name).toLowerCase().includes(q)).slice(0, 4)
  if (ts.length) rows.push('<div class="h">Tokens</div>' + ts.map((t) => `<div class="r" data-swap="${t.id}"><img src="${t.icon}" alt=""><div><div class="n">${t.symbol}</div><div class="s">${ui.esc(t.statusNote)}</div></div></div>`).join(''))
  const ms = (snap.miners?.miners ?? []).filter((m) => (m.rig + m.wallet).toLowerCase().includes(q)).slice(0, 4)
  if (ms.length) rows.push('<div class="h">Rigs</div>' + ms.map((m) => `<div class="r"><img src="${snap!.collections[0].cover}" alt=""><div><div class="n">${ui.esc(m.rig)}</div><div class="s">${fmt.hps(m.hash_rate)} · ${fmt.short(m.wallet, 6)}</div></div></div>`).join(''))
  if (/^[0-9a-f]{64}$/.test(q)) rows.push(`<div class="h">Wallet</div><div class="r" id="qWallet"><div><div class="n mono">${fmt.short(q, 8)}</div><div class="s">open in panel</div></div></div>`)
  box.innerHTML = rows.join('') || '<div class="h">No matches on this node</div>'
  box.classList.add('open')
  const qw = box.querySelector('#qWallet'); if (qw) qw.addEventListener('click', () => { wallet = q; app.classList.add('panel-open'); box.classList.remove('open'); poll() })
}

$('#hero').addEventListener('mouseenter', () => { heroHover = true })
$('#hero').addEventListener('mouseleave', () => { heroHover = false })

// ── boot ──────────────────────────────────────────────────────────────────
try { const w = localStorage.getItem('sigilvm-wallet'); if (w) wallet = w } catch { /* */ }
try { const qw = new URLSearchParams(location.search).get('wallet'); if (qw && /^[0-9a-f]{64}$/i.test(qw)) { wallet = qw.toLowerCase(); app.classList.add('panel-open') } } catch { /* */ }
poll()
setInterval(poll, 10_000)
document.querySelectorAll('.row').forEach((r) => r.addEventListener('wheel', (e) => { const we = e as WheelEvent; if (Math.abs(we.deltaY) > Math.abs(we.deltaX)) { (r as HTMLElement).scrollLeft += we.deltaY; e.preventDefault() } }, { passive: false }))
