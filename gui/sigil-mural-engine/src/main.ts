// SIGIL Mural Engine — animated Canvas2D rendering of the 8-piece master-action mural.
// Each panel = one Euler–Lagrange term of δS_SIGIL[g,A,φ,J,K,Σ]=0, threaded by one
// continuous red line (the stationary worldline / the fabric's +10% thread).
// Static composition is drawn once to an offscreen buffer; only the red-line flow animates.

const W = 2520, H = 1320
const PAL = {
  bg0: '#05030c', bg1: '#160c28', gold: '#f5c542', goldHi: '#fff0bf',
  mauve: '#b48ead', cyan: '#88c0d0', green: '#a3be8c', purple: '#5e3a8c',
  red: '#ff2d55', ink: '#e8e3f5', node: '#88c0d0',
}

type Panel = { n: number; term: string; name: string; blurb: string; x: number; y: number; draw: (c: CanvasRenderingContext2D) => void }

// 4×2 inner grid inside the 60px frame inset; each cell 600×600
const CW = 600, CH = 600, OX = 60, OY = 60
const cell = (col: number, row: number) => ({ x: OX + col * CW, y: OY + row * CH })

function mono(c: CanvasRenderingContext2D, s: string, x: number, y: number, size: number, fill: string) {
  c.font = `${size}px "JetBrains Mono", ui-monospace, monospace`; c.fillStyle = fill; c.fillText(s, x, y)
}
function sans(c: CanvasRenderingContext2D, s: string, x: number, y: number, size: number, fill: string) {
  c.font = `${size}px Inter, system-ui, sans-serif`; c.fillStyle = fill; c.fillText(s, x, y)
}
function glow(c: CanvasRenderingContext2D, color: string, blur: number, fn: () => void) {
  c.save(); c.shadowColor = color; c.shadowBlur = blur; fn(); c.restore()
}
function header(c: CanvasRenderingContext2D, p: Panel) {
  mono(c, p.term, 28, 48, 22, PAL.gold)
  sans(c, p.name, 28 + c.measureText(p.term).width + 18, 46, 19, PAL.mauve)
}

const panels: Panel[] = [
  { n: 1, term: 'δS/δg', name: 'The Braid', blurb: 'BlockDAG geometry — a woven lattice, not one chain.',
    ...cell(0, 0),
    draw(c) {
      header(c, this as Panel)
      glow(c, PAL.cyan, 14, () => {
        c.lineWidth = 3; c.strokeStyle = PAL.cyan
        c.beginPath(); c.moveTo(40, 300); c.bezierCurveTo(150, 200, 250, 400, 360, 300); c.bezierCurveTo(470, 200, 540, 400, 580, 320); c.stroke()
        c.strokeStyle = PAL.green; c.beginPath(); c.moveTo(40, 360); c.bezierCurveTo(150, 460, 250, 260, 360, 360); c.bezierCurveTo(470, 460, 540, 260, 580, 340); c.stroke()
        c.strokeStyle = PAL.mauve; c.beginPath(); c.moveTo(40, 330); c.lineTo(580, 330); c.stroke()
      })
      for (const [x, y, r] of [[120, 250, 12], [250, 350, -8], [360, 300, 18], [470, 360, -14], [540, 320, 6]] as number[][]) {
        c.save(); c.translate(x + 13, y + 13); c.rotate(r * Math.PI / 180); c.fillStyle = PAL.gold; c.strokeStyle = PAL.bg0; c.lineWidth = 1.5
        c.fillRect(-13, -13, 26, 26); c.strokeRect(-13, -13, 26, 26); c.restore()
      }
      mono(c, 'parents → child · woven, not linear', 40, 540, 12, '#8a82ad')
    } },
  { n: 2, term: 'δS/δA', name: 'Emission', blurb: 'Mint gauge under the 21M cap — committed in state roots.',
    ...cell(1, 0),
    draw(c) {
      header(c, this as Panel)
      c.fillStyle = 'rgba(136,192,208,0.12)'; c.beginPath(); c.moveTo(40, 460); c.bezierCurveTo(140, 300, 230, 250, 330, 250); c.lineTo(580, 250); c.lineTo(580, 480); c.lineTo(40, 480); c.closePath(); c.fill()
      glow(c, PAL.cyan, 12, () => { c.lineWidth = 4; c.strokeStyle = PAL.cyan; c.beginPath(); c.moveTo(40, 460); c.bezierCurveTo(140, 300, 230, 250, 330, 250); c.lineTo(580, 250); c.stroke() })
      c.setLineDash([6, 6]); c.strokeStyle = PAL.gold; c.lineWidth = 2; c.beginPath(); c.moveTo(40, 245); c.lineTo(580, 245); c.stroke(); c.setLineDash([])
      mono(c, '21,000,000 cap', 360, 236, 14, PAL.gold)
      c.fillStyle = 'rgba(163,190,140,0.7)'
      const bars = [[70, 430, 40], [120, 400, 70], [170, 380, 90], [220, 370, 100], [270, 365, 105], [320, 362, 108]]
      for (const [x, y, h] of bars) c.fillRect(x, y, 14, h)
      mono(c, 'checkpointed in state roots', 40, 540, 12, '#8a82ad')
    } },
  { n: 3, term: 'δS/δφ', name: 'Settlement', blurb: 'Sends, swaps, DEX — every transfer a ripple in φ.',
    ...cell(2, 0),
    draw(c) {
      header(c, this as Panel)
      c.strokeStyle = 'rgba(180,142,173,0.55)'; c.lineWidth = 2
      for (const p of [[90, 300, 300, 180, 510, 300], [90, 360, 300, 500, 510, 360], [120, 250, 300, 330, 480, 420], [120, 420, 300, 330, 480, 250]])
        { c.beginPath(); c.moveTo(p[0], p[1]); c.quadraticCurveTo(p[2], p[3], p[4], p[5]); c.stroke() }
      const grd = (x: number, y: number, r: number) => { const g = c.createRadialGradient(x, y, 0, x, y, r); g.addColorStop(0, '#eaf6ff'); g.addColorStop(.6, PAL.node); g.addColorStop(1, 'rgba(136,192,208,0)'); return g }
      glow(c, PAL.cyan, 14, () => { for (const [x, y, r] of [[90, 330, 22], [510, 330, 22], [300, 200, 16], [300, 460, 16]]) { c.fillStyle = grd(x, y, r); c.beginPath(); c.arc(x, y, r, 0, 7); c.fill() } })
      c.fillStyle = PAL.gold; for (const [x, y, r] of [[240, 252, 5], [360, 252, 5], [300, 330, 6]]) { c.beginPath(); c.arc(x, y, r, 0, 7); c.fill() }
      mono(c, 'SEND · SWAP · DEX', 200, 210, 13, PAL.cyan)
      mono(c, 'sends · swaps · DEX', 40, 540, 12, '#8a82ad')
    } },
  { n: 4, term: 'δS/δJ', name: 'Witness Current', blurb: 'DagKnight leaderless ordering picks the canonical braid.',
    ...cell(3, 0),
    draw(c) {
      header(c, this as Panel)
      c.strokeStyle = 'rgba(136,192,208,0.8)'; c.lineWidth = 2.5
      for (const [x1, y1, x2, y2] of [[80, 200, 290, 320], [80, 460, 290, 340], [520, 200, 320, 320], [520, 460, 320, 340]])
        { c.beginPath(); c.moveTo(x1, y1); c.lineTo(x2, y2); c.stroke() }
      glow(c, PAL.gold, 16, () => { c.strokeStyle = PAL.gold; c.lineWidth = 3; c.beginPath(); for (const [i, p] of [[300, 250], [360, 300], [340, 380], [260, 380], [240, 300]].entries()) i ? c.lineTo(p[0], p[1]) : c.moveTo(p[0], p[1]); c.closePath(); c.stroke() })
      mono(c, '♞', 282, 352, 34, PAL.gold)
      mono(c, 'observe→weigh→agree→order', 40, 540, 12, '#8a82ad')
    } },
  { n: 5, term: 'δS/δK', name: 'Topology', blurb: 'Gossip mesh — one unique peer_id per node, or it dies.',
    ...cell(3, 1),
    draw(c) {
      header(c, this as Panel)
      const nodes = [[120, 200], [480, 210], [220, 430], [430, 440], [300, 300]]
      c.strokeStyle = 'rgba(163,190,140,0.5)'; c.lineWidth = 1.5
      for (const [a, b] of [[0, 4], [4, 1], [0, 2], [2, 4], [1, 3], [3, 4], [2, 3]]) { c.beginPath(); c.moveTo(nodes[a][0], nodes[a][1]); c.lineTo(nodes[b][0], nodes[b][1]); c.stroke() }
      glow(c, PAL.cyan, 12, () => { for (const [i, [x, y]] of nodes.entries()) { const r = i === 4 ? 17 : 13; const g = c.createRadialGradient(x, y, 0, x, y, r); g.addColorStop(0, '#eaf6ff'); g.addColorStop(.6, PAL.node); g.addColorStop(1, 'rgba(136,192,208,0)'); c.fillStyle = g; c.beginPath(); c.arc(x, y, r, 0, 7); c.fill() } })
      mono(c, '12D3KooW…a1', 86, 188, 9, '#cdeefb'); mono(c, '…b2', 470, 200, 9, '#cdeefb')
      mono(c, 'one unique peer_id per node', 40, 540, 12, '#8a82ad')
    } },
  { n: 6, term: 'δS/δΣ', name: 'Crypto Triskelion', blurb: 'SQIsign ⊕ RLWE ⊕ BLAKE — break one, two still hold.',
    ...cell(2, 1),
    draw(c) {
      header(c, this as Panel)
      glow(c, PAL.mauve, 14, () => {
        c.lineWidth = 5
        c.strokeStyle = PAL.mauve; c.beginPath(); c.arc(245, 300, 95, 0, 7); c.stroke()
        c.strokeStyle = PAL.cyan; c.beginPath(); c.arc(355, 300, 95, 0, 7); c.stroke()
        c.strokeStyle = PAL.green; c.beginPath(); c.arc(300, 385, 95, 0, 7); c.stroke()
      })
      mono(c, 'isogeny', 158, 305, 12, '#d9b8ff'); mono(c, 'lattice', 382, 305, 12, '#bfeeff'); mono(c, 'hash', 272, 470, 12, '#c7e0b0')
      mono(c, 'SQIsign ⊕ RLWE ⊕ BLAKE', 40, 545, 12, '#8a82ad')
    } },
  { n: 7, term: '∮ fold', name: 'The Fold', blurb: 'Join at height N — verify one constant fold, skip replay.',
    ...cell(1, 1),
    draw(c) {
      header(c, this as Panel)
      c.fillStyle = 'rgba(94,58,140,0.55)'
      for (let i = 0; i < 8; i++) c.fillRect(60 + (i % 3) * 30, 200 + Math.floor(i / 3) * 30 + (i % 2) * 15, 18, 18)
      c.strokeStyle = 'rgba(180,142,173,0.6)'; c.lineWidth = 2
      c.beginPath(); c.moveTo(150, 250); c.bezierCurveTo(260, 250, 300, 250, 360, 290); c.stroke()
      c.beginPath(); c.moveTo(150, 290); c.bezierCurveTo(260, 290, 300, 330, 360, 330); c.stroke()
      glow(c, PAL.gold, 14, () => { c.fillStyle = PAL.gold; c.strokeStyle = '#fff'; c.lineWidth = 1.5; c.beginPath(); for (const [i, p] of [[430, 260], [480, 300], [460, 370], [400, 370], [380, 300]].entries()) i ? c.lineTo(p[0], p[1]) : c.moveTo(p[0], p[1]); c.closePath(); c.fill(); c.stroke() })
      mono(c, 'one proof π', 380, 420, 13, PAL.gold)
      mono(c, 'light client verifies at height N', 40, 540, 12, '#8a82ad')
    } },
  { n: 8, term: 'Σ → ∞', name: 'The Forge & Horizon', blurb: '21M struck once at genesis; the line runs to the horizon.',
    ...cell(0, 1),
    draw(c) {
      header(c, this as Panel)
      const dawn = c.createRadialGradient(300, 600, 0, 300, 600, 480); dawn.addColorStop(0, '#ffd9a0'); dawn.addColorStop(.35, '#e0668a'); dawn.addColorStop(.7, PAL.purple); dawn.addColorStop(1, 'rgba(12,7,32,0)')
      c.fillStyle = dawn; c.globalAlpha = .5; c.fillRect(0, 120, 600, 480); c.globalAlpha = 1
      const forge = c.createRadialGradient(300, 430, 0, 300, 430, 120); forge.addColorStop(0, '#fff'); forge.addColorStop(.3, '#ffd76a'); forge.addColorStop(.7, '#ff8a3c'); forge.addColorStop(1, 'rgba(255,138,60,0)')
      c.fillStyle = forge; c.globalAlpha = .55; c.beginPath(); c.arc(300, 430, 120, 0, 7); c.fill(); c.globalAlpha = 1
      glow(c, PAL.gold, 16, () => { c.strokeStyle = PAL.gold; c.lineWidth = 4; c.save(); c.translate(300, 360); c.beginPath(); for (const [i, p] of [[0, -60], [52, -30], [52, 30], [0, 60], [-52, 30], [-52, -30]].entries()) i ? c.lineTo(p[0], p[1]) : c.moveTo(p[0], p[1]); c.closePath(); c.stroke(); c.beginPath(); c.moveTo(-26, -15); c.lineTo(0, 30); c.lineTo(26, -15); c.moveTo(-16, 5); c.lineTo(16, 5); c.lineWidth = 3; c.stroke(); c.restore() })
      mono(c, 'Day 200 · 2026-12-17', 150, 500, 13, '#ffd9a0')
      mono(c, '21M forged once · runs to the horizon', 40, 558, 12, '#ffcf9a')
    } },
]

// continuous red line through all 8 panels (boustrophedon), in mural coords
const RED_PATH = 'M-30,330 C160,210 300,470 470,330 C620,210 760,470 930,330 C1080,210 1230,470 1400,330 C1560,210 1720,470 1900,330 C2120,210 2300,300 2360,470 C2410,640 2240,760 2060,900 C1900,1020 1740,820 1560,900 C1380,980 1240,760 1060,900 C880,1020 740,800 560,900 C380,1000 240,780 60,900 C-40,960 -80,960 -40,1040'
const redPath = new Path2D(RED_PATH)

function drawStatic(c: CanvasRenderingContext2D) {
  // frame
  c.fillStyle = '#040208'; c.fillRect(0, 0, W, H)
  const fr = c.createLinearGradient(0, 0, W, H); fr.addColorStop(0, '#7a5414'); fr.addColorStop(.22, '#f7dd8a'); fr.addColorStop(.5, '#c98a1a'); fr.addColorStop(.78, '#f7dd8a'); fr.addColorStop(1, '#7a5414')
  c.strokeStyle = fr; c.lineWidth = 7; glow(c, PAL.gold, 10, () => c.strokeRect(40, 40, W - 80, H - 80))
  // art group at (60,60)
  c.save(); c.translate(OX, OY)
  const sp = c.createRadialGradient(1200, 0, 0, 1200, 0, 1400); sp.addColorStop(0, '#1d1238'); sp.addColorStop(.55, '#0c0720'); sp.addColorStop(1, PAL.bg0)
  c.fillStyle = sp; c.fillRect(0, 0, 2400, 1200)
  // starfield
  c.fillStyle = 'rgba(255,255,255,0.5)'
  for (const [x, y, r] of [[120, 90, 1.2], [430, 180, 1], [760, 60, 1.4], [1010, 140, 1], [1330, 80, 1.2], [1620, 160, 1], [1950, 70, 1.3], [2230, 150, 1], [300, 520, 1], [680, 640, 1.2], [1180, 560, 1], [1500, 660, 1.3], [1880, 540, 1], [2150, 640, 1.1], [900, 1100, 1.2], [1700, 1120, 1], [2300, 980, 1.3]])
    { c.beginPath(); c.arc(x, y, r, 0, 7); c.fill() }
  // stitch blooms (screen blend) to dissolve seams
  c.globalCompositeOperation = 'screen'
  const bloom = (x: number, y: number, r: number, col: string, a: number) => { const g = c.createRadialGradient(x, y, 0, x, y, r); g.addColorStop(0, col); g.addColorStop(1, 'rgba(0,0,0,0)'); c.fillStyle = g; c.globalAlpha = a; c.beginPath(); c.arc(x, y, r, 0, 7); c.fill(); c.globalAlpha = 1 }
  bloom(1200, 600, 600, 'rgba(217,184,255,0.7)', .5)
  for (const [x, y] of [[600, 600], [1800, 600], [600, 300], [1200, 300], [1800, 300], [600, 900], [1200, 900], [1800, 900]]) bloom(x, y, 240, 'rgba(245,197,66,0.5)', .35)
  c.globalCompositeOperation = 'source-over'
  // panels
  for (const p of panels) { c.save(); c.translate(p.x - OX, p.y - OY); p.draw(c); c.restore() }
  // vignette
  const vg = c.createRadialGradient(1200, 600, 0, 1200, 600, 1488); vg.addColorStop(0, 'rgba(0,0,0,0)'); vg.addColorStop(.72, 'rgba(0,0,0,0)'); vg.addColorStop(1, 'rgba(0,0,0,0.55)')
  c.fillStyle = vg; c.fillRect(0, 0, 2400, 1200)
  // base red line (static, in art coords; path is mural-coord so undo translate)
  c.translate(-OX, -OY)
  glow(c, PAL.red, 22, () => { c.strokeStyle = PAL.red; c.lineWidth = 5; c.lineCap = 'round'; c.stroke(redPath) })
  c.restore()
  // cartouches
  c.fillStyle = '#0a0614'; c.strokeStyle = fr; c.lineWidth = 3
  c.beginPath(); c.roundRect(1010, 26, 500, 46, 23); c.fill(); c.stroke()
  c.textAlign = 'center'; mono(c, 'S I G I L', 1260, 56, 22, PAL.gold); c.textAlign = 'left'
}

const canvas = document.getElementById('mural') as HTMLCanvasElement
const ctx = canvas.getContext('2d')!
const buf = document.createElement('canvas'); buf.width = W; buf.height = H
drawStatic(buf.getContext('2d')!)

let frame = 0
function tick() {
  frame++
  ctx.clearRect(0, 0, W, H)
  ctx.drawImage(buf, 0, 0)
  // animated white flow dashes riding the red line (the worldline "moving")
  ctx.save(); ctx.translate(OX, OY); ctx.translate(-OX, -OY)
  ctx.strokeStyle = 'rgba(255,255,255,0.8)'; ctx.lineWidth = 2; ctx.lineCap = 'round'
  ctx.setLineDash([2, 40]); ctx.lineDashOffset = -(frame * 1.6) % 420
  glow(ctx, '#fff', 6, () => ctx.stroke(redPath))
  ctx.setLineDash([]); ctx.restore()
  ;(window as any).__mural.frame = frame
  requestAnimationFrame(tick)
}

// fill DOM legend from panel specs (also what the UI-score test reads)
const leg = document.getElementById('legend')!
leg.innerHTML = panels.map(p => `<div class="lg"><b>${p.n} · ${p.term}</b><span>${p.name} — ${p.blurb}</span></div>`).join('')

// expose hook for the headless UI-score test
;(window as any).__mural = { ready: true, frame: 0, panels: panels.map(p => ({ n: p.n, term: p.term, name: p.name })), palette: PAL, w: W, h: H }

requestAnimationFrame(tick)
