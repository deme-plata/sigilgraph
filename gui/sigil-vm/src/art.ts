// art.ts — procedural cover art. Every collection card on OpenSea leads with
// an image; SIGIL's collections are on-chain record families with no image
// bytes, so the art is derived deterministically from the collection's name
// (a BLAKE-less FNV hash is plenty for a palette pick). Same input → same art,
// forever, with no asset to lose.

function fnv(s: string): number {
  let h = 0x811c9dc5
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0 }
  return h >>> 0
}

const PALETTES: [string, string, string][] = [
  ['#00D9FF', '#6B46C1', '#0A0B14'], // quantum cyan → purple
  ['#d4af37', '#7c3aed', '#0b0710'], // sigil gold → violet
  ['#00FF88', '#0080FF', '#03110c'], // green → blue
  ['#FF0080', '#6B46C1', '#12040c'], // pink → purple
  ['#f0d878', '#e0503c', '#120b05'], // gold-b → ember
  ['#5aa9e6', '#2D1B69', '#050914'], // sky → indigo
  ['#a78bfa', '#00D9FF', '#0a0716'], // lavender → cyan
  ['#2fbf71', '#d4af37', '#071009'], // ok green → gold
]

export type Glyph = 'eye' | 'braid' | 'coin' | 'shield' | 'earth' | 'rig' | 'block' | 'seal' | 'book' | 'drop' | 'token'

function glyphPath(g: Glyph): string {
  switch (g) {
    case 'eye': return '<path d="M20 60 Q60 20 100 60 Q60 100 20 60Z" fill="none" stroke="#fff" stroke-width="3"/><circle cx="60" cy="60" r="14" fill="#fff" opacity=".9"/><circle cx="60" cy="60" r="6" fill="#000"/>'
    case 'braid': return '<path d="M25 30 C60 30 60 90 95 90 M25 90 C60 90 60 30 95 30 M25 60 H95" fill="none" stroke="#fff" stroke-width="4" stroke-linecap="round"/>'
    case 'coin': return '<circle cx="60" cy="60" r="34" fill="none" stroke="#fff" stroke-width="4"/><circle cx="60" cy="60" r="22" fill="none" stroke="#fff" stroke-width="2" opacity=".7"/><text x="60" y="68" text-anchor="middle" font-size="22" font-family="Sora,Inter,sans-serif" font-weight="800" fill="#fff">S</text>'
    case 'shield': return '<path d="M60 22 L92 34 V62 C92 82 76 94 60 100 C44 94 28 82 28 62 V34Z" fill="none" stroke="#fff" stroke-width="4"/><path d="M46 60 L56 70 L76 48" fill="none" stroke="#fff" stroke-width="4" stroke-linecap="round"/>'
    case 'earth': return '<circle cx="60" cy="60" r="34" fill="none" stroke="#fff" stroke-width="3"/><ellipse cx="60" cy="60" rx="14" ry="34" fill="none" stroke="#fff" stroke-width="2" opacity=".8"/><path d="M26 60 H94 M32 42 H88 M32 78 H88" stroke="#fff" stroke-width="2" opacity=".6"/>'
    case 'rig': return '<rect x="26" y="34" width="68" height="18" rx="4" fill="none" stroke="#fff" stroke-width="3"/><rect x="26" y="58" width="68" height="18" rx="4" fill="none" stroke="#fff" stroke-width="3"/><circle cx="36" cy="43" r="3" fill="#fff"/><circle cx="36" cy="67" r="3" fill="#fff"/><path d="M50 43 H84 M50 67 H84" stroke="#fff" stroke-width="2" opacity=".6"/>'
    case 'block': return '<path d="M60 22 L92 40 V80 L60 98 L28 80 V40Z" fill="none" stroke="#fff" stroke-width="3"/><path d="M60 22 V60 M60 60 L92 40 M60 60 L28 40" stroke="#fff" stroke-width="2" opacity=".8"/>'
    case 'seal': return '<circle cx="60" cy="60" r="36" fill="none" stroke="#fff" stroke-width="3"/><path d="M60 28 L68 50 L92 52 L74 66 L80 90 L60 78 L40 90 L46 66 L28 52 L52 50Z" fill="#fff" opacity=".9"/>'
    case 'book': return '<path d="M30 30 H58 V92 H30 Z M62 30 H90 V92 H62 Z" fill="none" stroke="#fff" stroke-width="3"/><path d="M36 44 H52 M36 54 H52 M68 44 H84 M68 54 H84" stroke="#fff" stroke-width="2" opacity=".7"/>'
    case 'drop': return '<path d="M60 22 C60 22 32 54 32 70 A28 28 0 0 0 88 70 C88 54 60 22 60 22Z" fill="none" stroke="#fff" stroke-width="3"/><path d="M48 70 A12 12 0 0 0 60 82" fill="none" stroke="#fff" stroke-width="3" stroke-linecap="round"/>'
    case 'token': return '<circle cx="60" cy="60" r="34" fill="none" stroke="#fff" stroke-width="4"/><path d="M44 60 H76 M60 44 V76" stroke="#fff" stroke-width="4" stroke-linecap="round"/>'
  }
}

export function coverSvg(name: string, glyph: Glyph, opts: { w?: number; h?: number; symbol?: string } = {}): string {
  const w = opts.w ?? 400, h = opts.h ?? 400
  const seed = fnv(name)
  const [a, b, bg] = PALETTES[seed % PALETTES.length]
  const angle = (seed >> 3) % 360
  const rings = 3 + (seed % 3)
  let geom = ''
  for (let i = 0; i < rings; i++) {
    const r = 34 + i * 22
    geom += `<circle cx="60" cy="60" r="${r}" fill="none" stroke="#fff" stroke-opacity="${0.12 - i * 0.02}" stroke-width="1"/>`
  }
  const tri = `<path d="M60 8 L112 100 L8 100Z" fill="none" stroke="#fff" stroke-opacity=".10" stroke-width="1"/>
  <path d="M60 112 L8 20 L112 20Z" fill="none" stroke="#fff" stroke-opacity=".08" stroke-width="1"/>`
  const label = opts.symbol
    ? `<text x="60" y="108" text-anchor="middle" font-size="9" letter-spacing="2" font-family="JetBrains Mono,monospace" fill="#fff" opacity=".7">${opts.symbol}</text>`
    : ''
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 120" width="${w}" height="${h}" preserveAspectRatio="xMidYMid slice">
  <defs>
    <linearGradient id="g" gradientTransform="rotate(${angle} .5 .5)"><stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient>
    <radialGradient id="v" cx=".5" cy=".5" r=".7"><stop offset="0" stop-color="${bg}" stop-opacity="0"/><stop offset="1" stop-color="${bg}" stop-opacity=".85"/></radialGradient>
  </defs>
  <rect width="120" height="120" fill="url(#g)"/>
  ${tri}${geom}
  <rect width="120" height="120" fill="url(#v)"/>
  <g opacity=".95">${glyphPath(glyph)}</g>
  ${label}
</svg>`
  return 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(svg)
}

export function avatarSvg(seedText: string, letter: string): string {
  const seed = fnv(seedText)
  const [a, b] = PALETTES[seed % PALETTES.length]
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40"><defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="1"><stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient></defs><rect width="40" height="40" rx="10" fill="url(#g)"/><text x="20" y="26" text-anchor="middle" font-size="18" font-family="Sora,Inter,sans-serif" font-weight="800" fill="#fff">${letter}</text></svg>`
  return 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(svg)
}

export function tokenIconSvg(symbol: string): string {
  const map: Record<string, [string, string]> = {
    SIGIL: ['#d4af37', '#f0d878'],
    USDS: ['#2fbf71', '#0080FF'],
    ROCKY: ['#FF0080', '#6B46C1'],
    SSHARE: ['#00D9FF', '#6B46C1'],
    wSIGIL3: ['#8247e5', '#d4af37'],
  }
  const [a, b] = map[symbol] || PALETTES[fnv(symbol) % PALETTES.length]
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40"><defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="1"><stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient></defs><circle cx="20" cy="20" r="19" fill="url(#g)"/><circle cx="20" cy="20" r="14" fill="none" stroke="#fff" stroke-opacity=".5"/><text x="20" y="25" text-anchor="middle" font-size="13" font-family="Sora,Inter,sans-serif" font-weight="800" fill="#fff">${symbol.slice(0, 1)}</text></svg>`
  return 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(svg)
}
