// main.ts — boot, state, polling, events. One store, one render pass per
// section, no framework: the page is ~1,500 lines and a node poll every 10 s.
import './style.css'
import { api, fmt, NATIVE_TOKEN } from './api'
import { buildSnapshot, quote, type Snapshot } from './model'
import * as ui from './ui'
import { I } from './icons'
import { avatarSvg } from './art'

const $ = <T extends HTMLElement = HTMLElement>(sel: string): T => document.querySelector(sel) as T
const pv = document.createElement('div')
const app = $('#root')
app.innerHTML = ui.shell()

// ── state ─────────────────────────────────────────────────────────────────
let snap: Snapshot | null = null
let heroIdx = 0
let heroTimer: number | undefined
let wallet: string | null = null
const balances: Record<string, number> = {}
let trendMode: ui.TrendMode = 'trending'
const watch = new Set<string>() // the visitor's watchlist — theirs, in this browser
try { for (const id of JSON.parse(localStorage.getItem('sigilvm-watch') || '[]')) if (typeof id === 'string') watch.add(id) } catch { /* */ }
function renderTrending(): void { if (!snap) return; $('#trendingTable').innerHTML = ui.trending(snap, trendMode, trendWin, watch); const n = document.getElementById('nWatch'); if (n) n.textContent = watch.size ? String(watch.size) : '' }
let trendWin = '24h'
let panelTab: 'tokens' | 'nfts' | 'activity' = 'tokens'
const swapSt: ui.SwapState = { mode: 'market', from: NATIVE_TOKEN, to: '', amount: '', limitPrice: '', limitDir: 'buy', slippage: 0.5 }
try { const sl = Number(localStorage.getItem('sigilvm-slippage')); if ([0.1, 0.5, 1, 3].includes(sl)) swapSt.slippage = sl } catch { /* */ } // a preference, remembered like the theme
const tokSt: ui.TokenTableState = { q: '', filter: 'all', sort: 'supply', dir: 'desc', open: null }
let collapsed = false
let cat = 'all'

try { if (new URLSearchParams(location.search).get('embed')) app.classList.add('embed') } catch { /* */ }
try { wallet = localStorage.getItem('sigil-wallet-address') } catch { /* blocked */ }
// theme: saved choice, else the OS preference; the toggle flips and persists
function applyTheme(t: 'dark' | 'light'): void { document.documentElement.setAttribute('data-theme', t); queueMicrotask(() => { if (document.getElementById('themeBtn')) syncToggleAria() }); document.querySelector('meta[name=theme-color]')?.setAttribute('content', t === 'dark' ? '#0b0b0f' : '#f6f6f9'); /* phone browser chrome follows the page */ const b = document.getElementById('themeBtn'); if (b) b.innerHTML = t === 'dark' ? I.sun : I.moon }
let theme: 'dark' | 'light' = 'dark'
try { const saved = localStorage.getItem('sigilvm-theme'); theme = saved === 'light' || saved === 'dark' ? saved : (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark') } catch { /* */ }
applyTheme(theme)
try { if (localStorage.getItem('sigilvm-panel') === 'open') app.classList.add('panel-open') } catch { /* */ }
if (innerWidth >= 1600 && !app.classList.contains('panel-open')) app.classList.add('panel-open')
if (app.classList.contains('panel-open')) document.getElementById('panelBtn')?.setAttribute('aria-expanded', 'true')
// One door for the wallet panel. In drawer mode (≤1100px it overlays the page) it behaves like a dialog: focus moves
// in, the page behind goes inert, and closing hands focus back to whoever opened it.
let panelOpener: HTMLElement | null = null
let lastNav = 'market' // the section the scroll-spy last lit (declared up here: setPanel runs at boot, before the spy is built)
function setPanel(open: boolean, persist = true): void {
  const was = app.classList.contains('panel-open')
  app.classList.toggle('panel-open', open)
  if (persist) { try { localStorage.setItem('sigilvm-panel', open ? 'open' : 'closed') } catch { /* */ } }
  const drawer = innerWidth <= 1100
  if (open && !was) {
    panelOpener = document.activeElement as HTMLElement | null
    if (drawer) { $('#main').setAttribute('inert', ''); document.querySelector('.topbar')?.setAttribute('inert', ''); document.querySelector('.bottomnav')?.removeAttribute('inert'); setTimeout(() => (document.getElementById('panelClose') || document.querySelector<HTMLElement>('.panel button'))?.focus(), 30) }
  } else if (!open && was) {
    $('#main').removeAttribute('inert'); document.querySelector('.topbar')?.removeAttribute('inert')
    if (drawer && panelOpener && document.contains(panelOpener)) panelOpener.focus()
  }
  // the phone tab bar says which sheet is up: Wallet lights while the drawer is open and the section tab stands down
  const bn = document.getElementById('bnWallet'); if (bn) { bn.classList.toggle('on', open && drawer); bn.setAttribute('aria-expanded', open && drawer ? 'true' : 'false') }
  // the toggles say what they did; the drawer is a dialog while it covers the page, a plain complementary column otherwise
  document.getElementById('panelBtn')?.setAttribute('aria-expanded', open ? 'true' : 'false')
  const panel = document.getElementById('panel'); if (panel) { if (open && drawer) { panel.setAttribute('role', 'dialog'); panel.setAttribute('aria-modal', 'true') } else { panel.removeAttribute('role'); panel.removeAttribute('aria-modal') } }
  applyNav(lastNav)
}
// a tablet rotated with the panel open crosses the drawer/docked line — inert must follow the mode, not the click
addEventListener('resize', () => {
  const open = app.classList.contains('panel-open'), drawer = innerWidth <= 1100
  for (const el of [$('#main'), document.querySelector('.topbar')]) { if (!el) continue; if (open && drawer) el.setAttribute('inert', ''); else if (!$('#modal').classList.contains('open')) el.removeAttribute('inert') }
})

function toast(msg: string, cls = ''): void {
  const host = $('#toasts')
  // the same message twice just refreshes the existing toast; the stack never grows past 3
  const same = Array.from(host.children).find((c) => c.textContent === msg) as HTMLElement | undefined
  if (same) { same.classList.remove('bump'); void same.offsetWidth; same.classList.add('bump'); clearTimeout(Number(same.dataset.t)); same.dataset.t = String(setTimeout(() => same.remove(), 4200)); return }
  while (host.children.length >= 3) host.firstElementChild?.remove()
  const t = document.createElement('div'); t.className = 'toast ' + cls; t.textContent = msg
  host.appendChild(t); t.dataset.t = String(setTimeout(() => t.remove(), 4200))
}

// ── render passes ─────────────────────────────────────────────────────────
let heroHover = false
function renderHero(): void {
  if (!snap) return
  $('#hero').innerHTML = ui.hero(snap, heroIdx)
  heroShown = snap.featured[heroIdx % snap.featured.length]?.id ?? ''
  refreshBelowHero()
  armHero()
}
let heroShown = ''
// A poll must not rebuild the hero: that restarted the 7 s progress bar every 10 s and rescheduled the rotation, so the
// carousel only ever advanced in step with polls. Patch the live text in place; fall back to a rebuild if the shape changed.
function refreshHero(): void {
  if (!snap) return
  const host = $('#hero'); if (!host.querySelector('h1') || (snap.featured[heroIdx % snap.featured.length]?.id ?? '') !== heroShown) { renderHero(); return } // the featured order moved under us → rebuild
  const tpl = document.createElement('template'); tpl.innerHTML = ui.hero(snap, heroIdx)
  const sel = '.eyebrow .chip, h1, .by, p, .stats .k, .stats .v'
  const cur = host.querySelectorAll<HTMLElement>(sel), nxt = tpl.content.querySelectorAll<HTMLElement>(sel)
  if (cur.length !== nxt.length) { renderHero(); return }
  cur.forEach((el, i) => { if (el.innerHTML !== nxt[i].innerHTML) { el.innerHTML = nxt[i].innerHTML; if (el.classList.contains('v')) { el.classList.add('flash'); setTimeout(() => el.classList.remove('flash'), 1400) } } })
  refreshBelowHero()
}
function refreshBelowHero(): void {
  if (!snap) return
  swapWithFlash($('#strip'), ui.strip(snap))
  swapWithFlash($('#foryou'), ui.forYou(snap))
}
function armHero(): void {
  clearTimeout(heroTimer)
  // a reduced-motion visitor gets a still hero (thumbnails still switch it by hand)
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return
  // hovering pauses the carousel: the slide stays, the progress bar pauses (CSS), and a due advance waits until the
  // pointer leaves — re-rendering under a hovering pointer used to rebuild the hero and reset the bar
  heroTimer = window.setTimeout(() => { if (heroHover) { heroPending = true; return } heroIdx++; renderHero() }, 7000)
}
let heroPending = false
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
  const feat = cat === 'all' ? snap.featured : snap.collections.filter((c) => c.cat === cat).sort((a, b) => (b.items ?? 0) - (a.items ?? 0)) // the subtitle promises 'ranked by items' — for a category too
  $('#featuredRow').classList.toggle('grid', cat !== 'all')
  swapWithFlash($('#featuredRow'), feat.map(ui.collectionCard).join('') || '<div class="pempty">No collections in this category.</div>')
  const tt = $('#tickerTrack'); const th = ui.ticker(snap); if (tt.innerHTML !== th) { tt.innerHTML = th; tt.style.animationDuration = `${Math.max(20, (tt.scrollWidth / 2) / 80)}s` } // constant ~80 px/s whatever the item count (a fixed 60 s made a long ticker race and a short one crawl)
  $('#dropsRow').innerHTML = snap.drops.map(ui.dropCard).join('')
  swapWithFlash($('#moversRow'), snap.movers.map(ui.moverCard).join('') || '<div class="pempty">No miners read — node offline?</div>')
  $('#salesRow').innerHTML = snap.sales.map(ui.saleCard).join('') || '<div class="pempty">Nothing settled this week that the node reports.</div>'
  swapWithFlash($('#trendingTable'), ui.trending(snap, trendMode, trendWin, watch)); { const n = document.getElementById('nWatch'); if (n) n.textContent = watch.size ? String(watch.size) : '' }
  $('#legend').innerHTML = ui.legend(snap)
  $('#footTs').textContent = `poll ${new Date(snap.at).toLocaleTimeString()} · height ${fmt.int(snap.head.height)}`
}
function renderChain(): void {
  if (!snap) return
  const chip = $('#chainChip'); const d = chip.querySelector('.d') as HTMLElement
  d.classList.toggle('on', snap.head.ok)
  $('#chainHeight').textContent = snap.head.ok ? fmt.int(snap.head.height) : 'offline'
  $('#walletLbl').textContent = wallet ? fmt.short(wallet, 5) : 'Connect wallet'
  const wb = $('#walletBtn'); wb.classList.toggle('connected', !!wallet); wb.setAttribute('aria-label', wallet ? `Wallet ${fmt.short(wallet, 5)} connected — open the wallet panel` : 'Connect a wallet') // the label span is hidden on phones; the name must not vanish with it
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
// The detail row spans the table with colspan. In fixed table layout (≤520px container) a colspan wider than the
// VISIBLE column count makes Chrome invent phantom columns for the hidden ones and hand them the Token column's
// width (measured: Token th 160px → 18px the moment a drawer opened). Span exactly the visible columns instead.
function fitDetailSpan(): void {
  const tds = document.querySelectorAll<HTMLTableCellElement>('#tokens tr.td-row td, #tokens tr.empty td'); if (!tds.length) return
  const n = Array.from(document.querySelectorAll('#tokens thead th')).filter((th) => getComputedStyle(th).display !== 'none').length
  if (n) tds.forEach((td) => { if (td.colSpan !== n) td.colSpan = n })
}
function renderTokens(): void { if (snap) { $('#tokens').innerHTML = ui.tokenTable(snap, tokSt); fitDetailSpan() } }
if ('ResizeObserver' in window) new ResizeObserver(() => fitDetailSpan()).observe($('#tokens'))
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
  refreshHero(); renderRows(); renderSwap(); renderTokens(); renderPanel()
  // an open collection page refreshes its stats and the active pane in place (keeps tab + scroll)
  if (openCollId && $('#modal').classList.contains('open')) {
    const c = snap!.collections.find((x) => x.id === openCollId)
    if (c) {
      const box = $('#modalBox'); const activeTab = (box.querySelector('.cm-tabs button.on') as HTMLElement | null)?.dataset.cmtab || 'items'
      const scroll = (box.querySelector('.cm-body') as HTMLElement | null)?.scrollTop ?? 0
      const fresh = new DOMParser().parseFromString(`<div>${ui.collectionModal(c, snap!, watch.has(c.id))}</div>`, 'text/html')
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
    if (!el) { el = document.createElement('div'); el.id = 'offline'; el.className = 'offline'; el.setAttribute('role', 'alert'); $('#main').prepend(el) }
    el.innerHTML = `<span class="chip bad"><i class="d"></i>node unreachable</span><span>sigil-api did not answer. Nothing on this page is a measurement until it does — retrying every 10 s.</span><button class="btn ghost sm" id="retryNow">Retry now</button><span class="muted mono" style="font-size:11px">or add <code>?api=http://host:18181</code></span>`
  } else if (el) el.remove()
  // the hero skeleton's eyebrow must not claim progress while the node is unreachable
  const eb = document.querySelector<HTMLElement>('.skel-hero .eyebrow .chip')
  if (eb) { eb.className = on ? 'chip bad' : 'chip gold'; eb.innerHTML = `<i class="d"></i>${on ? 'waiting for the node…' : 'reading the chain…'}` }
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
    // first paint: give every route 2.5 s, paint with what arrived (late routes read as unread), re-poll when they settle
    snap = await buildSnapshot(firstPaint ? { ms: 2500, onLate: (full) => { if (!polling) { snap = full; void loadBalances().then(renderAll) } } } : undefined)
    await loadBalances()
    renderAll()
    // a deep link (#dex, #trending…) scrolled before the live content existed — re-anchor once the page has its real height
    if (firstPaint && location.hash && !snap.offline) { const target = anchorBox(location.hash); if (target) requestAnimationFrame(() => target.scrollIntoView({ behavior: 'instant', block: 'start' })) }
    if (!snap.offline && prevHeight && snap.head.height > prevHeight) { const b = $('#bellBadge'); b.hidden = false; $('#bellBtn').setAttribute('aria-label', `Activity — ${fmt.n(snap.head.height - prevHeight, 'new block')} since you looked`) }
  } catch (e) {
    toast('poll failed: ' + (e as Error).message, 'bad')
  } finally { polling = false }
}

// ── row arrows: greyed at the ends (OpenSea) ───────────────────────────────
function syncRowArrows(): void {
  document.querySelectorAll<HTMLElement>('.arrows[data-scroll]').forEach((a) => {
    const row = document.getElementById(a.dataset.scroll!); if (!row) return
    const [l, r] = Array.from(a.querySelectorAll('button'))
    const max = row.scrollWidth - row.clientWidth
    if (l) l.disabled = row.scrollLeft <= 2
    if (r) r.disabled = max <= 2 || row.scrollLeft >= max - 2
  })
}
document.addEventListener('scroll', (e) => { const t = e.target as HTMLElement; if (t && t.classList?.contains('row')) syncRowArrows() }, { capture: true, passive: true })
addEventListener('resize', syncRowArrows)

// ── toggle semantics ──────────────────────────────────────────────────────
// Renderers mark the active choice with class="on"; screen readers need the same fact as ARIA. Toggle groups get
// aria-pressed, real tab strips (panel, collection sheet) get tablist/tab/aria-selected. A debounced observer keeps
// it true after every re-render without each renderer having to remember.
function syncToggleAria(): void {
  // popover + theme toggles say their state: the chain chip's menu open/closed, the theme button's NEXT theme
  document.getElementById('chainChip')?.setAttribute('aria-expanded', $('#chainMenu').classList.contains('open') ? 'true' : 'false')
  document.getElementById('q')?.setAttribute('aria-expanded', $('#qres').classList.contains('open') ? 'true' : 'false')
  const tb = document.getElementById('themeBtn'); if (tb) { const l = theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'; if (tb.getAttribute('aria-label') !== l) { tb.setAttribute('aria-label', l); tb.title = l } }
  document.querySelectorAll<HTMLElement>('.tabs, .seg, .cats, .tokens-tools .f').forEach((g) => { g.setAttribute('role', 'group'); g.querySelectorAll('button').forEach((b) => b.setAttribute('aria-pressed', b.classList.contains('on') ? 'true' : 'false')) })
  document.querySelectorAll<HTMLElement>('.ptabs, .cm-tabs').forEach((g) => { g.setAttribute('role', 'tablist'); g.querySelectorAll('button').forEach((b) => { b.setAttribute('role', 'tab'); b.setAttribute('aria-selected', b.classList.contains('on') ? 'true' : 'false') }) })
  // every link that leaves the page in a new tab says so: a ↗ after its text (cards carry it in their overlay already)
  document.querySelectorAll<HTMLAnchorElement>('a[target="_blank"]:not([data-ext])').forEach((a) => {
    a.dataset.ext = '1'
    const txt = a.textContent?.trim() ?? ''
    if (!a.title) a.title = (txt ? txt + ' — ' : '') + 'opens in a new tab'
    if (!txt || /↗/.test(txt) || a.classList.contains('card') || a.querySelector('.cta-btn')) return
    const host = a.querySelector('.tip') || a; host.insertAdjacentHTML('beforeend', '<span class="ext" aria-hidden="true"> ↗</span>')
  })
}
let ariaTimer = 0
new MutationObserver(() => { clearTimeout(ariaTimer); ariaTimer = window.setTimeout(() => { syncToggleAria(); syncRowArrows(); syncClipTitles() }, 30) }).observe(app, { subtree: true, childList: true, attributes: true, attributeFilter: ['class'] })
syncToggleAria()
// a clipped line gives its full text on hover: every ellipsised element that actually overflows carries its text as a
// title (and drops it again when it fits — widths change with the viewport, so this runs after renders and resizes)
const CLIP = '.strip .vt, .strip .s, .card .name, .card .byline, .card .kv .v, .card.sale .proof, .trending td.r, .trending td:nth-child(3), .trending td:nth-child(5), .trending .coll .n, .ttable .tk .nm, .td-cell .v, .pnft .m .n, .pact .w small, .cm-stats .v, .cm-item .t, .cm-item .s, .cm-item .m, .cm-item .h .hx'
function syncClipTitles(): void {
  for (const el of document.querySelectorAll<HTMLElement>(CLIP)) {
    if (el.scrollWidth > el.clientWidth + 1) { const t = (el.textContent || '').replace(/\s+/g, ' ').trim(); if (t && el.title !== t && (!el.title || el.dataset.clip)) { el.title = t; el.dataset.clip = '1' } }
    else if (el.dataset.clip) { el.removeAttribute('title'); delete el.dataset.clip }
  }
}
let clipTimer = 0
addEventListener('resize', () => { clearTimeout(clipTimer); clipTimer = window.setTimeout(syncClipTitles, 120) })
syncClipTitles()

// ── events (delegated) ────────────────────────────────────────────────────
// most handlers below re-render their section with innerHTML, which throws the keyboard's focus to <body>. When the
// activated control was focused, find its twin in the new markup (same tag, id, data-* and text) and put focus back.
function keepFocus(btn: HTMLElement): void {
  if (document.activeElement !== btn) return
  const key = { tag: btn.tagName, id: btn.id, data: JSON.stringify(btn.dataset), text: (btn.textContent || '').trim().slice(0, 40), cls: btn.className }; const first = Object.entries(btn.dataset)[0]
  queueMicrotask(() => {
    if (document.contains(btn) || (document.activeElement && document.activeElement !== document.body)) return
    const twin = Array.from(document.querySelectorAll<HTMLElement>(key.tag)).find((e) => e.id === key.id && JSON.stringify(e.dataset) === key.data && (e.textContent || '').trim().slice(0, 40) === key.text)
      || (first ? Array.from(document.querySelectorAll<HTMLElement>(`${key.tag}[data-${first[0].replace(/[A-Z]/g, (m) => '-' + m.toLowerCase())}="${first[1]}"]`))[0] : undefined) // its text may have changed (sort arrow, ☆→★) — the data attribute is the identity
      || (key.cls ? Array.from(document.querySelectorAll<HTMLElement>(key.tag)).find((e) => e.className === key.cls && (e.textContent || '').trim().slice(0, 40) === key.text) : undefined)
    twin?.focus({ preventScroll: true })
  })
}
document.addEventListener('click', (ev) => {
  const t = ev.target as HTMLElement
  if (t.id === 'modal') { closeModal(); return }
  if (t.id === 'panelScrim') { setPanel(false); return }
  // th before tr: a header click used to resolve to its <tr>, so th[data-sort] was never found and the token table never re-sorted
  const btn = t.closest('th[data-sort], button, a, tr, [data-hero], [data-coll], [data-swap], [data-copy]') as HTMLElement | null
  if (!btn) return
  keepFocus(btn)
  const ds = btn.dataset
  if (ds.hero !== undefined) { heroIdx = Number(ds.hero); renderHero(); return }
  if (btn.id === 'chainChip') { $('#chainMenu').classList.toggle('open'); return }
  if (btn.id === 'retryNow') {
    const b = btn as HTMLButtonElement; b.disabled = true; b.textContent = 'Retrying…'
    Promise.resolve(poll()).finally(() => { const still = document.getElementById('retryNow') as HTMLButtonElement | null; if (still) { still.disabled = false; still.textContent = 'Retry now'; toast('Still no answer from the node.', 'warn') } else toast('Node answered — live again.') })
    return
  }
  if (btn.id === 'footKeys') { openShortcuts(); return }
  if (btn.id === 'themeBtn') { theme = theme === 'dark' ? 'light' : 'dark'; applyTheme(theme); try { localStorage.setItem('sigilvm-theme', theme) } catch { /* */ } return }
  if (btn.id === 'panelBtn') { setPanel(!app.classList.contains('panel-open')); return }
  if (btn.id === 'bnWallet') { ev.preventDefault(); setPanel(!app.classList.contains('panel-open'), false); renderPanel(); return }
  if (btn.id === 'swapConnect' || btn.id === 'swapConnect2') { openWalletModal(); return }
  if (btn.id === 'walletBtn') { if (wallet) { setPanel(true, false); return } openWalletModal(); return }
  if (btn.id === 'panelClose') { setPanel(false); return }
  if (btn.id === 'pDisconnect') { wallet = null; try { localStorage.removeItem('sigilvm-wallet') } catch { /* */ } for (const k in balances) delete balances[k]; renderChain(); renderPanel(); renderSwap(); return }
  if (btn.id === 'bellBtn') { $('#bellBadge').hidden = true; btn.setAttribute('aria-label', 'Activity'); panelTab = 'activity'; setPanel(true, false); renderPanel(); return }
  if (btn.id === 'cartBtn') { toast('The cart lights up when listings exist on SIGIL VM — none do yet.', 'warn'); return }
  if (ds.cmtab) { const box = $('#modalBox'); box.querySelectorAll('.cm-tabs button').forEach((b) => b.classList.toggle('on', b === btn)); box.querySelectorAll<HTMLElement>('.cm-pane').forEach((pn) => { pn.hidden = pn.dataset.pane !== ds.cmtab }); return }
  if (ds.ptok) { tokSt.open = ds.ptok; renderTokens(); const row = document.querySelector(`#tokens tr[data-tok="${ds.ptok}"]`); (row || $('#tokens')).scrollIntoView({ behavior: 'smooth', block: 'center' }); return }
  if (ds.ptab) { panelTab = ds.ptab as typeof panelTab; renderPanel(); return }
  if (btn.id === 'pSend' || btn.id === 'pReceive') { window.open('/sigil-wallet-tron-embedded.html', '_blank', 'noopener'); return }
  if (btn.id === 'pSwap') { location.hash = '#swap'; $('#dex').scrollIntoView({ behavior: 'smooth' }); return }
  if (ds.mode && btn.closest('#trendMode')) { trendMode = ds.mode as typeof trendMode; document.querySelectorAll('#trendMode button').forEach((b) => b.classList.toggle('on', b === btn)); renderTrending(); return }
  if (ds.cat) { cat = ds.cat; document.querySelectorAll('#cats button').forEach((b) => b.classList.toggle('on', b === btn)); renderRows(); $('#featured').scrollIntoView({ behavior: 'smooth', block: 'start' }); return }
  if (ds.win) { trendWin = ds.win; document.querySelectorAll('#trendWin button').forEach((b) => b.classList.toggle('on', b === btn)); renderTrending(); return }
  if (ds.watch) { ev.preventDefault(); ev.stopPropagation(); if (watch.has(ds.watch)) watch.delete(ds.watch); else watch.add(ds.watch); try { localStorage.setItem('sigilvm-watch', JSON.stringify([...watch])) } catch { /* */ } renderTrending(); const on = watch.has(ds.watch); document.querySelectorAll<HTMLElement>(`.cm-foot [data-watch="${ds.watch}"]`).forEach((b) => { b.setAttribute('aria-pressed', on ? 'true' : 'false'); b.textContent = on ? '★ Watching' : '☆ Watch' }); return } // the sheet's button follows without a re-render (its focus stays put)
  if (ds.copy) { ev.preventDefault(); ev.stopPropagation(); navigator.clipboard?.writeText(ds.copy).then(() => toast(`Copied ${ds.copy!.length === 64 ? 'id' : 'hash'} ${ds.copy!.slice(0, 8)}…`)).catch(() => toast('Clipboard blocked.', 'warn')); return }
  if (ds.coll && !ev.ctrlKey && !ev.metaKey && !(ev as MouseEvent).button) { ev.preventDefault(); openCollection(ds.coll); return }
  const arrows = btn.closest('.arrows') as HTMLElement | null
  if (arrows && btn.tagName === 'BUTTON') { const row = $('#' + arrows.dataset.scroll); const dir = Array.from(arrows.children).indexOf(btn) === 0 ? -1 : 1; row.scrollBy({ left: dir * (row.clientWidth * 0.8), behavior: 'smooth' }); return }
  // swap module
  if (ds.mode && btn.closest('.seg')) { swapSt.mode = ds.mode as 'market' | 'limit'; renderSwap(); return }
  if (ds.sel) { openTokenPicker(ds.sel as 'from' | 'to'); return }
  if (ds.max) { const b = balances[swapSt.from] ?? 0; swapSt.amount = b ? String(Math.floor(b * 1e4) / 1e4) : ''; renderSwap(); return }
  if (ds.pct) { if (!wallet) { toast('Connect a wallet first — the percentages are of your balance.', 'warn'); openWalletModal(); return } const b = balances[swapSt.from] ?? 0; swapSt.amount = String(Math.floor(b * Number(ds.pct) / 100 * 1e4) / 1e4); renderSwap(); return }
  if (btn.id === 'swapFlip') { const q = currentQuote(); const f = swapSt.from; swapSt.from = swapSt.to; swapSt.to = f; swapSt.amount = q ? String(Math.floor(q.out * 1e4) / 1e4) : swapSt.amount; renderSwap(); return } // with a quote the output becomes the input (Uniswap); without one, keep what was typed rather than wipe it
  if (ds.dir) { swapSt.limitDir = ds.dir as 'buy' | 'sell'; renderSwap(); return }
  if (btn.id === 'swapCollapse') { collapsed = true; renderSwap(); return }
  if (btn.id === 'swapExpand') { collapsed = false; renderSwap(); return }
  if (btn.id === 'swapSettings') { openSlippage(); return }
  if (btn.id === 'swapGo') { if (snap) { const from = snap.tokens.find((x) => x.id === swapSt.from)!; const to = snap.tokens.find((x) => x.id === swapSt.to)!; window.open(`/sigil-wallet-tron-embedded.html#swap=${from.symbol}:${to.symbol}:${swapSt.amount}`, '_blank', 'noopener'); toast('Handing the swap to your wallet to sign.') } return }
  // token table
  if (ds.filter) { tokSt.filter = ds.filter as typeof tokSt.filter; renderTokens(); return }
  if (ds.swap) { swapSt.to = ds.swap; if (swapSt.from === ds.swap) swapSt.from = NATIVE_TOKEN === ds.swap ? snap!.tokens[1].id : NATIVE_TOKEN; renderSwap(); $('#dex').scrollIntoView({ behavior: 'smooth' }); return }
  if (ds.info) { tokSt.open = tokSt.open === ds.info ? null : ds.info; renderTokens(); (document.querySelector(`#tokens [data-info="${ds.info}"]`) as HTMLElement | null)?.focus({ preventScroll: true }); return } // the table re-renders: keep the keyboard on the same Info/Close button
  const trow = btn.closest('tr[data-tok]') as HTMLElement | null
  if (trow && !ds.swap) { tokSt.open = tokSt.open === trow.dataset.tok ? null : trow.dataset.tok!; renderTokens(); return }
  const th = btn.closest('th[data-sort]') as HTMLElement | null
  if (th) { const k = th.dataset.sort as ui.TokenTableState['sort']; if (tokSt.sort === k) tokSt.dir = tokSt.dir === 'asc' ? 'desc' : 'asc'; else { tokSt.sort = k; tokSt.dir = 'desc' } renderTokens(); return }
  if (btn.id === 'modalClose' || btn.id === 'modalClose2' || btn.closest('#modal') === btn) { closeModal(); return }
  if (ds.pick && ds.which) { if (ds.which === 'from') { if (swapSt.to === ds.pick) swapSt.to = swapSt.from; swapSt.from = ds.pick } else { if (swapSt.from === ds.pick) swapSt.from = swapSt.to; swapSt.to = ds.pick } closeModal(); renderSwap(); return }
  if (ds.slip) { swapSt.slippage = Number(ds.slip); try { localStorage.setItem('sigilvm-slippage', ds.slip) } catch { /* */ } closeModal(); renderSwap(); return }
  if (btn.id === 'walletUse') { const inp = $('#walletIn') as HTMLInputElement; const v = inp.value.trim().toLowerCase().replace(/^sigil1s:/, '').split(':')[0]; if (!/^[0-9a-f]{64}$/.test(v)) { inp.classList.add('over'); inp.setAttribute('aria-invalid', 'true'); const hint = $('#walletHint'); if (hint) hint.textContent = `That is ${v.length} characters — a SIGIL wallet id is 64 hex characters (0-9, a-f).`; inp.focus(); return } wallet = v; try { localStorage.setItem('sigilvm-wallet', v) } catch { /* */ } closeModal(); setPanel(true, false); poll(); return }
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
    if (ev.key === 'w') { setPanel(!app.classList.contains('panel-open'), false) }
    if (ev.key === 't') { theme = theme === 'dark' ? 'light' : 'dark'; applyTheme(theme); try { localStorage.setItem('sigilvm-theme', theme) } catch { /* */ } }
    if (ev.key === 's') { $('#dex').scrollIntoView({ behavior: 'smooth' }) }
    if (ev.key === 'g') { window.scrollTo({ top: 0, behavior: 'smooth' }) }
  }
  // tablists: ←/→ (and Home/End) move between tabs and activate them, as the tab pattern expects
  const tab = (document.activeElement as HTMLElement | null)?.closest('[role="tablist"] > button') as HTMLButtonElement | null
  if (tab && ['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(ev.key)) {
    const tabs = Array.from(tab.parentElement!.querySelectorAll<HTMLButtonElement>('button')); const i = tabs.indexOf(tab)
    const n = ev.key === 'Home' ? 0 : ev.key === 'End' ? tabs.length - 1 : (i + (ev.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length
    ev.preventDefault(); tabs[n].click(); tabs[n].focus(); return
  }
  const modalWasOpen = $('#modal').classList.contains('open')
  if (ev.key === 'Escape') { closeModal(); $('#qres').classList.remove('open'); if ($('#chainMenu').classList.contains('open') && (document.activeElement as HTMLElement | null)?.closest('.chain-wrap')) $('#chainChip').focus(); $('#chainMenu').classList.remove('open'); if (innerWidth <= 1100 && app.classList.contains('panel-open') && !modalWasOpen) setPanel(false) }
  // search results: ↑/↓ move a highlight through the hits, Enter opens the highlighted one (or the first)
  const qres = $('#qres')
  if (qres.classList.contains('open') && (document.activeElement?.id === 'q' || document.activeElement?.classList.contains('r')) && (ev.key === 'ArrowDown' || ev.key === 'ArrowUp' || ev.key === 'Enter')) {
    const hits = Array.from(qres.querySelectorAll<HTMLElement>('.r'))
    const cur = hits.findIndex((h) => h.classList.contains('on'))
    if (ev.key === 'Enter') { const pick = hits[cur >= 0 ? cur : 0]; if (pick) { pick.click(); qres.classList.remove('open') } return }
    ev.preventDefault()
    const n = ev.key === 'ArrowDown' ? Math.min(hits.length - 1, cur + 1) : Math.max(0, cur - 1)
    hits.forEach((h, i) => { h.classList.toggle('on', i === n); h.setAttribute('aria-selected', i === n ? 'true' : 'false') }); hits[n]?.scrollIntoView({ block: 'nearest' })
    if (hits[n]) $('#q').setAttribute('aria-activedescendant', hits[n].id)
    return
  }
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
// a popover that loses keyboard focus closes (Tab past its last link used to leave the chain menu hanging open)
// coming back to a search that still holds text shows its hits again (they close whenever focus leaves)
document.getElementById('q')?.addEventListener('focus', (ev) => { globalSearch((ev.target as HTMLInputElement).value) }) // empty → the 'Trending now' start
document.querySelector('.search')?.addEventListener('focusout', (ev) => { const to = (ev as FocusEvent).relatedTarget as HTMLElement | null; if (!to || !to.closest('.search')) $('#qres').classList.remove('open') })
document.querySelector('.chain-wrap')?.addEventListener('focusout', (ev) => { const to = (ev as FocusEvent).relatedTarget as HTMLElement | null; if (!to || !to.closest('.chain-wrap')) $('#chainMenu').classList.remove('open') })
document.addEventListener('click', (ev) => { const t = ev.target as HTMLElement; if (!t.closest('.search')) $('#qres').classList.remove('open'); if (!t.closest('.chain-wrap')) $('#chainMenu').classList.remove('open') })

// ── modals ────────────────────────────────────────────────────────────────
let modalOpener: HTMLElement | null = null
function openModal(html: string, cls = ''): void {
  const box = $('#modalBox'); box.className = 'box ' + cls; box.innerHTML = (cls ? '' : `<button class="ibtn x" id="modalClose" aria-label="Close" title="Close (Esc)">${I.x}</button>`) + html
  modalOpener = document.activeElement as HTMLElement | null
  $('#chainMenu').classList.remove('open'); $('#qres').classList.remove('open'); pv.hidden = true // popovers close under a dialog
  const modal = $('#modal'); modal.classList.add('open'); modal.setAttribute('role', 'dialog'); modal.setAttribute('aria-modal', 'true')
  const h = box.querySelector<HTMLElement>('h2, h3, h4'); if (h) { h.id = 'modalTitle'; modal.setAttribute('aria-labelledby', 'modalTitle'); modal.removeAttribute('aria-label') } else { modal.removeAttribute('aria-labelledby'); modal.setAttribute('aria-label', 'Dialog') } // the dialog is named by its own heading
  $('#main').setAttribute('inert', ''); $('#panel').setAttribute('inert', ''); document.querySelector('.topbar')?.setAttribute('inert', ''); document.querySelector('.rail')?.setAttribute('inert', '')
  const first = box.querySelector<HTMLElement>('input, button:not(#modalClose), [tabindex="0"]') || box.querySelector<HTMLElement>('button'); first?.focus()
}
function closeModal(): void {
  const modal = $('#modal'); if (!modal.classList.contains('open')) return
  modal.classList.remove('open'); modal.removeAttribute('role'); modal.removeAttribute('aria-modal'); modal.removeAttribute('aria-labelledby'); modal.removeAttribute('aria-label')
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
function openCollection(id: string): void { const c = snap?.collections.find((x) => x.id === id); if (c) { openCollId = id; openModal(ui.collectionModal(c, snap!, watch.has(id)), 'coll') } }
function openTokenPicker(which: 'from' | 'to'): void {
  if (!snap) return
  openModal(`<h4>Select a token <span class="muted kbd-only" style="font-size:11px;font-weight:500" aria-hidden="true">↑↓ Enter</span></h4><div class="list">${snap.tokens.map((t) => `<button data-pick="${t.id}" data-which="${which}"><img src="${t.icon}" alt=""><div><div class="s">${t.symbol}</div><div class="n">${ui.esc(t.name)}</div></div><span class="r">${balances[t.id] === undefined ? '' : fmt.num(balances[t.id], 4)}</span></button>`).join('')}</div>`)
}
function openShortcuts(): void {
  openModal(`<h4>Keyboard shortcuts</h4><div class="keys">
    <div><kbd>/</kbd><span>Search</span></div><div><kbd>?</kbd><span>This sheet</span></div>
    <div><kbd>w</kbd><span>Toggle wallet panel</span></div><div><kbd>t</kbd><span>Toggle light / dark</span></div>
    <div><kbd>s</kbd><span>Jump to Swap &amp; Tokens</span></div><div><kbd>g</kbd><span>Back to top</span></div>
    <div><kbd>↑</kbd><kbd>↓</kbd><kbd>⏎</kbd><span>Pick a token or a search hit</span></div><div><kbd>Esc</kbd><span>Close anything</span></div>
    <div><kbd>←</kbd><kbd>→</kbd><span>Move between tabs</span></div><div><kbd>⏎</kbd><kbd>Space</kbd><span>Open a focused card or row</span></div>
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
  if (!snap) { box.classList.remove('open'); return }
  const rows: string[] = []
  // an empty box offers a start (OpenSea's empty search lists what is trending): the biggest collections and the native tokens — all live figures
  const cs = q ? snap.collections.filter((c) => c.name.toLowerCase().includes(q) || c.blurb.toLowerCase().includes(q)).slice(0, 4) : snap.featured.slice(0, 4)
  if (!q) rows.push('<div class="h">Trending now · by items on chain</div>')
  if (cs.length) rows.push((q ? '<div class="h">Collections</div>' : '') + cs.map((c) => `<div class="r" data-coll="${c.id}"><img src="${c.cover}" alt=""><div><div class="n">${ui.esc(c.name)}</div><div class="s">${c.items === null ? '—' : fmt.int(c.items)} ${c.items === 1 ? 'item' : 'items'} · open</div></div></div>`).join(''))
  const ts = (q ? snap.tokens.filter((t) => (t.symbol + t.name).toLowerCase().includes(q)) : snap.tokens.filter((t) => t.status !== 'external' && t.status !== 'dormant')).slice(0, 4)
  if (ts.length) rows.push('<div class="h">Tokens</div>' + ts.map((t) => `<div class="r" data-swap="${t.id}"><img src="${t.icon}" alt=""><div><div class="n">${t.symbol}</div><div class="s">${ui.esc(t.statusNote)}</div></div></div>`).join(''))
  const ms = q ? (snap.miners?.miners ?? []).filter((m) => (m.rig + m.wallet).toLowerCase().includes(q)).slice(0, 4) : []
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
  box.querySelectorAll('.r').forEach((r, i) => { r.setAttribute('role', 'option'); r.id = 'qr-' + i; r.setAttribute('aria-selected', 'false') }); box.querySelectorAll('.h').forEach((h) => h.setAttribute('role', 'presentation'))
  $('#q').removeAttribute('aria-activedescendant')
  box.classList.add('open')
  const qw = box.querySelector('#qWallet'); if (qw) qw.addEventListener('click', () => { wallet = q; try { localStorage.setItem('sigilvm-wallet', q) } catch { /* */ } setPanel(true, false); box.classList.remove('open'); poll() })
}

// swipe on the hero (phones): a horizontal drag of ≥48px switches the slide; vertical drags keep scrolling the page
{ let sx = 0, sy = 0, live = false
  const hero = $('#hero')
  hero.addEventListener('pointerdown', (e) => { if (e.pointerType === 'mouse') return; sx = e.clientX; sy = e.clientY; live = true }, { passive: true })
  hero.addEventListener('pointerup', (e) => { if (!live) return; live = false; const dx = e.clientX - sx, dy = e.clientY - sy; if (Math.abs(dx) < 48 || Math.abs(dx) < Math.abs(dy) * 1.5 || !snap) return; heroIdx = (heroIdx + (dx < 0 ? 1 : snap.featured.length - 1)) % snap.featured.length; renderHero() }, { passive: true })
  hero.addEventListener('pointercancel', () => { live = false }, { passive: true })
}
$('#hero').addEventListener('mouseenter', () => { heroHover = true })
$('#hero').addEventListener('mouseleave', () => { heroHover = false; if (heroPending) { heroPending = false; clearTimeout(heroTimer); heroTimer = window.setTimeout(() => { heroIdx++; renderHero() }, 1200) } })

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
pv.className = 'preview'; pv.hidden = true; document.body.appendChild(pv)
document.addEventListener('mouseover', (ev) => {
  const tr = (ev.target as HTMLElement).closest('#trendingTable tr[data-coll]') as HTMLElement | null
  if (!tr || !snap) { return }
  const c = snap.collections.find((x) => x.id === tr.dataset.coll); if (!c) return
  // the card lives in the gutter beside the table; with no gutter on either side (≈≤1180px) it would sit on top of the
  // neighbouring rows' names, so it stays away — the row already carries the facts and a tap opens the sheet
  const r = tr.getBoundingClientRect(); const w = 300
  const fitsRight = r.right + w + 16 < innerWidth, fitsLeft = r.left - w - 16 >= 0
  if ((!fitsRight && !fitsLeft) || matchMedia('(hover: none)').matches) { pv.hidden = true; return }
  pv.innerHTML = `<div class="pv-img" style="background-image:url('${c.cover}')"></div><div class="pv-b"><div class="pv-n">${ui.esc(c.name)}</div><div class="pv-by">${ui.esc(c.by || '')}</div><div class="pv-t">${ui.esc(c.blurb)}</div><div class="pv-kv"><span>Floor <b>${ui.esc(c.floor)}</b></span><span>Items <b>${c.items === null ? '—' : fmt.int(c.items)}</b></span></div></div>`
  pv.hidden = false
  const ph = pv.getBoundingClientRect().height || 280
  pv.style.top = `${Math.max(8, Math.min(innerHeight - ph - 8, r.top))}px`
  pv.style.left = `${fitsRight ? r.right + 8 : r.left - w - 8}px`
})
document.addEventListener('mouseout', (ev) => { if ((ev.target as HTMLElement).closest('#trendingTable tr[data-coll]')) pv.hidden = true })
document.addEventListener('scroll', () => { pv.hidden = true }, { passive: true })

// scroll spy: the rail and nav marker follow the section in view (click still wins for the moment of the click)
const SPY: Record<string, string> = { market: 'market', featured: 'featured', drops: 'drops', trending: 'trending', movers: 'trending', sales: 'trending', dex: 'dex' }
// the five-tab bottom nav has no DEX or Trending tab: its Swap covers the DEX and Discover covers Trending/Movers/Sales (data-also);
// the rail keeps every section apart. While the wallet drawer covers the page (≤1100px) the bottom nav's section tab stands down.
function applyNav(nav: string): void {
  const drawerOpen = innerWidth <= 1100 && app.classList.contains('panel-open')
  document.querySelectorAll<HTMLElement>('[data-nav]').forEach((a) => a.classList.toggle('on', (a.dataset.nav === nav || (a.dataset.also || '').split(' ').includes(nav)) && !(drawerOpen && !!a.closest('.bottomnav'))))
}
const spyTargets = Object.keys(SPY).map((id) => document.getElementById(id)).filter(Boolean) as HTMLElement[]
let spyLock = 0
const spy = new IntersectionObserver((entries) => {
  if (Date.now() < spyLock) return
  const visible = entries.filter((e) => e.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)
  if (!visible.length) return
  lastNav = SPY[(visible[0].target as HTMLElement).id]; applyNav(lastNav)
}, { rootMargin: `-${72 + 60}px 0px -55% 0px`, threshold: 0 })
spyTargets.forEach((t) => spy.observe(t))
document.addEventListener('click', (ev) => { if ((ev.target as HTMLElement).closest('[data-nav]')) spyLock = Date.now() + 900 })

// an anchor whose target has no box scrolls nowhere: #swap is display:contents on wide screens (its two cards sit in the DEX
// grid), so the rail's Swap link and a #swap deep link did nothing above 1180px. Resolve such a target to its first boxed child.
function anchorBox(hash: string): Element | null {
  let el: Element | null = null; try { el = document.querySelector(hash.split('=')[0]) } catch { return null } if (!el) return null
  return el.getClientRects().length ? el : (Array.from(el.children).find((c) => c.getClientRects().length) ?? el)
}
document.addEventListener('click', (ev) => {
  const a = (ev.target as HTMLElement).closest('a[href^="#"]') as HTMLAnchorElement | null
  if (!a || a.getAttribute('href')!.length < 2) return
  const href = a.getAttribute('href')!; let el: Element | null = null; try { el = document.querySelector(href) } catch { return } if (!el || el.getClientRects().length) return
  const box = anchorBox(href); if (!box || box === el) return
  ev.preventDefault(); history.replaceState(null, '', a.getAttribute('href')); box.scrollIntoView({ behavior: 'smooth', block: 'start' })
})

// back-to-top after a screen of scrolling
const toTop = $('#toTop')
addEventListener('scroll', () => { toTop.hidden = scrollY < innerHeight * 0.9 }, { passive: true })
toTop.addEventListener('click', () => window.scrollTo({ top: 0, behavior: 'smooth' }))

// ── boot ──────────────────────────────────────────────────────────────────
try { const w = localStorage.getItem('sigilvm-wallet'); if (w) wallet = w } catch { /* */ }
try { const qw = new URLSearchParams(location.search).get('wallet'); if (qw && /^[0-9a-f]{64}$/i.test(qw)) { const w = qw.toLowerCase(); wallet = w; localStorage.setItem('sigilvm-wallet', w); if (innerWidth > 1100) setPanel(true, false); else setTimeout(() => { if (!app.classList.contains('panel-open')) toast(`Wallet ${w.slice(0, 6)}…${w.slice(-4)} connected — tap Wallet for balances and records.`) }, 400) } /* ≤1100px the panel is a drawer over the page: a visitor arriving from the gate should land on the market, not on a dialog */ else if (qw) { setTimeout(() => toast(`Ignored ?wallet=${qw.slice(0, 12)}${qw.length > 12 ? '…' : ''} — a SIGIL wallet id is 64 hex characters (this is ${qw.length}).`, 'warn'), 400) } } catch { /* */ }
poll()
setInterval(() => { if (!document.hidden) poll() }, 10_000)
document.addEventListener('visibilitychange', () => { if (!document.hidden) poll() })
document.querySelectorAll('.row').forEach((r) => r.addEventListener('wheel', (e) => { const we = e as WheelEvent; if (Math.abs(we.deltaY) > Math.abs(we.deltaX)) { (r as HTMLElement).scrollLeft += we.deltaY; e.preventDefault() } }, { passive: false }))
