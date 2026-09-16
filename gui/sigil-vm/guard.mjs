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
const widths = [390, 820, 1024, 1440, 1920]
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
    }))
    const bad = []
    if (m.docW > w) bad.push(`page overflows ${m.docW}/${w}`)
    if (m.topW > w) bad.push(`topbar overflows ${m.topW}/${w}`)
    if (!m.wallet) bad.push('wallet button off-screen')
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
