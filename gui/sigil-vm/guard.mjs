#!/usr/bin/env node
// guard.mjs — layout guard for the ship gate. Serves dist/ through fluxc serve, loads the verify entry
// (index.html → live node via the site proxy) at phone/tablet/laptop/desktop widths in both themes and
// fails if anything overflows the viewport, the top bar overflows, the wallet button is off-screen, or
// the page logs an error. Usage: node guard.mjs  (exit 0 = pass)
import { spawn } from 'node:child_process'
import net from 'node:net'
import pkg from '/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/node_modules/playwright-core/index.js'
const { chromium } = pkg
// a FREE port every run — 8091 turned out to be another lane's fluxc serve, and the guard probed its page
const PORT = await new Promise((res) => { const srv = net.createServer(); srv.listen(0, '127.0.0.1', () => { const p = srv.address().port; srv.close(() => res(p)) }) })
const here = new URL('.', import.meta.url).pathname
const serve = spawn('/home/storage/deepseek-codewhale/flux/target/debug/fluxc', ['serve'], {
  env: { ...process.env, FLUX_STATIC_DIR: here + 'dist', FLUX_SERVE_HOST: '127.0.0.1', FLUX_SERVE_PORT: String(PORT), FLUX_SPA_FALLBACK: '0' },
  stdio: 'ignore', detached: true,
})
await new Promise((r) => setTimeout(r, 1500))
console.log(`guard serving dist on 127.0.0.1:${PORT}`)
const widths = [390, 820, 1024, 1440, 1600, 1920] // 1600 = the tightest panel-auto-open layout (main 1156px), where the trending table clipped on 09-16
const fails = []
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] })
try {
  for (const scheme of ['dark', 'light']) for (const w of widths) {
    const ctx = await browser.newContext({ viewport: { width: w, height: 900 }, colorScheme: scheme, isMobile: w < 760, hasTouch: w < 760 })
    // index.html pins the theme to dark when localStorage has none, so a fresh context ignores colorScheme —
    // every "light" pass had been rendering dark. Seed the persisted theme before any page script runs.
    await ctx.addInitScript((t) => { try { localStorage.setItem('sigilvm-theme', t) } catch {} }, scheme)
    const page = await ctx.newPage()
    const errs = []
    page.on('pageerror', (e) => errs.push('PAGEERR ' + e.message.slice(0, 120)))
    // the footer's wallet-manifest fetch (/downloads/…) 404s on the guard's dist-only server; that is not a page fault
    const bad404 = []
    page.on('response', (r) => { if (r.status() === 404 && !r.url().includes('/downloads/')) bad404.push(r.url().slice(0, 100)) })
    page.on('console', (m) => { if (m.type() === 'error' && !/status of 404/.test(m.text())) errs.push(m.text().slice(0, 120)) })
    await page.goto(`http://127.0.0.1:${PORT}/index.html`, { waitUntil: 'networkidle', timeout: 30000 }).catch((e) => errs.push('goto ' + e.message.slice(0, 80)))
    await page.waitForSelector('#featuredRow .card, #offline', { timeout: 20000 }).catch(() => errs.push('first poll never rendered within 20 s'))
    await page.waitForTimeout(600)
    const m = await page.evaluate(() => ({
      docW: document.documentElement.scrollWidth, topW: document.querySelector('.topbar')?.scrollWidth ?? 0,
      wallet: (document.querySelector('#walletBtn')?.getBoundingClientRect().right ?? 0) <= innerWidth,
      offline: !!document.getElementById('offline'), cards: document.querySelectorAll('#featuredRow .card').length,
      tokens: document.querySelectorAll('#tokens tbody tr[data-tok]').length,
      // hidden horizontal overflow inside a box that is NOT a deliberate carousel — how the trending table clipped at 1600px
      clipped: Array.from(document.querializeAll ? [] : document.querySelectorAll('*')).filter((e) => {
        const cs = getComputedStyle(e); if (!/auto|scroll|hidden|clip/.test(cs.overflowX)) return false
        if (e.scrollWidth <= e.clientWidth + 1) return false
        if (e.closest('.row, .cats, .ticker, .ttable, .hero, .card .img, .navlinks, .modal, .preview, .tabs')) return false
        if (/^(TD|TH|SPAN|SMALL|B|I|A|BUTTON|DIV)$/.test(e.tagName) && (cs.textOverflow === 'ellipsis')) return false // intentional ellipsis
        return true
      }).map((e) => `${e.tagName.toLowerCase()}.${String(e.className).split(' ')[0]} ${e.scrollWidth}>${e.clientWidth}`).slice(0, 6),
    }))
    // dialogs: open the widest collection sheet and the connect sheet, and look for anything poking out of the box
    // (tick 217: stat tiles ran 16px past the phone sheet under its own overflow clip)
    const dialogProbe = async (open) => {
      await page.evaluate(open); await page.waitForTimeout(350)
      const r = await page.evaluate(() => {
        const box = document.querySelector('#modal.open .box'); if (!box) return ['dialog did not open']
        const br = box.getBoundingClientRect(); const out = []
        if (br.right > innerWidth + 1 || br.left < -1) out.push(`box ${Math.round(br.left)}..${Math.round(br.right)} vs ${innerWidth}`)
        box.querySelectorAll('*').forEach((e) => { const rr = e.getBoundingClientRect(); if (rr.width > 0 && rr.right > br.right + 1 && !e.closest('.list, .cm-items')) out.push(`${e.tagName.toLowerCase()}.${String(e.className).split(' ')[0]} +${Math.round(rr.right - br.right)}px`) })
        return out.slice(0, 5)
      })
      await page.keyboard.press('Escape'); await page.waitForTimeout(150)
      return r
    }
    const dlg = [
      ...(await dialogProbe(() => { const c = [...document.querySelectorAll('#featuredRow .card')].find((x) => x.dataset.coll === 'earth') || document.querySelector('#featuredRow .card'); c?.click() })).map((x) => 'collection sheet: ' + x),
      ...(await dialogProbe(() => document.getElementById('walletBtn')?.click())).map((x) => 'connect sheet: ' + x),
    ]
    // text contrast: any visible text leaf under 3:1 against its nearest painted background (chips over artwork and
    // gradient-clipped text excluded). 3:1 is a floor to catch invisible text, not the AA target — that is audited by hand.
    const lowContrast = await page.evaluate(() => {
      const parse = (c) => { const m = c.match(/[\d.]+/g) || []; return { r: +m[0] || 0, g: +m[1] || 0, b: +m[2] || 0, a: m.length > 3 ? +m[3] : 1 } }
      const lum = ({ r, g, b }) => { const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4 }; return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b) }
      const blend = (fg, bg) => ({ r: fg.r * fg.a + bg.r * (1 - fg.a), g: fg.g * fg.a + bg.g * (1 - fg.a), b: fg.b * fg.a + bg.b * (1 - fg.a), a: 1 })
      // composite bottom-up from the body through every painted ancestor; a gradient/image background anywhere in the chain
      // means we cannot know the colour — return null and skip the leaf
      const bgOf = (e) => { const layers = []; let n = e; while (n && n !== document.documentElement) { const cs = getComputedStyle(n); if (cs.backgroundImage && cs.backgroundImage !== 'none') return null; const c = parse(cs.backgroundColor); if (c.a > 0) layers.push(c); n = n.parentElement } let acc = parse(getComputedStyle(document.body).backgroundColor); if (acc.a < 1) acc = blend(acc, { r: 255, g: 255, b: 255, a: 1 }); for (const c of layers.reverse()) acc = c.a >= 1 ? c : blend(c, acc); return acc }
      const out = []
      document.querySelectorAll('body *').forEach((e) => {
        if (e.children.length) return; const t = e.textContent.trim(); if (!t) return
        const cs = getComputedStyle(e); if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return
        if (cs.color === 'rgba(0, 0, 0, 0)' || cs.webkitBackgroundClip === 'text' || cs.backgroundClip === 'text') return
        if (e.closest('.card .img, .hero, .cm-head, .pv-img, .skip, .preview, .toast, #offline, .modal, .panel, .cats')) return
        const r = e.getBoundingClientRect(); if (r.width < 2 || r.height < 2 || r.bottom < 0 || r.top > innerHeight * 8) return
        const fg = parse(cs.color); if (fg.a < 0.5) return
        const bg = bgOf(e); if (!bg) return; const l1 = lum(fg.a < 1 ? blend(fg, bg) : fg), l2 = lum(bg); const ratio = (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05)
        if (ratio < 3) out.push(`${e.tagName.toLowerCase()}.${String(e.className).split(' ')[0]} "${t.slice(0, 18)}" ${ratio.toFixed(2)}:1`)
      })
      return out.slice(0, 6)
    })
    const bad = []
    if (lowContrast.length) bad.push('low contrast: ' + lowContrast.join(', '))
    if (dlg.length) bad.push(dlg.join('; '))
    if (m.docW > w) bad.push(`page overflows ${m.docW}/${w}`)
    if (m.topW > w) bad.push(`topbar overflows ${m.topW}/${w}`)
    if (!m.wallet) bad.push('wallet button off-screen')
    if (m.clipped.length) bad.push('clipped: ' + m.clipped.join(', '))
    if (m.offline) bad.push('offline banner (node unreachable from the guard)')
    if (m.cards < 1 || m.tokens < 1) bad.push(`empty sections cards=${m.cards} tokens=${m.tokens}`)
    if (bad404.length) bad.push('404: ' + bad404.join(', '))
    if (errs.length) bad.push('errors: ' + errs.join(' | '))
    console.log(`${bad.length ? '✗' : '✓'} ${scheme} ${w}px${bad.length ? ' — ' + bad.join('; ') : ''}`)
    if (bad.length) fails.push(`${scheme}@${w}: ${bad.join('; ')}`)
    await ctx.close()
  }
} finally {
  await browser.close()
  try { process.kill(-serve.pid) } catch {}
}
if (fails.length) { console.log('GUARD FAILED'); process.exit(1) }
console.log('GUARD PASSED')
