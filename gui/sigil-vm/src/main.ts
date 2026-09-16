// main.ts — boot, state, polling, events. One store, one render pass per
// section, no framework: the page is ~1,500 lines and a node poll every 10 s.
import './style.css'
import { api, fmt, NATIVE_TOKEN } from './api'
import { buildSnapshot, quote, type Snapshot } from './model'
import * as ui from './ui'
import { I } from './icons'
import { avatarSvg } from './art'

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

try { if (new URLSearchParams(location.search).get('embed')) app.classList.add('embed') } catch { /* */ }
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
  // type=number inputs report selectionStart as null, so remember the caret as 'end of value' and restore it there
  const active = document.activeElement as HTMLInputElement | null
  const keepFocus = !!active && (active.id === 'swapAmt' || active.id === 'limitPx')
  const keepId = active?.id
  host.innerHTML = ui.swapModule(snap, swapSt, balances, currentQuote(), wallet) + ui.routeCard(snap, swapSt)
  if (keepFocus && keepId) { const el = document.getElementById(keepId) as HTMLInputElement | null; if (el) { el.focus(); const v = el.value; el.value = ''; el.value = v } }
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
let openCollId: string | null = null
function renderAll(): void {
  renderChain()
  if (snap && snap.offline) { showOffline(true); return } // keep the skeletons; zeros would read as measurements
  showOffline(false)
  renderHero(); renderRows(); renderSwap(); renderTokens(); renderPanel()
  // an open collection page refreshes its stats and the active pane in place (keeps tab + scroll)
  if (openCollId && $('#modal').classList.contains('open')) {
    const c = snap!.collections.find((x) => x.id === openCollId)
    if (c) {
      const box = $('#modalBox'); const activeTab = (box.querySelector('.cm-tabs button.on') as HTMLElement | null)?.dataset.cmtab || 'items'
      const scroll = (box.querySelector('.cm-body') as HTMLElement | null)?.scrollTop ?? 0
      const fresh = new DOMParser().parseFromString(`<div>${ui.collectionModal(c, snap!)}</div>`, 'text/html')
      const stats = fresh.querySelector('.cm-stats'); if (stats) box.querySelector('.cm-stats')!.innerHTML = stats.innerHTML
      const pane = fresh.querySelector(`.cm-pane[data-pane="${activeTab}"]`); const cur = box.querySelector(`.cm-pane[data-pane="${activeTab}"]`)
      if (pane && cur && cur.innerHTML !== pane.innerHTML) { cur.innerHTML = pane.innerHTML; (box.querySelector('.cm-body') as HTMLElement).scrollTop = scroll }
      const n = fresh.querySelector('.cm-tabs button[data-cmtab="items"] .n'); const curN = box.querySelector('.cm-tabs button[data-cmtab="items"] .n'); if (n && curN) curN.textContent = n.textContent
    }
  }
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
    const firstPaint = !snap
    snap = await buildSnapshot()
    await loadBalances()
    renderAll()
    // a deep link (#dex, #trending…) scrolled before the live content existed — re-anchor once the page has its real height
    if (firstPaint && location.hash && !snap.offline) { const target = document.querySelector(location.hash.split('=')[0]); if (target) requestAnimationFrame(() => target.scrollIntoView({ behavior: 'instant', block: 'start' })) }
    if (!snap.offline && prevHeight && snap.head.height > prevHeight) { const b = $('#bellBadge'); b.hidden = false }
  } catch (e) {
    toast('poll failed: ' + (e as Error).message, 'bad')
  } finally { polling = false }
}

// ── events (delegated) ────────────────────────────────────────────────────
document.addEventListener('click', (ev) => {
  const t = ev.target as HTMLElement
  if (t.id === 'modal') { closeModal(); return }
  const btn = t.closest('button, a, tr, [data-hero], [data-coll], [data-swap], [data-copy]') as HTMLElement | null
  if (!btn) return
  const ds = btn.dataset
  if (ds.hero !== undefined) { heroIdx = Number(ds.hero); renderHero(); return }
  if (btn.id === 'chainChip') { $('#chainMenu').classList.toggle('open'); return }
  if (btn.id === 'retryNow') { poll(); return }
  if (btn.id === 'footKeys') { openShortcuts(); return }
  if (btn.id === 'themeBtn') { theme = theme === 'dark' ? 'light' : 'dark'; applyTheme(theme); try { localStorage.setItem('sigilvm-theme', theme) } catch { /* */ } return }
  if (btn.id === 'panelBtn') { app.classList.toggle('panel-open'); try { localStorage.setItem('sigilvm-panel', app.classList.contains('panel-open') ? 'open' : 'closed') } catch { /* */ } return }
  if (btn.id === 'bnWallet') { ev.preventDefault(); app.classList.toggle('panel-open'); renderPanel(); return }
  if (btn.id === 'swapConnect' || btn.id === 'swapConnect2') { openWalletModal(); return }
  if (btn.id === 'walletBtn') { if (wallet) { app.classList.add('panel-open'); return } openWalletModal(); return }
  if (btn.id === 'pDisconnect') { wallet = null; try { localStorage.removeItem('sigilvm-wallet') } catch { /* */ } for (const k in balances) delete balances[k]; renderChain(); renderPanel(); renderSwap(); return }
  if (btn.id === 'bellBtn') { $('#bellBadge').hidden = true; panelTab = 'activity'; app.classList.add('panel-open'); renderPanel(); return }
  if (btn.id === 'cartBtn') { toast('The cart lights up when listings exist on SIGIL VM — none do yet.', 'warn'); return }
  if (ds.cmtab) { const box = $('#modalBox'); box.querySelectorAll('.cm-tabs button').forEach((b) => b.classList.toggle('on', b === btn)); box.querySelectorAll<HTMLElement>('.cm-pane').forEach((pn) => { pn.hidden = pn.dataset.pane !== ds.cmtab }); return }
  if (ds.ptok) { tokSt.open = ds.ptok; renderTokens(); const row = document.querySelector(`#tokens tr[data-tok="${ds.ptok}"]`); (row || $('#tokens')).scrollIntoView({ behavior: 'smooth', block: 'center' }); return }
  if (ds.ptab) { panelTab = ds.ptab as typeof panelTab; renderPanel(); return }
  if (btn.id === 'pSend' || btn.id === 'pReceive') { window.open('/sigil-wallet-tron-embedded.html', '_blank', 'noopener'); return }
  if (btn.id === 'pSwap') { location.hash = '#swap'; $('#dex').scrollIntoView({ behavior: 'smooth' }); return }
  if (ds.mode && btn.closest('#trendMode')) { trendMode = ds.mode as typeof trendMode; document.querySelectorAll('#trendMode button').forEach((b) => b.classList.toggle('on', b === btn)); if (snap) $('#trendingTable').innerHTML = ui.trending(snap, trendMode, trendWin); return }
  if (ds.cat) { cat = ds.cat; document.querySelectorAll('#cats button').forEach((b) => b.classList.toggle('on', b === btn)); renderRows(); $('#featured').scrollIntoView({ behavior: 'smooth', block: 'start' }); return }
  if (ds.win) { trendWin = ds.win; document.querySelectorAll('#trendWin button').forEach((b) => b.classList.toggle('on', b === btn)); if (snap) $('#trendingTable').innerHTML = ui.trending(snap, trendMode, trendWin); return }
  if (ds.copy) { ev.preventDefault(); ev.stopPropagation(); navigator.clipboard?.writeText(ds.copy).then(() => toast('Proof copied.')).catch(() => toast('Clipboard blocked.', 'warn')); return }
  if (ds.coll && !ev.ctrlKey && !ev.metaKey && !(ev as MouseEvent).button) { ev.preventDefault(); openCollection(ds.coll); return }
  const arrows = btn.closest('.arrows') as HTMLElement | null
  if (arrows && btn.tagName === 'BUTTON') { const row = $('#' + arrows.dataset.scroll); const dir = Array.from(arrows.children).indexOf(btn) === 0 ? -1 : 1; row.scrollBy({ left: dir * (row.clientWidth * 0.8), behavior: 'smooth' }); return }
  // swap module
  if (ds.mode && btn.closest('.seg')) { swapSt.mode = ds.mode as 'market' | 'limit'; renderSwap(); return }
  if (ds.sel) { openTokenPicker(ds.sel as 'from' | 'to'); return }
  if (ds.max) { const b = balances[swapSt.from] ?? 0; swapSt.amount = b ? String(Math.floor(b * 1e4) / 1e4) : ''; renderSwap(); return }
  if (ds.pct) { if (!wallet) { toast('Connect a wallet first — the percentages are of your balance.', 'warn'); openWalletModal(); return } const b = balances[swapSt.from] ?? 0; swapSt.amount = String(Math.floor(b * Number(ds.pct) / 100 * 1e4) / 1e4); renderSwap(); return }
  if (btn.id === 'swapFlip') { const q = currentQuote(); const f = swapSt.from; swapSt.from = swapSt.to; swapSt.to = f; swapSt.amount = q ? String(Math.floor(q.out * 1e4) / 1e4) : ''; renderSwap(); return }
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
  if (btn.id === 'walletUse') { const inp = $('#walletIn') as HTMLInputElement; const v = inp.value.trim().toLowerCase().replace(/^sigil1s:/, '').split(':')[0]; if (!/^[0-9a-f]{64}$/.test(v)) { inp.classList.add('over'); inp.setAttribute('aria-invalid', 'true'); const hint = $('#walletHint'); if (hint) hint.textContent = `That is ${v.length} characters — a SIGIL wallet id is 64 hex characters (0-9, a-f).`; inp.focus(); return } wallet = v; try { localStorage.setItem('sigilvm-wallet', v) } catch { /* */ } closeModal(); app.classList.add('panel-open'); poll(); return }
  const nav = btn.closest('[data-nav]') as HTMLElement | null
  if (nav) { document.querySelectorAll('[data-nav]').forEach((a) => a.classList.toggle('on', (a as HTMLElement).dataset.nav === nav.dataset.nav)) }
})
document.addEventListener('input', (ev) => {
  const t = ev.target as HTMLInputElement
  if (t.id === 'swapAmt') {
    // update only the dependent parts while typing — a full re-render of a type=number input eats a trailing "."
    swapSt.amount = t.value
    if (!snap) return
    const q = currentQuote(); const bal = balances[swapSt.from] ?? 0; const amt = Number(swapSt.amount) || 0
    const pct = bal > 0 ? Math.min(100, (amt / bal) * 100) : 0
    const out = $('#swapOut') as HTMLInputElement | null; if (out) out.value = q ? q.out.toFixed(6) : ''
    const lab = document.querySelector('.slider .lab b'); if (lab) lab.textContent = pct.toFixed(0) + '%'
    const fill = document.querySelector('.slider .fill') as HTMLElement | null; if (fill) fill.style.width = pct + '%'
    const rng = $('#swapRange') as HTMLInputElement | null; if (rng) rng.value = pct.toFixed(0)
    const from = snap.tokens.find((x) => x.id === swapSt.from)!, to = snap.tokens.find((x) => x.id === swapSt.to)!
    const go = $('#swapGo') as HTMLButtonElement | null
    if (go) { const noPool = !snap.pools.length; const over = balances[from.id] !== undefined && amt > bal; go.disabled = noPool || amt <= 0 || over; go.classList.toggle('insufficient', over); go.textContent = amt <= 0 ? 'Enter an amount' : over ? `Insufficient ${from.symbol} balance` : noPool ? 'No pool yet' : `Swap ${from.symbol} → ${to.symbol}` }
    const amtEl = $('#swapAmt'); if (amtEl) amtEl.classList.toggle('over', balances[from.id] !== undefined && amt > bal)
    const rate = document.querySelector('.info:not(.warnbox) .v'); if (rate && q && amt > 0) rate.textContent = `1 ${from.symbol} ≈ ${(q.out / amt).toFixed(6)} ${to.symbol}`
    return
  }
  if (t.id === 'swapRange') { if (!wallet) { t.value = '0'; return } const b = balances[swapSt.from] ?? 0; swapSt.amount = String(Math.floor(b * Number(t.value) / 100 * 1e4) / 1e4); renderSwap(); return }
  if (t.id === 'limitPx') { swapSt.limitPrice = t.value; return }
  if (t.id === 'tokQ') { tokSt.q = t.value; const box = $('#tokens .ttable tbody'); if (snap) { box.innerHTML = (new DOMParser().parseFromString(ui.tokenTable(snap, tokSt), 'text/html').querySelector('tbody') as HTMLElement).innerHTML } return }
  if (t.id === 'q') { globalSearch(t.value) }
  if (t.id === 'walletIn') { t.classList.remove('over'); t.removeAttribute('aria-invalid'); const h = document.getElementById('walletHint'); if (h) h.textContent = /^[0-9a-f]{64}$/i.test(t.value.trim()) ? '✓ looks like a wallet id' : '' }
})
document.addEventListener('focusin', (ev) => { const t = ev.target as HTMLElement; if (t.matches && t.matches('tr[data-coll], [role="button"][data-coll]')) t.scrollIntoView({ block: 'center', behavior: 'smooth' }) })
document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter' && (document.activeElement as HTMLElement | null)?.id === 'walletIn') { ev.preventDefault(); (document.getElementById('walletUse') as HTMLButtonElement | null)?.click(); return }
  // Enter / Space on a card, row or tile that is a role=button div behaves like a click
  if ((ev.key === 'Enter' || ev.key === ' ') && (document.activeElement as HTMLElement | null)?.matches('[role="button"][data-coll], tr[data-coll]')) { ev.preventDefault(); (document.activeElement as HTMLElement).click(); return }
  if (ev.key === '/' && document.activeElement?.tagName !== 'INPUT') { ev.preventDefault(); ($('#q') as HTMLInputElement).focus() }
  if (ev.key === '?' && document.activeElement?.tagName !== 'INPUT') { ev.preventDefault(); openShortcuts() }
  if (document.activeElement?.tagName !== 'INPUT' && !ev.metaKey && !ev.ctrlKey) {
    if (ev.key === 'w') { app.classList.toggle('panel-open') }
    if (ev.key === 't') { theme = theme === 'dark' ? 'light' : 'dark'; applyTheme(theme); try { localStorage.setItem('sigilvm-theme', theme) } catch { /* */ } }
    if (ev.key === 's') { $('#dex').scrollIntoView({ behavior: 'smooth' }) }
    if (ev.key === 'g') { window.scrollTo({ top: 0, behavior: 'smooth' }) }
  }
  if (ev.key === 'Escape') { closeModal(); $('#qres').classList.remove('open'); $('#chainMenu').classList.remove('open') }
  if (ev.key === 'Enter' && document.activeElement?.id === 'q') { const first = $('#qres').querySelector('.r') as HTMLElement | null; if (first) { first.click(); $('#qres').classList.remove('open') } }
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
let modalOpener: HTMLElement | null = null
function openModal(html: string, cls = ''): void {
  const box = $('#modalBox'); box.className = 'box ' + cls; box.innerHTML = (cls ? '' : `<button class="ibtn x" id="modalClose">${I.x}</button>`) + html
  modalOpener = document.activeElement as HTMLElement | null
  const modal = $('#modal'); modal.classList.add('open'); modal.setAttribute('role', 'dialog'); modal.setAttribute('aria-modal', 'true')
  $('#main').setAttribute('inert', ''); $('#panel').setAttribute('inert', ''); document.querySelector('.topbar')?.setAttribute('inert', ''); document.querySelector('.rail')?.setAttribute('inert', '')
  const first = box.querySelector<HTMLElement>('input, button:not(#modalClose), [tabindex="0"]') || box.querySelector<HTMLElement>('button'); first?.focus()
}
function closeModal(): void {
  const modal = $('#modal'); if (!modal.classList.contains('open')) return
  modal.classList.remove('open'); modal.removeAttribute('role'); modal.removeAttribute('aria-modal')
  $('#main').removeAttribute('inert'); $('#panel').removeAttribute('inert'); document.querySelector('.topbar')?.removeAttribute('inert'); document.querySelector('.rail')?.removeAttribute('inert')
  modalOpener?.focus?.(); modalOpener = null; openCollId = null
}
// focus trap: Tab cycles inside the open modal
document.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Tab') return
  const modal = $('#modal'); if (!modal.classList.contains('open')) return
  const f = Array.from(modal.querySelectorAll<HTMLElement>('a[href], button, input, [tabindex="0"]')).filter((e) => !e.hidden && e.offsetParent !== null)
  if (!f.length) return
  const i = f.indexOf(document.activeElement as HTMLElement)
  if (ev.shiftKey && (i <= 0)) { ev.preventDefault(); f[f.length - 1].focus() }
  else if (!ev.shiftKey && (i === f.length - 1 || i < 0)) { ev.preventDefault(); f[0].focus() }
})
function openCollection(id: string): void { const c = snap?.collections.find((x) => x.id === id); if (c) { openCollId = id; openModal(ui.collectionModal(c, snap!), 'coll') } }
function openTokenPicker(which: 'from' | 'to'): void {
  if (!snap) return
  openModal(`<h4>Select a token <span class="muted" style="font-size:11px;font-weight:500">↑↓ Enter</span></h4><div class="list">${snap.tokens.map((t) => `<button data-pick="${t.id}" data-which="${which}"><img src="${t.icon}" alt=""><div><div class="s">${t.symbol}</div><div class="n">${ui.esc(t.name)}</div></div><span class="r">${balances[t.id] === undefined ? '' : fmt.num(balances[t.id], 4)}</span></button>`).join('')}</div>`)
}
function openShortcuts(): void {
  openModal(`<h4>Keyboard shortcuts</h4><div class="keys">
    <div><kbd>/</kbd><span>Search</span></div><div><kbd>?</kbd><span>This sheet</span></div>
    <div><kbd>w</kbd><span>Toggle wallet panel</span></div><div><kbd>t</kbd><span>Toggle light / dark</span></div>
    <div><kbd>s</kbd><span>Jump to Swap &amp; Tokens</span></div><div><kbd>g</kbd><span>Back to top</span></div>
    <div><kbd>↑</kbd><kbd>↓</kbd><kbd>⏎</kbd><span>Pick a token</span></div><div><kbd>Esc</kbd><span>Close anything</span></div>
  </div>`)
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
    <div class="amt"><input id="walletIn" placeholder="64-hex wallet id" value="${gate ?? ''}" spellcheck="false" autocomplete="off"></div><div id="walletHint" class="muted" style="font-size:11.5px;margin-top:6px;min-height:16px"></div>
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
  if (cs.length) rows.push('<div class="h">Collections</div>' + cs.map((c) => `<div class="r" data-coll="${c.id}"><img src="${c.cover}" alt=""><div><div class="n">${ui.esc(c.name)}</div><div class="s">${c.items === null ? '—' : fmt.int(c.items)} items · open</div></div></div>`).join(''))
  const ts = snap.tokens.filter((t) => (t.symbol + t.name).toLowerCase().includes(q)).slice(0, 4)
  if (ts.length) rows.push('<div class="h">Tokens</div>' + ts.map((t) => `<div class="r" data-swap="${t.id}"><img src="${t.icon}" alt=""><div><div class="n">${t.symbol}</div><div class="s">${ui.esc(t.statusNote)}</div></div></div>`).join(''))
  const ms = (snap.miners?.miners ?? []).filter((m) => (m.rig + m.wallet).toLowerCase().includes(q)).slice(0, 4)
  // a block height typed in: find it in the recent set, or say it is older / not minted yet
  const hq = q.replace(/[,\s]/g, '')
  if (/^\d{4,}$/.test(hq)) {
    const h = Number(hq)
    const blk = snap.recent?.blocks.find((x) => x.height === h)
    const sub = blk ? `${blk.is_blue ? 'blue' : 'red'} · score ${fmt.int(blk.blue_score)} · in the recent set` : h <= snap.head.height ? 'older than the recent set · open the braid' : 'not minted yet'
    rows.push(`<div class="h">Block</div><div class="r" data-coll="blocks"><img src="${snap.collections.find((c) => c.id === 'blocks')!.cover}" alt=""><div><div class="n mono">#${fmt.int(h)}</div><div class="s">${sub}</div></div></div>`)
  }
  if (ms.length) rows.push('<div class="h">Rigs</div>' + ms.map((m) => `<div class="r"><img src="${snap!.collections[0].cover}" alt=""><div><div class="n">${ui.esc(m.rig)}</div><div class="s">${fmt.hps(m.hash_rate)} · ${fmt.short(m.wallet, 6)}</div></div></div>`).join(''))
  if (/^[0-9a-f]{64}$/.test(q)) rows.push(`<div class="h">Wallet</div><div class="r" id="qWallet"><img src="${avatarSvg(q, q.slice(0, 1).toUpperCase())}" alt=""><div><div class="n mono">${fmt.short(q, 8)}</div><div class="s">open in panel · balances and records</div></div></div>`)
  box.innerHTML = rows.join('') || '<div class="h">No matches on this node</div>'
  box.classList.add('open')
  const qw = box.querySelector('#qWallet'); if (qw) qw.addEventListener('click', () => { wallet = q; try { localStorage.setItem('sigilvm-wallet', q) } catch { /* */ } app.classList.add('panel-open'); box.classList.remove('open'); poll() })
}

$('#hero').addEventListener('mouseenter', () => { heroHover = true })
$('#hero').addEventListener('mouseleave', () => { heroHover = false })

// live countdowns: between polls, advance the estimated height with the measured block rate
setInterval(() => {
  document.querySelectorAll<HTMLElement>('[data-cd-target]').forEach((el) => {
    const target = Number(el.dataset.cdTarget), h0 = Number(el.dataset.cdHeight), rate = Number(el.dataset.cdRate), at = Number(el.dataset.cdAt)
    if (!(rate > 0)) return
    const est = h0 + ((Date.now() - at) / 1000) * rate
    const left = Math.max(0, target - est)
    const secs = left / rate
    const eta = secs < 60 ? `${Math.round(secs)} s` : secs < 3600 ? `${Math.floor(secs / 60)} min ${Math.round(secs % 60)} s` : secs < 86400 ? `${Math.floor(secs / 3600)} h ${Math.round((secs % 3600) / 60)} min` : `${(secs / 86400).toFixed(1)} d`
    el.textContent = `in ${eta} · ${fmt.int(Math.round(left))} blocks to ${fmt.int(target)} · ${rate.toFixed(1)} blk/s`
  })
}, 1000)

// the wallet download button follows the signed manifest (version + exact file), never a guessed filename
fetch('/downloads/sigil-wallet-latest.json', { headers: { accept: 'application/json' } }).then((r) => (r.headers.get('content-type') || '').includes('json') ? r.json() : null).then((m: { version?: string; url?: string } | null) => {
  const a = document.getElementById('walletApk') as HTMLAnchorElement | null
  if (a && m?.url) { a.href = m.url; a.textContent = `Get the wallet · v${m.version ?? ''}` }
}).catch(() => { /* keep the stable link */ })

// trending hover preview (OpenSea shows a quick card on hover)
const pv = document.createElement('div'); pv.className = 'preview'; pv.hidden = true; document.body.appendChild(pv)
document.addEventListener('mouseover', (ev) => {
  const tr = (ev.target as HTMLElement).closest('#trendingTable tr[data-coll]') as HTMLElement | null
  if (!tr || !snap) { return }
  const c = snap.collections.find((x) => x.id === tr.dataset.coll); if (!c) return
  pv.innerHTML = `<div class="pv-img" style="background-image:url('${c.cover}')"></div><div class="pv-b"><div class="pv-n">${ui.esc(c.name)}</div><div class="pv-by">${ui.esc(c.by || '')}</div><div class="pv-t">${ui.esc(c.blurb)}</div><div class="pv-kv"><span>Floor <b>${ui.esc(c.floor)}</b></span><span>Items <b>${c.items === null ? '—' : fmt.int(c.items)}</b></span></div></div>`
  pv.hidden = false
  const r = tr.getBoundingClientRect(); const w = 300
  pv.style.top = `${Math.min(innerHeight - 190, r.top)}px`
  pv.style.left = `${r.right + w + 16 < innerWidth ? r.right + 8 : Math.max(8, r.left - w - 8)}px`
})
document.addEventListener('mouseout', (ev) => { if ((ev.target as HTMLElement).closest('#trendingTable tr[data-coll]')) pv.hidden = true })
document.addEventListener('scroll', () => { pv.hidden = true }, { passive: true })

// scroll spy: the rail and nav marker follow the section in view (click still wins for the moment of the click)
const SPY: Record<string, string> = { market: 'market', featured: 'featured', drops: 'drops', trending: 'trending', movers: 'trending', sales: 'trending', dex: 'dex' }
const spyTargets = Object.keys(SPY).map((id) => document.getElementById(id)).filter(Boolean) as HTMLElement[]
let spyLock = 0
const spy = new IntersectionObserver((entries) => {
  if (Date.now() < spyLock) return
  const visible = entries.filter((e) => e.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)
  if (!visible.length) return
  const nav = SPY[(visible[0].target as HTMLElement).id]
  document.querySelectorAll<HTMLElement>('[data-nav]').forEach((a) => a.classList.toggle('on', a.dataset.nav === nav || (nav === 'dex' && a.dataset.nav === 'swap' && false)))
}, { rootMargin: `-${72 + 60}px 0px -55% 0px`, threshold: 0 })
spyTargets.forEach((t) => spy.observe(t))
document.addEventListener('click', (ev) => { if ((ev.target as HTMLElement).closest('[data-nav]')) spyLock = Date.now() + 900 })

// back-to-top after a screen of scrolling
const toTop = $('#toTop')
addEventListener('scroll', () => { toTop.hidden = scrollY < innerHeight * 0.9 }, { passive: true })
toTop.addEventListener('click', () => window.scrollTo({ top: 0, behavior: 'smooth' }))

// ── boot ──────────────────────────────────────────────────────────────────
try { const w = localStorage.getItem('sigilvm-wallet'); if (w) wallet = w } catch { /* */ }
try { const qw = new URLSearchParams(location.search).get('wallet'); if (qw && /^[0-9a-f]{64}$/i.test(qw)) { wallet = qw.toLowerCase(); app.classList.add('panel-open'); localStorage.setItem('sigilvm-wallet', wallet) } } catch { /* */ }
poll()
setInterval(() => { if (!document.hidden) poll() }, 10_000)
document.addEventListener('visibilitychange', () => { if (!document.hidden) poll() })
document.querySelectorAll('.row').forEach((r) => r.addEventListener('wheel', (e) => { const we = e as WheelEvent; if (Math.abs(we.deltaY) > Math.abs(we.deltaX)) { (r as HTMLElement).scrollLeft += we.deltaY; e.preventDefault() } }, { passive: false }))
