// UI self-test: how good is the generated mural UI vs the rubric?
// Headless Chromium renders the built dist, samples pixels + DOM, scores /100.
// Run: node test/ui-score.mjs   (uses shared sigil-wallet node_modules/playwright-chromium)
import { chromium } from 'playwright-chromium'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const __dir = dirname(fileURLToPath(import.meta.url))
const rubric = JSON.parse(readFileSync(resolve(__dir, '../src/rubric.json'), 'utf8'))
const distIndex = 'file://' + resolve(__dir, '../dist/index.html')

const TARGETS = rubric.ground_truth.signature_palette
const dist = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2])

// file:// + ES modules need these flags (origin null otherwise blocks the module fetch)
const browser = await chromium.launch({ args: ['--allow-file-access-from-files', '--disable-web-security'] })
const page = await browser.newPage({ viewport: { width: 1360, height: 900 } })
const errs = []
page.on('console', m => { if (m.type() === 'error') errs.push(m.text()) })
page.on('pageerror', e => errs.push(String(e)))

await page.goto(distIndex, { waitUntil: 'networkidle' })
await page.waitForFunction('window.__mural && window.__mural.ready', { timeout: 8000 }).catch(() => {})
await page.waitForTimeout(400)

// ── pixel analysis done inside the page (avoid shipping 13MB imagedata back) ──
const px = await page.evaluate((TARGETS) => {
  const cv = document.getElementById('mural')
  const c = cv.getContext('2d')
  const W = cv.width, H = cv.height
  const img = c.getImageData(0, 0, W, H).data
  const d = (r, g, b, t) => Math.hypot(r - t[0], g - t[1], b - t[2])
  const counts = {}, names = Object.keys(TARGETS)
  for (const n of names) counts[n] = 0
  let bright = 0, darkPurple = 0, samples = 0
  const red = { tl: 0, tr: 0, bl: 0, br: 0, minX: W, maxX: 0 }
  const step = 6
  for (let y = 0; y < H; y += step) for (let x = 0; x < W; x += step) {
    const i = (y * W + x) * 4, r = img[i], g = img[i + 1], b = img[i + 2]
    samples++
    const lum = 0.2126 * r + 0.7152 * g + 0.0722 * b
    if (lum > 180) bright++
    if (r < 60 && g < 50 && b > 25 && b < 90) darkPurple++
    let best = 9999, bn = null
    for (const n of names) { const dd = d(r, g, b, TARGETS[n]); if (dd < best) { best = dd; bn = n } }
    if (best < 70) {
      counts[bn]++
      if (bn === 'red') {
        red.minX = Math.min(red.minX, x); red.maxX = Math.max(red.maxX, x)
        const top = y < H / 2, left = x < W / 2
        if (top && left) red.tl++; else if (top && !left) red.tr++; else if (!top && left) red.bl++; else red.br++
      }
    }
  }
  return { W, H, samples, counts, bright, darkPurple, red }
}, TARGETS)

// ── DOM analysis ──
const dom = await page.evaluate(() => ({
  bodyText: document.body.innerText,
  legendCount: document.querySelectorAll('#legend .lg').length,
  panels: (window.__mural && window.__mural.panels) ? window.__mural.panels.length : 0,
  terms: (window.__mural && window.__mural.panels) ? window.__mural.panels.map(p => p.term) : [],
}))

// ── animation: frame advances + render differs over 600ms ──
const f0 = await page.evaluate(() => window.__mural.frame)
const shot0 = await page.screenshot()
await page.waitForTimeout(600)
const f1 = await page.evaluate(() => window.__mural.frame)
const shot1 = await page.screenshot()
const shotDiffers = Buffer.compare(shot0, shot1) !== 0

await page.screenshot({ path: resolve(__dir, '../dist/_uiscore.png'), fullPage: true })
await browser.close()

// ── score each criterion ──
const results = []
const add = (id, ok, got, partial = null) => {
  const crit = rubric.criteria.find(c => c.id === id)
  const earned = partial != null ? Math.round(crit.weight * partial) : (ok ? crit.weight : 0)
  results.push({ id, weight: crit.weight, earned, got })
}

const paletteHits = Object.entries(px.counts).filter(([, n]) => n > 3).map(([k]) => k)
add('palette', null, `${paletteHits.length}/6 colors [${paletteHits.join(',')}], darkPurple=${px.darkPurple}`,
  Math.min(1, paletteHits.length / 4) * (px.darkPurple > 50 ? 1 : 0.6))

add('panels8', dom.panels === 8 && dom.legendCount === 8, `panels=${dom.panels} legend=${dom.legendCount}`)

const r = px.red
const continuity = (r.tl + r.bl > 2) && (r.tr + r.br > 2) && (r.tl + r.tr > 2) && (r.bl + r.br > 2)
  && r.minX < px.W / 3 && r.maxX > 2 * px.W / 3
add('redline', continuity, `quadrants tl${r.tl} tr${r.tr} bl${r.bl} br${r.br}, span ${r.minX}->${r.maxX}/${px.W}`)

add('frame', px.counts.gold > 20 && /SIGIL/.test(dom.bodyText), `gold=${px.counts.gold} hasSIGIL=${/SIGIL/.test(dom.bodyText)}`)
add('equation', /δS_SIGIL/.test(dom.bodyText) && /=\s*0/.test(dom.bodyText), `eqn present=${/δS_SIGIL/.test(dom.bodyText)}`)
add('richness', null, `bright=${px.bright}/${px.samples}`, Math.min(1, px.bright / 1500))
add('animation', f1 > f0 && shotDiffers, `frame ${f0}->${f1}, render differs=${shotDiffers}`)

const want = ['δS/δg', 'δS/δA', 'δS/δφ', 'δS/δJ', 'δS/δK', 'δS/δΣ', '∮ fold', 'Σ → ∞']
const haveTerms = want.filter(t => dom.terms.includes(t))
add('termlabels', haveTerms.length === 8, `${haveTerms.length}/8 terms`)

const total = results.reduce((s, r) => s + r.earned, 0)
const band = Object.entries(rubric.grade_bands).find(([, v]) => total >= v)?.[0] ?? 'F'

console.log('\n  SIGIL Mural — UI fidelity self-test')
console.log('  ' + '─'.repeat(64))
for (const r of results) {
  const bar = '█'.repeat(Math.round(r.earned / r.weight * 10)).padEnd(10, '·')
  console.log(`  ${r.id.padEnd(11)} ${String(r.earned).padStart(2)}/${String(r.weight).padEnd(2)} ${bar}  ${r.got}`)
}
console.log('  ' + '─'.repeat(64))
console.log(`  TOTAL  ${total}/100   grade ${band}` + (errs.length ? `   ⚠ ${errs.length} console errors` : '   ✓ no console errors'))
if (errs.length) errs.slice(0, 5).forEach(e => console.log('    ! ' + e))
console.log('  screenshot: dist/_uiscore.png\n')
process.exit(total >= rubric.grade_bands.C ? 0 : 1)
