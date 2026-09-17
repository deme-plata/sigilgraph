#!/usr/bin/env node
// outage.mjs — degrade-honestly gate. Fails ONE node route family at a time (browser-side abort) and asserts the page
// (a) does not fall into the all-offline state, (b) prints no NaN, and (c) says "unread" / "did not answer" / "—"
// where that family's figures live — never a zero dressed as a measurement. One width, dark; ~70 s.
// Every phrase in DENY was actually printed by the page under that outage on 2026-09-16 before the fix.
import { spawn } from 'node:child_process'
import net from 'node:net'
import pkg from '/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/node_modules/playwright-core/index.js'
const { chromium } = pkg
const PORT = await new Promise((res) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)) }) })
const here = new URL('.', import.meta.url).pathname
const serve = spawn('/home/storage/deepseek-codewhale/flux/target/debug/fluxc', ['serve'], { env: { ...process.env, FLUX_STATIC_DIR: here + 'dist', FLUX_SERVE_HOST: '127.0.0.1', FLUX_SERVE_PORT: String(PORT), FLUX_SPA_FALLBACK: '0' }, stdio: 'ignore', detached: true })
await new Promise((r) => setTimeout(r, 1200))
// family → [selector whose text must admit the gap, deny-phrases that must not appear anywhere]
const FAMS = {
  mining: ['.strip, #foryou, .ticker', ['0 live rigs', '0 H/s', '0 rigs', '0 miners']],
  shielded: ['.strip, #foryou', ['0 notes', '0 sealed notes', '0 wallets have published', '0.00 SIGIL locked']],
  supply: ['.strip, #foryou', ['Supply 0.00 SIGIL', '0.00% of 21M', '0.00 of 21M']], // not bare '0.00 SIGIL': the bridge vault is legitimately 0.00 SIGIL
  finality: ['.strip, #foryou', ['h 0 ·', '0 validators', 'sits 19,']],
  nation: ['.strip', ['Treasury 0.00']],
  court: ['#trendingTable', ['1 honour', '1 seat']],
  earth: ['#trendingTable', []],
  rocky: ['#foryou', ['blocks to 19,150,000', '-5']],
  gauge: ['#foryou', ['live at block 0', 'at block 0']], // the ROCKY drop read 'live at block 0' when /v1/gauge failed (09-17)
  pools: ['#foryou, .qcard.route', ['holds 0 pools', '0 pools on g2', 'No liquidity pool exists']],
  dagknight: ['#trendingTable', []],
  bridge: ['#trendingTable', []],
  usds: ['#dropsRow', []],
  balance: ['.panel', ['Portfolio 0 SIGIL', 'Portfolio 0\n']], // with a wallet in the URL — the panel total once read '0 SIGIL' when /v1/balance failed
}
const WALLET_QS = '?wallet=490248e6068fd93a40ee6c11b3f04ce0b87f3d62da64299291396898b566a78b'
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] })
const fails = []
try {
  for (const [fam, [sel, deny]] of Object.entries(FAMS)) {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' })
    await ctx.addInitScript(() => { try { localStorage.setItem('sigilvm-theme', 'dark') } catch {} })
    const page = await ctx.newPage()
    await page.route(new RegExp('/v1/' + fam + '(/|$|\\?)'), (r) => r.abort('connectionrefused'))
    await page.goto(`http://127.0.0.1:${PORT}/index.html${fam === 'balance' ? WALLET_QS : ''}`, { waitUntil: 'networkidle', timeout: 30000 }).catch(() => {})
    await page.waitForSelector('#featuredRow .card, #offline', { timeout: 20000 }).catch(() => {})
    await page.waitForTimeout(800)
    const r = await page.evaluate(([sel]) => ({
      offline: !!document.getElementById('offline'), nan: document.body.innerText.includes('NaN'),
      text: document.body.innerText.replace(/\s+/g, ' ') + ' ' + [...document.querySelectorAll('.panel .total')].map((e) => e.innerText.replace(/\s+/g, ' ')).join(' '),
      admits: /unread|did not answer|—/.test([...document.querySelectorAll(sel)].map((e) => e.innerText).join(' ')),
      pretend: document.querySelectorAll('.chip.pretend, .chip.gold').length,
    }), [sel])
    const bad = []
    if (r.offline) bad.push('fell into the all-offline state')
    if (r.nan) bad.push('NaN printed')
    if (!r.admits) bad.push(`nothing in ${sel} admits the gap`)
    for (const d of deny) if (r.text.includes(d)) bad.push(`printed "${d}"`)
    console.log(`${bad.length ? '✗' : '✓'} ${fam} down${bad.length ? ' — ' + bad.join('; ') : ''}`)
    if (bad.length) fails.push(fam)
    await ctx.close()
  }
  // storage blocked (private mode): the page must still render live, with ?api= honoured, and log no errors
  {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' })
    await ctx.addInitScript(() => { const boom = () => { throw new DOMException('blocked', 'SecurityError') }; Object.defineProperty(window, 'localStorage', { get: boom }); Object.defineProperty(window, 'sessionStorage', { get: boom }) })
    const page = await ctx.newPage(); const errs = []
    page.on('pageerror', (e) => errs.push(e.message.slice(0, 120)))
    await page.goto(`http://127.0.0.1:${PORT}/index.html?api=http://127.0.0.1:8459`, { waitUntil: 'networkidle', timeout: 30000 }).catch(() => {})
    await page.waitForSelector('#featuredRow .card:not(.skel), #offline', { timeout: 20000 }).catch(() => {})
    const r = await page.evaluate(() => ({ cards: document.querySelectorAll('#featuredRow .card:not(.skel)').length, offline: !!document.getElementById('offline') }))
    const bad = []; if (r.offline || r.cards < 1) bad.push(`did not render live (cards=${r.cards}, offline=${r.offline})`); if (errs.length) bad.push('errors: ' + errs.join(' | '))
    console.log(`${bad.length ? '✗' : '✓'} storage blocked${bad.length ? ' — ' + bad.join('; ') : ''}`); if (bad.length) fails.push('storage')
    await ctx.close()
  }
  // print: light, no fixed chrome, hero flows (tick 285)
  {
    const ctx = await browser.newContext({ viewport: { width: 1000, height: 1200 }, colorScheme: 'dark' })
    const page = await ctx.newPage()
    await page.goto(`http://127.0.0.1:${PORT}/index.html`, { waitUntil: 'networkidle', timeout: 30000 }).catch(() => {})
    await page.waitForSelector('#featuredRow .card:not(.skel), #offline', { timeout: 20000 }).catch(() => {})
    await page.emulateMedia({ media: 'print' }); await page.waitForTimeout(300)
    const r = await page.evaluate(() => ({ bg: getComputedStyle(document.body).backgroundColor, bars: ['.topbar', '.rail', '.bottomnav', '.ticker', '.panel'].filter((s) => getComputedStyle(document.querySelector(s)).display !== 'none'), heroH: document.querySelector('.hero').getBoundingClientRect().height, h1: !!document.querySelector('.hero h1')?.getBoundingClientRect().height }))
    const bad = []; if (r.bg !== 'rgb(255, 255, 255)') bad.push('body not white: ' + r.bg); if (r.bars.length) bad.push('fixed chrome printed: ' + r.bars.join(',')); if (r.heroH < 100 || !r.h1) bad.push('hero collapsed in print')
    console.log(`${bad.length ? '✗' : '✓'} print${bad.length ? ' — ' + bad.join('; ') : ''}`); if (bad.length) fails.push('print')
    await ctx.close()
  }
} finally { await browser.close(); try { process.kill(-serve.pid) } catch {} }
if (fails.length) { console.log('OUTAGE GATE FAILED'); process.exit(1) }
console.log('OUTAGE GATE PASSED')
