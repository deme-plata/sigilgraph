// showcase.ts — put the live SIGIL VM inside a frame image. The frame is drawn full-bleed (letterboxed to
// its own aspect), the screen rectangle is a fraction of the frame, and the real page renders inside it as
// an iframe laid out at desktop width and scaled down, so it stays fully interactive.
import './showcase.css'

interface Frame { name: string; src: string; w: number; h: number; screen: { x: number; y: number; w: number; h: number }; layout: number }
// Screen rectangles are measured on the frame image (see gui/sigil-vm/frames.md): pixel box of the empty screen,
// inset a few px so the neon bezel line stays visible on top of the page.
const FRAMES: Record<string, Frame> = {
  neon: { name: 'Neon bezel', src: '/frames/sigil-vm-bezel-neon.png', w: 1536, h: 1024, screen: { x: 168, y: 119, w: 1189, h: 784 }, layout: 1440 },
}

const q = new URLSearchParams(location.search)
const frameKey = q.get('frame') && FRAMES[q.get('frame')!] ? q.get('frame')! : 'neon'
const frame = FRAMES[frameKey]
// ?screen=x,y,w,h overrides the measured box (calibration); ?layout=1280 changes the page's virtual width
const ov = (q.get('screen') || '').split(',').map(Number)
if (ov.length === 4 && ov.every((n) => isFinite(n) && n > 0)) frame.screen = { x: ov[0], y: ov[1], w: ov[2], h: ov[3] }
const layoutW = Number(q.get('layout')) > 400 ? Number(q.get('layout')) : frame.layout
const appSrc = q.get('app') || '/sigil-vm.html'

const stage = document.getElementById('stage')!
const img = document.getElementById('frame') as HTMLImageElement
const screen = document.getElementById('screen')!
const app = document.getElementById('app') as HTMLIFrameElement
const calib = document.getElementById('calib')!
const hud = document.getElementById('hudInfo')!
img.src = frame.src
app.src = appSrc + (appSrc.includes('?') ? '&' : '?') + 'embed=1'

function fit(): void {
  // letterbox the frame into the viewport, keeping its aspect
  const vw = innerWidth, vh = innerHeight
  const k = Math.min(vw / frame.w, vh / frame.h)
  const W = Math.round(frame.w * k), H = Math.round(frame.h * k)
  stage.style.width = W + 'px'; stage.style.height = H + 'px'
  const s = frame.screen
  const sx = s.x * k, sy = s.y * k, sw = s.w * k, sh = s.h * k
  screen.style.left = sx + 'px'; screen.style.top = sy + 'px'; screen.style.width = sw + 'px'; screen.style.height = sh + 'px'
  // the page is laid out at layoutW CSS px and scaled into the screen width
  const scale = sw / layoutW
  app.style.width = layoutW + 'px'; app.style.height = Math.round(sh / scale) + 'px'; app.style.transform = `scale(${scale})`
  calib.style.left = sx + 'px'; calib.style.top = sy + 'px'; calib.style.width = sw + 'px'; calib.style.height = sh + 'px'
  calib.dataset.txt = `screen=${s.x},${s.y},${s.w},${s.h}  scale ${scale.toFixed(3)}`
  hud.textContent = `${frame.name} · ${frame.w}×${frame.h} · page at ${layoutW}px × ${scale.toFixed(2)}`
}
fit()
addEventListener('resize', fit)

// calibration: press k to show the box, arrows move it, shift+arrows resize, c copies ?screen=
let calibOn = false
addEventListener('keydown', (e) => {
  if (e.key === 'k') { calibOn = !calibOn; calib.hidden = !calibOn; return }
  if (!calibOn) return
  const s = frame.screen; const d = e.altKey ? 10 : 1
  if (e.key === 'ArrowLeft') { if (e.shiftKey) s.w -= d; else s.x -= d }
  if (e.key === 'ArrowRight') { if (e.shiftKey) s.w += d; else s.x += d }
  if (e.key === 'ArrowUp') { if (e.shiftKey) s.h -= d; else s.y -= d }
  if (e.key === 'ArrowDown') { if (e.shiftKey) s.h += d; else s.y += d }
  if (e.key === 'c') { navigator.clipboard?.writeText(`?screen=${s.x},${s.y},${s.w},${s.h}`) }
  if (e.key.startsWith('Arrow')) { e.preventDefault(); fit() }
})
