#!/usr/bin/env node
// snapshot.mjs — pre-render a plain-text summary of the live page INTO dist/sigil-vm.html so a fetcher that
// does not run JavaScript (ChatGPT's browser, search engines, screen readers, curl) still gets the real
// headings, collections, drops, tokens and chain figures. The script replaces it with the live app on boot.
import { spawn } from 'node:child_process'
import net from 'node:net'
import fs from 'node:fs'
import pkg from '/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/node_modules/playwright-core/index.js'
const { chromium } = pkg
const here = new URL('.', import.meta.url).pathname
const PORT = await new Promise((res) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)) }) })
const serve = spawn('/home/storage/deepseek-codewhale/flux/target/debug/fluxc', ['serve'], {
  env: { ...process.env, FLUX_STATIC_DIR: here + 'dist', FLUX_SERVE_HOST: '127.0.0.1', FLUX_SERVE_PORT: String(PORT), FLUX_SPA_FALLBACK: '0' }, stdio: 'ignore', detached: true,
})
await new Promise((r) => setTimeout(r, 1500))
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] })
let html = ''
try {
  const page = await (await browser.newContext({ viewport: { width: 1440, height: 1000 }, colorScheme: 'dark' })).newPage()
  await page.goto(`http://127.0.0.1:${PORT}/index.html`, { waitUntil: 'networkidle', timeout: 30000 })
  await page.waitForSelector('#featuredRow .card', { timeout: 20000 })
  await page.waitForTimeout(800)
  html = await page.evaluate(() => {
    // 'after next poll' promises a refresh the static copy will never make — say when the figure was taken instead
    const t = (el) => (el?.textContent || '').replace(/\s+/g, ' ').trim().replace(/\b(rate|age) after next poll\b/g, '$1 not sampled at deploy')
    const esc = (s) => s.replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]))
    const strip = [...document.querySelectorAll('#strip .cell')].map((c) => `<li><b>${esc(t(c.querySelector('.k')))}</b> ${esc(t(c.querySelector('.v')))} <i>${esc(t(c.querySelector('.s')))}</i></li>`).join('')
    const fy = [...document.querySelectorAll('#foryou .fy-col p')].map((p) => `<p>${esc(t(p))}</p>`).join('')
    const next = [...document.querySelectorAll('#foryou .fy-next li')].map((l) => `<li>${esc(t(l))}</li>`).join('')
    const kv = (c) => [...c.querySelectorAll('.kv > div')].map((d) => `${t(d.querySelector('.k'))} ${t(d.querySelector('.v, .delta'))}`).join(' · ')
    const cards = (sel) => [...document.querySelectorAll(sel)].map((c) => `<li><b>${esc(t(c.querySelector('.name')))}</b> ${esc(t(c.querySelector('.byline, .when')))} — ${esc(kv(c))}</li>`).join('')
    const trend = [...document.querySelectorAll('#trendingTable tbody tr')].map((r) => `<li>${esc(t(r.querySelector('.rank')))}. <b>${esc(t(r.querySelector('.n')))}</b> — ${[...r.querySelectorAll('td.num')].map(t).map(esc).join(' · ')}</li>`).join('')
    const tokens = [...document.querySelectorAll('#tokens tbody tr[data-tok]')].map((r) => `<li><b>${esc(t(r.querySelector('.sym')))}</b> — ${esc(t(r.querySelector('.nm')))}</li>`).join('')
    const h = t(document.querySelector('#chainHeight'))
    return `<div class="boot static" data-snapshot="${new Date().toISOString()}">
  <p class="static-note">Snapshot taken when this page was deployed (block ${esc(h)}). The live version loads with JavaScript and refreshes every 10 seconds from sigil-api.</p>
  <h1>SIGIL VM — Marketplace &amp; DEX</h1>
  <p>The marketplace for everything the SIGIL chain can prove: collections, drops, tokens and the native swap. Every figure is read from the node and labelled live, derived or pretend; nothing is a price until an oracle says so.</p>
  <h2>Chain</h2><ul>${strip}</ul>
  <h2>For you</h2>${fy}
  <h2>What's next</h2><ul>${next}</ul>
  <h2>Featured Collections</h2><ul>${cards('#featuredRow .card')}</ul>
  <h2>Featured Drops</h2><ul>${cards('#dropsRow .card')}</ul>
  <h2>Trending Collections</h2><ol>${trend}</ol>
  <h2>Top Movers Today</h2><ul>${cards('#moversRow .card')}</ul>
  <h2>Highest Weekly Sales</h2><ul>${cards('#salesRow .card')}</ul>
  <h2>Available Tokens</h2><ul>${tokens}</ul>
  <p><a href="/sigil-explorer.html">Explorer</a> · <a href="/api.html">API</a> · <a href="/sigil-wallet-tron-embedded.html">Wallet</a> · <a href="/downloads/sigil-vm-src.tar.gz">Page source</a></p>
</div>`
  })
} finally {
  await browser.close()
  try { process.kill(-serve.pid) } catch {}
}
if (!html || html.length < 2000) { console.log('snapshot: nothing rendered'); process.exit(1) }
const f = here + 'dist/sigil-vm.html'
let src = fs.readFileSync(f, 'utf8')
const re = /<div class="boot[^"]*"[\s\S]*?<\/div>\s*<\/div>|<div class="boot static"[\s\S]*?<\/div>\n<\/div>/
if (!/<div class="boot/.test(src)) { console.log('snapshot: boot div not found'); process.exit(1) }
// replace the boot placeholder (the 2-line skeleton) with the static summary
src = src.replace(/<div class="boot">[\s\S]*?<\/div>\s*<\/div>\s*<\/div>/, html + '\n  </div>')
fs.writeFileSync(f, src)
console.log(`snapshot: ${html.length} bytes of static summary written into dist/sigil-vm.html`)
