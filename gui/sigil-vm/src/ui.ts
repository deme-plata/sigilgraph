// ui.ts — pure renderers: snapshot → HTML strings. No fetches here.
import { fmt, glyphsToSigil, hex } from './api'
import { I } from './icons'
import { avatarSvg } from './art'
import type { Snapshot, Collection, Drop, Mover, Sale, Token, Provenance } from './model'

const esc = (s: unknown): string => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c] as string))
const prov = (p: Provenance, small = true): string => `<span class="chip ${p}${small ? '' : ' big'}"><i class="d"></i>${p}</span>`
const delta = (n: number | null, d = 1): string => {
  if (n === null || n === undefined || !isFinite(n)) return '<span class="flat">—</span>'
  const c = n > 0.05 ? 'up' : n < -0.05 ? 'down' : 'flat'
  return `<span class="${c}">${fmt.pct(n, d)}</span>`
}
let sparkSeq = 0
export function sparkline(values: number[], w = 120, h = 32, cls = 'up'): string {
  let v = values.filter((x) => isFinite(x))
  if (v.length < 2) return ''
  // a series that spans more than 20× (a GPU rig joining a CPU network) flattens to a line at zero on a
  // linear scale — switch to log so both the baseline and the spike stay legible
  const pos = v.filter((x) => x > 0)
  if (pos.length > 1 && Math.max(...pos) / Math.min(...pos) > 20) v = v.map((x) => (x > 0 ? Math.log10(x) : Math.log10(Math.min(...pos))))
  const min = Math.min(...v), max = Math.max(...v), span = max - min || 1
  const pts = v.map((x, i) => `${((i / (v.length - 1)) * (w - 2) + 1).toFixed(1)},${(h - 2 - ((x - min) / span) * (h - 4)).toFixed(1)}`)
  const id = 'sp' + (++sparkSeq).toString(36)
  return `<svg class="spark ${cls}" viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" preserveAspectRatio="none"><defs><linearGradient id="${id}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="currentColor" stop-opacity=".35"/><stop offset="1" stop-color="currentColor" stop-opacity="0"/></linearGradient></defs><path d="M${pts[0]} L${pts.join(' L')} L${w - 1},${h - 1} L1,${h - 1}Z" fill="url(#${id})" stroke="none"/><polyline points="${pts.join(' ')}" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke"/><circle cx="${pts[pts.length - 1].split(',')[0]}" cy="${pts[pts.length - 1].split(',')[1]}" r="2" fill="currentColor"/></svg>`
}
// A block's age from height distance × the measured block rate (blocks carry no timestamp) — DERIVED, hence the ≈.
function blockAgo(s: Snapshot, height: number): string {
  const d = s.head.height - height
  if (d <= 0) return 'tip'
  if (!s.head.blkPerSec) return `${fmt.int(d)} blk ago`
  return '≈ ' + fmt.ago(d / s.head.blkPerSec)
}
const ver = (v: boolean) => (v ? '<span class="verified" title="on-chain record family">✓</span>' : '')

// ── shell ─────────────────────────────────────────────────────────────────
export function shell(): string {
  return `
  <a class="skip" href="#main">Skip to content</a>
  <header class="topbar">
    <a class="brand" href="/sigil-vm.html"><span class="cube">🔮</span><span>SIGIL <span class="vm">VM</span></span><small>MARKET · DEX</small></a>
    <div class="search">
      <span class="ico">${I.search}</span>
      <input id="q" type="search" placeholder="Search collections, tokens, rigs, wallets…" autocomplete="off" />
      <kbd>/</kbd>
      <div class="search-results" id="qres"></div>
    </div>
    <nav class="navlinks">
      <a href="#market" class="on" data-nav="market">Market</a>
      <a href="#drops" data-nav="drops">Drops</a>
      <a href="#trending" data-nav="trending">Trending</a>
      <a href="#dex" data-nav="dex">DEX</a>
      <a href="/sigil-explorer.html" target="_blank" rel="noopener">Explorer</a>
    </nav>
    <div class="actions">
      <div class="chain-wrap"><button class="chain-chip" id="chainChip" title="sigil-g2 · sigil-api"><i class="d"></i><span class="t">sigil-g2</span><span id="chainHeight">—</span><span class="car">▾</span></button>
        <div class="chain-menu" id="chainMenu">
          <div class="cm on"><i class="d on"></i><div><b>SIGIL g2</b><small>mainnet · sigil-api :18181</small></div><span class="chip live">live</span></div>
          <a class="cm" href="https://polygonscan.com/token/0x3FCED760" target="_blank" rel="noopener"><i class="d" style="background:#8247e5;color:#8247e5"></i><div><b>Polygon</b><small>wSIGIL3 · Uniswap pool</small></div><span class="chip derived">external</span></a>
          <div class="cm dis"><i class="d" style="background:#555"></i><div><b>Soneium</b><small>wSIGIL leg · not deployed</small></div><span class="chip bad">blocked</span></div>
        </div></div>
      <button class="ibtn" id="themeBtn" title="Theme" aria-label="Toggle light / dark">${I.sun}</button>
      <button class="ibtn" id="bellBtn" title="Activity" aria-label="Activity">${I.bell}<i class="badge" id="bellBadge" hidden></i></button>
      <button class="ibtn" id="cartBtn" title="Cart (coming with listings)" aria-label="Cart">${I.cart}</button>
      <button class="btn primary" id="walletBtn">${I.wallet}<span id="walletLbl">Connect wallet</span></button>
      <button class="ibtn" id="panelBtn" title="Toggle wallet panel" aria-label="Toggle wallet panel">${I.panel}</button>
    </div>
  </header>
  <aside class="rail">
    <a href="#market" class="on" data-nav="market">${I.home}<span class="tip">Home</span></a>
    <a href="#featured" data-nav="featured">${I.compass}<span class="tip">Discover</span></a>
    <a href="#trending" data-nav="trending">${I.grid}<span class="tip">Collections</span></a>
    <a href="#dex" data-nav="dex">${I.coins}<span class="tip">Tokens &amp; DEX</span></a>
    <a href="#drops" data-nav="drops">${I.rocket}<span class="tip">Drops</span></a>
    <a href="#swap" data-nav="swap">${I.swap}<span class="tip">Swap</span></a>
    <span class="sp"></span>
    <a href="/api.html" target="_blank" rel="noopener">${I.code}<span class="tip">Build on SIGIL VM</span></a>
    <a href="/sigil-explorer.html" target="_blank" rel="noopener">${I.eye}<span class="tip">Explorer</span></a>
  </aside>
  <main class="main" id="main">
    <div class="ticker" id="ticker" aria-label="Live chain activity"><div class="tk-track" id="tickerTrack"></div></div>
    <section id="market" class="section" style="margin-top:0">
      <div class="hero skel-hero" id="hero"><div class="bg"></div><div class="veil"></div><div class="body"><div class="eyebrow"><span class="chip gold"><i class="d"></i>reading the chain…</span></div><div class="sk sk-h1"></div><div class="sk sk-p"></div><div class="stats"><div class="stat sk-stat"></div><div class="stat sk-stat"></div><div class="stat sk-stat"></div><div class="stat sk-stat"></div></div><div class="cta"><div class="sk sk-btn"></div><div class="sk sk-btn"></div></div></div></div>
      <div class="strip-wrap"><div class="strip" id="strip">${skStrip()}</div></div>
      <div id="foryou"><div class="foryou"><div class="fy-col"><div class="sk sk-k"></div><div class="sk sk-p"></div><div class="sk sk-p"></div><div class="sk sk-p short"></div></div><div class="fy-col"><div class="sk sk-k"></div><div class="sk sk-p"></div><div class="sk sk-p"></div><div class="sk sk-p short"></div></div></div></div>
    </section>
    <div class="cats" id="cats">
      <button class="on" data-cat="all">All</button><button data-cat="chain">Chain</button><button data-cat="mining">Mining</button><button data-cat="court">Court</button><button data-cat="shielded">Shielded</button><button data-cat="tokens">Tokens</button><button data-cat="bridge">Bridge</button><button data-cat="science">Science</button><button data-cat="physical">Physical</button><button data-cat="story">Story</button>
    </div>
    <section id="featured" class="section">
      <div class="sec-head"><div><h2>Featured Collections</h2><div class="sub">Record families the chain can prove — ranked by items on chain.</div></div><div class="right"><div class="arrows" data-scroll="featuredRow"><button aria-label="Scroll left">${I.left}</button><button aria-label="Scroll right">${I.right}</button></div><a class="viewall" href="#trending">View all</a></div></div>
      <div class="row" id="featuredRow">${skeleton(5)}</div>
    </section>
    <section id="drops" class="section">
      <div class="sec-head"><div><h2>Featured Drops</h2><div class="sub">Activations gated by block height. Countdowns are measured from the live height and block rate.</div></div><div class="right"><div class="arrows" data-scroll="dropsRow"><button aria-label="Scroll left">${I.left}</button><button aria-label="Scroll right">${I.right}</button></div></div></div>
      <div class="row" id="dropsRow">${skeleton(4, 'drop wide')}</div>
    </section>
    <section id="trending" class="section">
      <div class="sec-head">
        <div><h2>Trending Collections</h2><div class="sub">Two columns, OpenSea-style. Volume and floor are the collection's own live measures.</div></div>
        <div class="right">
          <div class="tabs" id="trendMode"><button class="on" data-mode="trending">Trending</button><button data-mode="top">Top</button></div>
          <div class="tabs pill" id="trendWin"><button data-win="1h">1h</button><button data-win="6h">6h</button><button class="on" data-win="24h">24h</button><button data-win="7d">7d</button></div>
          <a class="viewall" href="/sigil-explorer.html" target="_blank" rel="noopener">View all</a>
        </div>
      </div>
      <div class="trending" id="trendingTable">${skTrending()}</div>
    </section>
    <section id="movers" class="section">
      <div class="sec-head"><div><h2>Top Movers Today</h2><div class="sub">Rigs by hashrate. Change is measured against the first sample this page took today (kept in your browser).</div></div><div class="right"><div class="arrows" data-scroll="moversRow"><button aria-label="Scroll left">${I.left}</button><button aria-label="Scroll right">${I.right}</button></div></div></div>
      <div class="row" id="moversRow">${skeleton(6, 'mover')}</div>
    </section>
    <section id="sales" class="section">
      <div class="sec-head"><div><h2>Highest Weekly Sales</h2><div class="sub">Settlements the chain can prove this week — each card carries its tx hash or docket leaf. No marketplace sales exist on SIGIL yet; nothing here is a price.</div></div><div class="right"><div class="arrows" data-scroll="salesRow"><button aria-label="Scroll left">${I.left}</button><button aria-label="Scroll right">${I.right}</button></div></div></div>
      <div class="row" id="salesRow">${skeleton(6, 'sale')}</div>
    </section>
    <section id="dex" class="section">
      <div class="sec-head"><div><h2>Swap &amp; Tokens</h2><div class="sub">The Quillon Graph DEX modules, on SIGIL: the swap module and the token list side by side. Signing happens in your wallet.</div></div><div class="right"><a class="viewall" href="/sigil-dex.html" target="_blank" rel="noopener">Trading desk</a></div></div>
      <div class="dex"><div id="swap">${skSwap()}</div><div id="tokens">${skTokens()}</div></div>
    </section>
    <div class="legend" id="legend"></div>
    <footer class="foot">
      <div class="foot-top">
        <div class="foot-brand"><div class="brand"><span class="cube">🔮</span><span>SIGIL <span class="vm">VM</span></span></div>
          <p>The marketplace for everything the SIGIL chain can prove. Every figure on this page is read from the node and labelled live, derived or pretend — nothing is a price until an oracle says so.</p>
          <div class="foot-cta"><a class="btn primary" id="walletApk" href="/downloads/sigil-wallet-native-latest.apk">Get the wallet</a><a class="btn ghost" href="/downloads/sigil-top-latest.json" target="_blank" rel="noopener">sigil-top releases</a></div></div>
        <div class="foot-cols">
          <div><h5>Market</h5><a href="#featured">Collections</a><a href="#drops">Drops</a><a href="#trending">Trending</a><a href="#swap">Swap</a><a href="/sigil-dex.html" target="_blank" rel="noopener">Trading desk</a></div>
          <div><h5>Chain</h5><a href="/sigil-explorer.html" target="_blank" rel="noopener">Explorer</a><a href="/api.html" target="_blank" rel="noopener">API</a><a href="/datacenter.html" target="_blank" rel="noopener">Datacenter</a><a href="/kristensen-board.html" target="_blank" rel="noopener">K board</a><a href="/kristensen-time.html" target="_blank" rel="noopener">Kristensen Time</a></div>
          <div><h5>Nation</h5><a href="/sigil-nation-whitepaper.html" target="_blank" rel="noopener">Whitepaper</a><a href="/sigil-wallet-tron-embedded.html#court" target="_blank" rel="noopener">Supreme Court</a><a href="/kristensen-earth.html" target="_blank" rel="noopener">Earth K⊕</a><a href="/bridge-slider.html" target="_blank" rel="noopener">Bridge</a></div>
          <div><h5>Build</h5><a href="/downloads/sigil-vm-src.tar.gz">Page source (.tar.gz)</a><a href="https://github.com/deme-plata/sigilgraph" target="_blank" rel="noopener">GitHub</a><a href="/sigil-top-openapi.json" target="_blank" rel="noopener">OpenAPI</a><a href="/flux-versions.html" target="_blank" rel="noopener">Flux versions</a></div>
        </div>
      </div>
      <div class="foot-bottom"><span>© 2026 SIGIL Graph · sigilgraph.org</span><span id="footTs" class="mono"></span><span class="muted">Built with the Flux Vite engine · shipped through <span class="mono">ship.sh</span></span><button class="foot-keys" id="footKeys">Keyboard <kbd>?</kbd></button></div>
    </footer>
  </main>
  <aside class="panel" id="panel">
    <div class="ph" id="panelHead"></div>
    <div class="ptabs" id="panelTabs"><button class="on" data-ptab="tokens">Tokens<span class="n" id="nTok"></span></button><button data-ptab="nfts">NFTs<span class="n" id="nNft"></span></button><button data-ptab="activity">Activity</button></div>
    <div class="pbody" id="panelBody"></div>
    <div class="pf"><button class="btn ghost" id="pSend" title="Opens the SIGIL wallet app — signing never happens on this page">Send ↗</button><button class="btn ghost" id="pReceive" title="Opens the SIGIL wallet app to show your payment code">Receive ↗</button><button class="btn primary" id="pSwap">Swap</button></div>
  </aside>
  <nav class="bottomnav" aria-label="Mobile navigation">
    <a href="#market" data-nav="market" class="on">${I.home}<span>Home</span></a>
    <a href="#featured" data-nav="featured">${I.compass}<span>Discover</span></a>
    <a href="#drops" data-nav="drops">${I.rocket}<span>Drops</span></a>
    <a href="#swap" data-nav="swap">${I.swap}<span>Swap</span></a>
    <a href="#" id="bnWallet">${I.wallet}<span>Wallet</span></a>
  </nav>
  <button class="totop" id="toTop" aria-label="Back to top" hidden>${I.up}</button>
  <div class="modal" id="modal"><div class="box" id="modalBox"></div></div>
  <div class="toasts" id="toasts"></div>`
}


function skStrip(): string { return Array.from({ length: 6 }, () => '<div class="cell"><div class="sk sk-k"></div><div class="sk sk-v"></div></div>').join('') }
function skTrending(): string {
  const rows = Array.from({ length: 5 }, (_, i) => `<tr><td class="rank">${i + 1}</td><td><div class="coll"><div class="sk sk-av"></div><div><div class="sk sk-k" style="width:140px"></div><div class="sk sk-k short"></div></div></div></td><td class="r"><div class="sk sk-k" style="width:70px;margin-left:auto"></div></td></tr>`).join('')
  return `<div class="tcol"><table><tbody>${rows}</tbody></table></div><div class="tcol"><table><tbody>${rows}</tbody></table></div>`
}
function skSwap(): string { return '<div class="qcard"><div class="inner"><div class="sk sk-h"></div><div class="sk sk-bar"></div><div class="sk sk-input"></div><div class="sk sk-bar"></div><div class="sk sk-input"></div><div class="sk sk-btn full"></div></div></div>' }
function skTokens(): string { return '<div class="qcard pinkish"><div class="inner"><div class="sk sk-h"></div><div class="sk sk-input"></div>' + Array.from({ length: 5 }, () => '<div class="sk sk-row"></div>').join('') + '</div></div>' }

export function skeleton(n: number, cls = ''): string {
  return Array.from({ length: n }, () => `<div class="card skel ${cls}"><div class="img"></div><div class="meta"><div class="name">&nbsp;</div><div class="blurb"></div></div></div>`).join('')
}

// ── hero ──────────────────────────────────────────────────────────────────
export function hero(s: Snapshot, idx: number): string {
  const list = s.featured.length ? s.featured : s.collections
  if (!list.length) return ''
  const c = list[idx % list.length]
  const h = s.head
  return `
    <div class="bg" style="background-image:url('${c.cover}')"></div><div class="veil"></div>
    <div class="dots">${list.map((c, i) => `<button class="${i === idx % list.length ? 'on' : ''}" data-hero="${i}" aria-label="Show ${esc(c.name)}"></button>`).join('')}</div>
    <div class="body">
      <div class="eyebrow">${prov(c.provenance)}<span class="chip gold">featured collection</span>${h.ok ? '<span class="chip live"><i class="d"></i>sigil-g2 · block ' + fmt.int(h.height) + '</span>' : '<span class="chip bad">node unreachable</span>'}</div>
      <h1>${esc(c.name)}${ver(c.verified)}</h1>
      ${c.by ? `<div class="by">${esc(c.by)}</div>` : ''}
      <p>${esc(c.blurb)}</p>
      <div class="stats">
        <div class="stat"><div class="k">Floor</div><div class="v">${esc(c.floor)}</div></div>
        <div class="stat"><div class="k">Items</div><div class="v">${c.items === null ? '—' : fmt.int(c.items)}</div></div>
        <div class="stat"><div class="k">Owners</div><div class="v">${c.owners === null ? '—' : fmt.int(c.owners)}</div></div>
        <div class="stat"><div class="k">Volume</div><div class="v">${esc(c.volume)}</div></div>
        ${(() => { const ch = s.changes[c.id]?.['24h'] ?? (c.id === 'rigs' ? c.volumeChange : null); if (ch !== null && ch !== undefined) return `<div class="stat"><div class="k">24h</div><div class="v">${delta(ch)}</div></div>`; if (c.id === 'blocks' && h.blkPerSec) return `<div class="stat"><div class="k">Rate</div><div class="v">${h.blkPerSec.toFixed(1)} blk/s</div></div>`; if (c.id === 'rigs' && h.hashChange !== null) return `<div class="stat"><div class="k">Window</div><div class="v">${delta(h.hashChange)}</div></div>`; return '' })()}
      </div>
      <div class="cta"><a class="btn primary lg" href="${c.link}" target="_blank" rel="noopener">View collection</a><a class="btn ghost lg" href="#swap">Swap SIGIL</a></div>
    </div>
    <div class="thumbs">${list.map((x, i) => `<button data-hero="${i}" class="${i === idx % list.length ? 'on' : ''}" title="${esc(x.name)}"><img src="${x.cover}" alt=""></button>`).join('')}</div>`
}

export function strip(s: Snapshot): string {
  const h = s.head
  const cell = (k: string, v: string, small = '', coll = '', extra = '') => `<button class="cell"${coll ? ` data-coll="${coll}"` : ''}><div class="k">${k}</div><div class="v"><span class="vt">${v}</span>${extra}</div>${small ? `<div class="s">${small}</div>` : ''}</button>`
  return [
    cell('Height', fmt.int(h.height), h.blkPerSec ? `${h.blkPerSec.toFixed(1)} blk/s` : 'measuring rate…', 'blocks'),
    cell('Finality', h.finalityGate, `h ${fmt.int(h.finalityHeight)} · ${h.committee} validators`, 'blocks'),
    cell('Supply', fmt.num(h.supplySigil) + ' SIGIL', `${(h.mintedPct).toFixed(2)}% of 21M`, 'rigs'),
    cell('Hashrate', fmt.hps(h.netHps), `${h.liveMiners} miners${h.hashChange !== null ? ' · ' + fmt.pct(h.hashChange) : ''}`, 'rigs', s.hashHist.length > 2 ? sparkline(s.hashHist, 90, 22, h.hashChange !== null && h.hashChange < 0 ? 'down' : 'up') : ''),
    cell('Shielded pool', fmt.int(h.notes) + ' notes', `${fmt.num(h.valueLocked)} SIGIL locked · ${fmt.int(h.nullifiers)} spent`, 'notes'),
    cell('Treasury', fmt.num(h.treasurySigil) + ' SIGIL', 'nation welfare', 'treasury'),
  ].join('')
}

export function forYou(s: Snapshot): string {
  const h = s.head
  const lagS = h.blkPerSec ? h.lagBlocks / h.blkPerSec : null
  const rockyDrop = s.drops.find((d) => d.id === 'rocky')
  const tag = (t: string, cls = '') => `<span class="chip ${cls}">${t}</span>`
  const next: string[] = []
  if (rockyDrop) {
    const r = s.rocky
    const txt = r?.live
      ? (r.bootstrapped ? `activated at block ${esc(rockyDrop.when.replace(/^live at block /, ''))} and bootstrapped — cover policies against K⊕ excursions are buyable.` : `${esc(rockyDrop.when)} — the gate passed; the owner's bootstrap (delegate + bootstrap) has not run yet, so supply is 0 and no policy can be written.`)
      : `${esc(rockyDrop.when)}. Cover policies against K⊕ excursions become buyable.`
    next.push(`<li>${tag(rockyDrop.provenance === 'live' ? 'MEASURED' : 'DESIGN', rockyDrop.provenance === 'live' ? 'live' : 'gold')} <b>ROCKY + GaugePush</b> — ${txt}</li>`)
    if (s.gaugeFresh !== null) next.push(`<li>${tag('MEASURED', 'live')} <b>K⊕ gauge feeds</b> — ${s.gaugeFresh} of ${s.gaugeFeeds} feeds fresh on chain${s.gaugeFresh === 0 ? '; the feeder wallet has not pushed yet' : ''}.</li>`)
  }
  next.push(`<li>${tag('DESIGN', 'gold')} <b>USDS oracle feed</b> — ${s.usds?.price_fresh ? 'fed and fresh' : 'not fed yet'}; until a feeder pushes a price, USDS mints cannot be quoted in dollars.</li>`)
  next.push(`<li>${tag('MEASURED', 'live')} <b>First DEX pool</b> — <span class="mono">/v1/pools</span> holds ${s.pools.length} pool${s.pools.length === 1 ? '' : 's'}; the swap module below quotes the instant liquidity exists.</li>`)
  next.push(`<li>${tag('DESIGN', 'gold')} <b>SIGIL Coins</b> — first NFC tag written and claimed back on one phone, then a 100-coin batch anchored.</li>`)
  next.push(`<li>${tag('EXTERNAL', 'derived')} <b>wSIGIL3 mints</b> — Polygon token + pool exist; bridge mints wait on the relayer (${s.collections.find((c) => c.id === 'bridge')?.items ?? 0} locks so far).</li>`)
  return `<div class="foryou">
    <div class="fy-col">
      <div class="fy-h">For you <span class="muted">— money words, not instruments</span></div>
      <p><b>Is my payment final?</b> ${h.ok ? `The finality certificate sits ${fmt.int(h.lagBlocks)} block${h.lagBlocks === 1 ? '' : 's'} behind the tip${lagS !== null ? ` ≈ <b>${lagS < 1 ? '<1' : lagS.toFixed(0)} s</b>` : ''} (gate <span class="mono">${esc(h.finalityGate)}</span>, ${h.committee} validators).` : 'Node unreachable — no certificate read.'} ${tag('MEASURED', 'live')}</p>
      <p><b>What protects it?</b> ${fmt.int(h.liveMiners)} live rigs at ${fmt.hps(h.netHps)} (BLAKE4 + VDF), ${fmt.int(h.notes)} sealed notes holding ${fmt.num(h.valueLocked)} SIGIL, and a hybrid post-quantum block check on the producer. ${tag('MEASURED', 'live')}</p>
      <p><b>What is it worth?</b> ${fmt.num(h.supplySigil)} of 21M SIGIL minted (${h.mintedPct.toFixed(2)}%). There is <b>no on-chain price</b>: the USDS oracle is not fed and no SIGIL pool exists on g2. The only market is the wSIGIL3 pool on Polygon, off this chain. ${tag('MEASURED', 'live')} ${tag('EXTERNAL', 'derived')}</p>
      <p><b>Can I pay with it?</b> Yes — privately only. Transparent sends are retired, so every payment is a sealed note: ${h.capacity ? `<b>${fmt.int(Math.max(0, h.capacity - h.notes))}</b> of ${fmt.int(h.capacity)} leaves are free this epoch, ` : ''}${fmt.int(h.registered)} wallets have published receiving keys, and ${fmt.int(h.nullifiers)} notes have ever been spent. The proof and the signature happen in your wallet app, never on this page. ${tag('MEASURED', 'live')}</p>
    </div>
    <div class="fy-col">
      <div class="fy-h">What's next <span class="muted">— ETAs from live height and block rate</span></div>
      <ul class="fy-next">${next.join('')}</ul>
    </div>
  </div>`
}

// ── cards ─────────────────────────────────────────────────────────────────
export function collectionCard(c: Collection): string {
  return `<a class="card" href="${c.link}" target="_blank" rel="noopener" data-coll="${c.id}">
    <div class="img" style="background-image:url('${c.cover}')"><span class="prov">${prov(c.provenance)}</span><span class="cta"><span class="cta-blurb">${esc(c.blurb)}</span><span class="cta-btn">View collection ↗</span></span></div>
    <div class="meta">
      <div class="name">${esc(c.name)}${ver(c.verified)}</div>
      ${c.by ? `<div class="byline">${esc(c.by)}</div>` : ''}
      <div class="kv"><div><div class="k">Floor</div><div class="v">${esc(c.floor)}</div></div><div style="text-align:right"><div class="k">Items</div><div class="v">${c.items === null ? '—' : fmt.int(c.items)}</div></div></div>
    </div></a>`
}

export function dropCard(d: Drop): string {
  const st: Record<Drop['status'], string> = { live: 'chip live', minting: 'chip derived', upcoming: 'chip gold', blocked: 'chip bad' }
  const lbl: Record<Drop['status'], string> = { live: 'live', minting: 'minting', upcoming: 'upcoming', blocked: 'blocked' }
  return `<a class="card drop wide" href="${d.link}" target="_blank" rel="noopener">
    <div class="img" style="background-image:url('${d.cover}')"><span class="prov">${prov(d.provenance)}</span>${d.status === 'live' && d.provenance === 'live' ? '' : `<span class="status ${st[d.status]}"><i class="d"></i>${lbl[d.status]}</span>`}<span class="cta"><span class="cta-blurb">${esc(d.detail)}</span><span class="cta-btn">Open drop ↗</span></span></div>
    <div class="meta">
      <div class="name">${esc(d.name)}</div>
      <div class="when"${d.countdown ? ` data-cd-target="${d.countdown.target}" data-cd-height="${d.countdown.height}" data-cd-rate="${d.countdown.blkPerSec ?? ''}" data-cd-at="${d.countdown.at}"` : ''}>${esc(d.when)}</div>
      ${d.progress !== null ? `<div class="bar"><i style="width:${(d.progress * 100).toFixed(2)}%"></i></div>` : ''}
    </div></a>`
}

export function moverCard(m: Mover): string {
  return `<div class="card mover${m.idle ? ' idle' : ''}" title="${esc(m.id)}" data-coll="rigs" role="button" tabindex="0">
    <div class="img" style="background-image:url('${m.cover}')"><span class="prov">${prov(m.provenance)}</span>${m.idle ? '<span class="status chip"><i class="d"></i>idle</span>' : m.sub.startsWith('GPU') ? '<span class="status chip derived"><i class="d"></i>GPU</span>' : ''}<span class="cta"><span class="cta-blurb">${m.idle ? 'idle · last seen ' : ''}${esc(m.sub)}<br><span class="mono">wallet ${fmt.short(m.id, 8)}</span></span><span class="cta-btn">Open rigs ↗</span></span></div>
    <div class="meta"><div class="name">${esc(m.name)}</div><div class="blurb" style="height:auto">${esc(m.sub)}</div>
      ${m.series && m.series.length > 2 ? `<div class="sparkwrap">${sparkline(m.series, 180, 34, m.series[m.series.length - 1] < m.series[0] ? 'down' : 'up')}</div>` : m.idle ? '' : `<div class="sparkwrap ph" title="The sparkline is sampled in this browser, one point per poll">sampling · ${Math.max(0, 3 - (m.series?.length ?? 0))} more poll${3 - (m.series?.length ?? 0) === 1 ? '' : 's'}</div>`}
      <div class="kv"><div><div class="k">Hashrate</div><div class="v">${esc(m.value)}</div></div><div style="text-align:right">${m.change !== null ? `<div class="k">Today</div><div class="delta">${delta(m.change)}</div>` : m.share !== undefined ? `<div class="k">Share</div><div class="delta share">${m.share.toFixed(1)}%</div>` : `<div class="k">Today</div><div class="delta">—</div>`}</div></div>
    </div></div>`
}

export function saleCard(x: Sale): string {
  return `<div class="card sale" data-proof="${esc(x.proof)}" data-coll="${x.collection.startsWith('Elefant') ? 'honours' : x.collection.startsWith('Justices') ? 'bench' : x.collection.startsWith('Kristensen') ? 'earth' : x.collection.startsWith('DagKnight') ? 'blocks' : x.collection.startsWith("Miners") ? 'rigs' : x.collection === 'Drops' ? 'treasury' : ''}" role="button" tabindex="0">
    <div class="img" style="background-image:url('${x.cover}')"><span class="prov">${prov(x.provenance)}</span><span class="cta"><span class="cta-blurb mono">${esc(x.proof)}</span><span class="cta-btn" data-copy="${esc(x.proof)}">Copy proof</span></span></div>
    <div class="meta"><div class="name">${esc(x.name)}</div><div class="byline">${esc(x.collection)} · ${esc(x.when)}</div>
      <div class="kv"><div><div class="k">Settled</div><div class="v">${esc(x.price)}</div></div><div style="text-align:right"><div class="k">Proof</div><div class="v mono" title="${esc(x.proof)}">${fmt.short(x.proof, 5)}</div></div></div></div></div>`
}

// ── trending table ────────────────────────────────────────────────────────
export function trending(s: Snapshot, mode: 'trending' | 'top', win: string): string {
  const list = [...s.collections]
  const chg = (c: Collection): number | null => c.id === 'rigs' && win === '24h' ? (c.volumeChange ?? s.changes[c.id]?.[win] ?? null) : (s.changes[c.id]?.[win] ?? null)
  // "trending" = biggest measured growth in the window (items), unmeasured rows after, then by activity; "top" = by items
  list.sort((a, b) => mode === 'top'
    ? (b.items ?? -1) - (a.items ?? -1)
    : ((chg(b) ?? -Infinity) - (chg(a) ?? -Infinity)) || (((b.sales ?? 0) * 3 + (b.provenance === 'live' ? 1 : 0)) - ((a.sales ?? 0) * 3 + (a.provenance === 'live' ? 1 : 0))))
  const half = Math.ceil(list.length / 2)
  const table = (rows: Collection[], off: number) => `<table><thead><tr><th>#</th><th>Collection</th><th class="r">Floor</th><th class="r" title="change in items over the window — sampled in your browser once a minute; shows — until enough samples exist">${win} chg</th><th class="r">Volume</th><th class="r">Items</th><th class="r">Owners</th></tr></thead><tbody>
    ${rows.map((c, i) => `<tr data-coll="${c.id}" data-link="${c.link}" tabindex="0"><td class="rank">${off + i + 1}</td><td><div class="coll"><img src="${c.cover}" alt=""><div><div class="n">${esc(c.name)}${ver(c.verified)}</div><div class="s">${prov(c.provenance)}</div></div></div></td>
      <td class="r num">${esc(c.floor)}</td><td class="r num" title="items now vs. items ${win} ago, sampled by this browser">${delta(chg(c))}</td><td class="r num">${esc(c.volume)}</td><td class="r num">${c.items === null ? '—' : fmt.int(c.items)}</td><td class="r num">${c.owners === null ? '—' : fmt.int(c.owners)}<span class="chev">${I.right}</span></td></tr>`).join('')}
  </tbody></table>`
  return `<div class="tcol">${table(list.slice(0, half), 0)}</div><div class="tcol">${table(list.slice(half), half)}</div>`
}

// ── DEX: swap module (Quillon-shaped) ─────────────────────────────────────
export interface SwapState { mode: 'market' | 'limit'; from: string; to: string; amount: string; limitPrice: string; limitDir: 'buy' | 'sell'; slippage: number }

export function swapModule(s: Snapshot, st: SwapState, balances: Record<string, number>, q: { out: number; impact: number } | null, wallet: string | null = null): string {
  const tk = (id: string) => s.tokens.find((t) => t.id === id) || s.tokens[0]
  const from = tk(st.from), to = tk(st.to)
  const bal = balances[from.id] ?? 0
  const amt = Number(st.amount) || 0
  const pctv = bal > 0 ? Math.min(100, (amt / bal) * 100) : 0
  const noPool = !s.pools.length
  const sel = (which: 'from' | 'to', t: Token) => `<button class="tsel" data-sel="${which}"><img src="${t.icon}" alt=""><span>${esc(t.symbol)}</span><span class="car">▼</span></button>`
  const marketForm = `
    <div class="field"><label><span>From</span><span class="bal">${wallet ? `Balance: ${bal.toFixed(4)}<button class="maxbtn" data-max="1">MAX</button>` : `<button class="maxbtn" id="swapConnect">Connect wallet to see balance</button>`}</span></label>
      <div class="amt"><input id="swapAmt" type="number" inputmode="decimal" placeholder="0.0" value="${esc(st.amount)}" min="0" step="any">${sel('from', from)}</div></div>
    <div class="slider"><div class="lab"><span>Quick Select Amount</span><b>${pctv.toFixed(0)}%</b></div>
      <div class="track"><div class="fill" style="width:${pctv}%"></div><input id="swapRange" type="range" min="0" max="100" step="1" value="${pctv.toFixed(0)}"></div>
      <div class="quick">${[25, 50, 75, 100].map((p) => `<button data-pct="${p}">${p}%</button>`).join('')}</div></div>
    <div class="flip"><button id="swapFlip" title="Flip">${I.updown}</button></div>
    <div class="field"><label><span>To (Estimated)</span><span class="bal">${wallet ? `${to.symbol} · ${balances[to.id] !== undefined ? balances[to.id].toFixed(4) : '—'}` : esc(to.symbol)}</span></label>
      <div class="amt"><input id="swapOut" type="text" readonly placeholder="0.0" value="${q ? q.out.toFixed(6) : ''}">${sel('to', to)}</div></div>
    ${noPool
      ? `<div class="info warnbox">No liquidity pool exists on sigil-g2 for ${esc(from.symbol)}/${esc(to.symbol)} yet — <span class="mono">/v1/pools</span> is empty, so there is no quote to show. The module is wired to the constant-product formula and lights up the moment a pool is added.</div>`
      : `<div class="info"><div><span class="k">Rate</span><span class="v">1 ${esc(from.symbol)} ≈ ${q && amt > 0 ? (q.out / amt).toFixed(6) : '—'} ${esc(to.symbol)}</span></div><div><span class="k">Price impact</span><span class="v ${q && q.impact > 5 ? 'down' : ''}">${q ? q.impact.toFixed(2) + '%' : '—'}</span></div><div><span class="k">Fee</span><span class="v">0.30%</span></div><div><span class="k">Slippage</span><span class="v">${st.slippage.toFixed(1)}%</span></div></div>`}
    ${!wallet ? `<button class="btn primary lg full swapbtn" id="swapConnect2">Connect wallet</button>` : `<button class="btn primary lg full swapbtn${balances[from.id] !== undefined && amt > bal ? ' insufficient' : ''}" id="swapGo" ${noPool || amt <= 0 || (balances[from.id] !== undefined && amt > bal) ? 'disabled' : ''}>${amt <= 0 ? 'Enter an amount' : balances[from.id] !== undefined && amt > bal ? `Insufficient ${esc(from.symbol)} balance` : noPool ? 'No pool yet' : `Swap ${esc(from.symbol)} → ${esc(to.symbol)}`}</button>`}
    <div class="muted" style="font-size:11.5px;margin-top:8px;text-align:center">Signing and the STARK proof happen in your wallet — this page never sees a seed.</div>`
  const limitForm = `<div class="limit">
    <div class="pair"><div style="flex:1"><div class="sm">Sell</div>${sel('from', from)}<div class="sm" style="text-align:right;margin-top:4px">${wallet ? `Balance: ${bal.toFixed(4)}` : 'Balance: — · no wallet'}</div></div><div class="arrow">→</div><div style="flex:1"><div class="sm">Buy</div>${sel('to', to)}<div class="sm" style="text-align:right;margin-top:4px">${to.symbol} · ${wallet && balances[to.id] !== undefined ? balances[to.id].toFixed(4) : '—'}</div></div></div>
    <div class="field"><label>Amount (${esc(from.symbol)})</label><div class="amt"><input id="swapAmt" type="number" placeholder="0.0" value="${esc(st.amount)}"><button class="tsel" data-max="1" style="color:#fbbf24">MAX</button></div></div>
    <div class="field"><label>Trigger condition</label><div class="dir" style="margin-top:6px"><button class="${st.limitDir === 'buy' ? 'on buy' : ''}" data-dir="buy">Buy when price ↓ below</button><button class="${st.limitDir === 'sell' ? 'on sell' : ''}" data-dir="sell">Sell when price ↑ above</button></div></div>
    <div class="field"><label>Limit price (${esc(to.symbol)} per ${esc(from.symbol)})</label><div class="amt"><input id="limitPx" type="number" placeholder="0.0" value="${esc(st.limitPrice)}"></div></div>
    <div class="info warnbox">Limit orders need an order book on SIGIL VM. The desk keeps them client-side until a pool and a keeper exist — nothing is placed on chain from here yet.</div>
    <button class="btn lg full swapbtn" style="background:linear-gradient(90deg,#fbbf24,#f97316);color:#000" disabled>Place limit order (soon)</button></div>`
  return `<div class="qcard"><div class="glow"></div><div class="inner">
    <div class="head"><h3>Swap Tokens</h3><div class="icons"><button id="swapSettings" title="Slippage">${I.settings}</button><button id="swapCollapse" title="Collapse">${I.left}</button></div></div>
    <div class="seg"><button class="${st.mode === 'market' ? 'on' : ''}" data-mode="market">Market</button><button class="${st.mode === 'limit' ? 'on limit' : ''}" data-mode="limit">Limit</button></div>
    ${st.mode === 'market' ? marketForm : limitForm}
  </div></div>`
}

export function routeCard(s: Snapshot, st: SwapState): string {
  const tk = (id: string) => s.tokens.find((t) => t.id === id) || s.tokens[0]
  const from = tk(st.from), to = tk(st.to)
  const h = s.head
  const lagS = h.blkPerSec ? h.lagBlocks / h.blkPerSec : null
  const free = Math.max(0, h.capacity - h.notes)
  return `<div class="qcard route"><div class="glow"></div><div class="inner">
    <div class="head"><h3>Route &amp; pool</h3><span class="chip ${s.pools.length ? 'live' : 'gold'}"><i class="d"></i>${s.pools.length} pool${s.pools.length === 1 ? '' : 's'} on g2</span></div>
    <div class="route-pair"><img src="${from.icon}" alt=""><span>${esc(from.symbol)}</span><span class="arr">→</span><img src="${to.icon}" alt=""><span>${esc(to.symbol)}</span></div>
    <div class="info">
      <div><span class="k">Pool</span><span class="v">${s.pools.length ? 'constant product' : 'none yet'}</span></div>
      <div><span class="k">Formula</span><span class="v">Δy = y·Δx(1−f) / (x + Δx(1−f))</span></div>
      <div><span class="k">Fee f</span><span class="v">0.30%</span></div>
      <div><span class="k">Slippage</span><span class="v">${st.slippage.toFixed(1)}%</span></div>
      <div><span class="k">Settles in</span><span class="v">${lagS !== null ? (lagS < 1 ? '<1 s' : lagS.toFixed(0) + ' s') : fmt.int(h.lagBlocks) + ' blocks'} · ${esc(h.finalityGate)}</span></div>
      <div><span class="k">Output lands as</span><span class="v">a shielded note (${fmt.int(free)} free)</span></div>
    </div>
    <div class="route-cta"><a class="btn ghost" href="/sigil-wallet-tron-embedded.html" target="_blank" rel="noopener">Add liquidity</a><a class="btn ghost" href="/sigil-dex.html" target="_blank" rel="noopener">Trading desk ↗</a></div>
    <div class="muted" style="font-size:11.5px;margin-top:10px">Everything above is read from the node this poll. A pool is a ${'`'}PoolState${'`'} in the DEX state root; until one is committed there is nothing to route through.</div>
  </div></div>`
}

// ── DEX: token table (Quillon "Available Tokens") ─────────────────────────
export interface TokenTableState { q: string; filter: 'all' | 'gainers' | 'losers'; sort: keyof Token | 'supply'; dir: 'asc' | 'desc'; open: string | null }
export function tokenTable(s: Snapshot, st: TokenTableState): string {
  let list = s.tokens.filter((t) => !st.q || (t.symbol + ' ' + t.name + ' ' + t.id).toLowerCase().includes(st.q.toLowerCase()))
  if (st.filter === 'gainers') list = list.filter((t) => (t.change24h ?? 0) > 0)
  if (st.filter === 'losers') list = list.filter((t) => (t.change24h ?? 0) < 0)
  list.sort((a, b) => {
    const av = (a[st.sort] as number | null) ?? -Infinity, bv = (b[st.sort] as number | null) ?? -Infinity
    return st.dir === 'asc' ? (av as number) - (bv as number) : (bv as number) - (av as number)
  })
  const th = (k: string, label: string, title = '') => `<th data-sort="${k}" title="${title}">${label}${st.sort === k ? (st.dir === 'asc' ? ' ↑' : ' ↓') : ''}</th>`
  const usd = (n: number | null) => (n === null ? '<span class="muted">—</span>' : '$' + fmt.num(n, 4))
  const tagCls = (t: string) => t.toLowerCase().replace(/\s/g, '')
  return `<div class="qcard pinkish"><div class="glow"></div><div class="inner">
    <h3 style="margin-bottom:16px">Available Tokens</h3>
    <div class="tokens-tools"><div class="s">${I.search}<input id="tokQ" placeholder="Search by name, symbol, or token id…" value="${esc(st.q)}"></div>
      <div class="f">${(['all', 'gainers', 'losers'] as const).map((f) => `<button class="${st.filter === f ? 'on' : ''}" data-filter="${f}">${f[0].toUpperCase() + f.slice(1)}</button>`).join('')}</div></div>
    <div class="ttable"><table><thead><tr><th>Token</th>${th('price', 'Price', 'USD price · none of these has an on-chain oracle reading yet')}${th('change1h', '1h')}${th('change24h', '24h')}${th('change7d', '7d')}${th('volume24h', 'Vol')}${th('supply', 'Supply')}${th('liquidity', 'Liq')}${th('holders', 'Holders')}${th('ageBlocks', 'Age')}<th>Actions</th></tr></thead><tbody>
      ${list.map((t, i) => `<tr data-tok="${t.id}" style="animation-delay:${i * 50}ms"><td><div class="tk"><img src="${t.icon}" alt=""><div><div class="sym"><span class="status-dot ${t.status}" title="${esc(t.statusNote)}"></span>${esc(t.symbol)}${t.tags.map((g) => `<span class="tag ${tagCls(g)}">${esc(g)}</span>`).join('')}</div><div class="nm">${esc(t.name)} · ${esc(t.statusNote)}</div></div></div></td>
        <td>${usd(t.price)}</td><td>${delta(t.change1h)}</td><td>${delta(t.change24h)}</td><td>${delta(t.change7d)}</td><td>${t.volume24h === null ? '<span class="muted">—</span>' : fmt.num(t.volume24h)}</td>
        <td>${t.supply === null ? '<span class="muted">—</span>' : fmt.num(t.supply) + (t.maxSupply ? ` <span class="muted">/ ${fmt.num(t.maxSupply, 0)}</span>` : '')}</td>
        <td>${t.liquidity === null ? '<span class="muted">—</span>' : fmt.num(t.liquidity)}</td><td>${t.holders === null ? '<span class="muted">—</span>' : fmt.int(t.holders)}</td><td>${t.ageBlocks === null ? '<span class="muted">—</span>' : fmt.num(t.ageBlocks, 1) + ' blk'}</td>
        <td><div class="act"><button class="sw" data-swap="${t.id}">Swap</button><button data-info="${t.id}">${st.open === t.id ? 'Close' : 'Info'}</button></div></td></tr>${st.open === t.id ? tokenDetailRow(t, s) : ''}`).join('')}
    ${list.length ? '' : `<tr class="empty"><td colspan="11"><div class="tempty">${st.q ? `Nothing matches “${esc(st.q)}”.` : st.filter === 'gainers' ? 'No gainers — there is no on-chain price yet, so nothing has moved. The moment an oracle or a pool publishes one, this fills in.' : 'No losers — there is no on-chain price yet, so nothing has moved.'}</div></td></tr>`}
    </tbody></table></div>
    <div class="muted" style="font-size:11.5px;margin-top:10px">Price, 1h/24h/7d and volume read “—” because sigil-g2 has no price oracle feeding these tokens yet (USDS oracle: not fed; ROCKY: gated). Supply, holders, age and liquidity are live from the node.</div>
  </div></div>`
}

function tokenDetailRow(t: Token, s: Snapshot): string {
  const cell = (k: string, v: string) => `<div class="td-cell"><div class="k">${k}</div><div class="v">${v}</div></div>`
  const rows = [
    cell('Token id', `<span class="mono" title="${esc(t.id)}">${fmt.short(t.id, 10)}</span>`),
    cell('Decimals', String(t.decimals)),
    cell('Status', `<span class="status-dot ${t.status}"></span>${esc(t.statusNote)}`),
    cell('Supply', t.supply === null ? '—' : fmt.num(t.supply, 4) + (t.maxSupply ? ` / ${fmt.num(t.maxSupply, 0)}` : '')),
    cell('Holders', t.holders === null ? '—' : fmt.int(t.holders) + ' registered'),
    cell('Liquidity', t.liquidity === null ? 'no pool' : fmt.num(t.liquidity)),
    cell('Provenance', prov(t.provenance)),
  ]
  const spark = t.symbol === 'SIGIL' && s.hashHist.length > 2 ? `<div class="td-spark"><div class="k">Network hashrate (mining issuance) · ${fmt.pct(s.head.hashChange)}</div>${sparkline(s.hashHist, 800, 56, (s.head.hashChange ?? 0) < 0 ? 'down' : 'up')}</div>` : ''
  const links = t.symbol === 'wSIGIL3'
    ? `<a class="btn ghost sm" href="https://polygonscan.com/token/0x3FCED760" target="_blank" rel="noopener">Polygonscan ↗</a><a class="btn ghost sm" href="/bridge-slider.html" target="_blank" rel="noopener">Bridge ↗</a>`
    : t.symbol === 'ROCKY' ? `<a class="btn ghost sm" href="/kristensen-board.html" target="_blank" rel="noopener">K board ↗</a><a class="btn ghost sm" href="/api.html" target="_blank" rel="noopener">/v1/rocky ↗</a>`
    : t.symbol === 'USDS' ? `<a class="btn ghost sm" href="/api.html" target="_blank" rel="noopener">/v1/usds/status ↗</a>`
    : `<a class="btn ghost sm" href="/sigil-explorer.html" target="_blank" rel="noopener">Explorer ↗</a><a class="btn ghost sm" href="/api.html" target="_blank" rel="noopener">API ↗</a>`
  return `<tr class="td-row"><td colspan="11"><div class="td-wrap"><div class="td-grid">${rows.join('')}</div>${spark}<div class="td-links">${links}</div></div></td></tr>`
}

// ── wallet panel ──────────────────────────────────────────────────────────
export function panelHead(addr: string | null, s: Snapshot, balances: Record<string, number>): string {
  if (!addr) return `<div class="who"><img src="${avatarSvg('nobody', '?')}" alt=""><div><div class="a">No wallet connected</div><div class="b">Open the gate to load your SIGIL wallet</div></div><button class="ibtn x" id="panelClose" title="Close panel" aria-label="Close panel">${I.x}</button></div><div class="total"><div class="k">Portfolio</div><div class="v">—</div></div>`
  const native = balances['0'.repeat(64)] ?? 0
  const rig = s.miners?.miners.find((m) => m.wallet.toLowerCase() === addr.toLowerCase())
  const glyphs = balances['0'.repeat(64)] !== undefined ? Math.round(native * 1e10) : null
  return `<div class="who"><img src="${avatarSvg(addr, addr.slice(0, 1).toUpperCase())}" alt=""><div><div class="a" title="${esc(addr)}">${fmt.short(addr, 6)} <button class="copy" data-copy="${esc(addr)}" title="Copy wallet id">${I.copy}</button></div><div class="b">sigil-g2 · ${s.head.ok ? 'live' : 'offline'}${rig ? ` · mining as ${esc(rig.rig)}` : ''} · <button class="forget" id="pDisconnect" title="Forget this wallet on this page (nothing on chain changes)">forget</button></div></div><button class="ibtn x" id="panelClose" title="Close panel" aria-label="Close panel">${I.x}</button></div>
    <div class="total"><div class="k">Portfolio</div><div class="v">${native.toLocaleString('en-US', { maximumFractionDigits: 4 })}<small>SIGIL</small></div><div class="muted" style="font-size:11.5px">${glyphs !== null ? fmt.int(glyphs) + ' glyphs · ' : ''}USD value unavailable — no oracle price on chain yet</div></div>`
}

export function panelTokens(addr: string | null, s: Snapshot, balances: Record<string, number>): string {
  if (!addr) return `<div class="pempty"><div class="big">${I.wallet}</div>Connect a wallet to see your tokens.<br><br><a class="btn primary" href="/enter-sigil.html" target="_blank" rel="noopener">Open the gate</a></div>`
  return s.tokens.map((t) => `<button class="prow" data-ptok="${t.id}"><img src="${t.icon}" alt=""><div class="l"><div class="s">${esc(t.symbol)}</div><div class="n">${esc(t.name)}</div></div><div class="r">${balances[t.id] === undefined ? '<span class="muted">—</span>' : fmt.num(balances[t.id], 4)}<div class="u">${t.status === 'live' ? 'on chain' : t.status}</div></div></button>`).join('')
}

export function panelNfts(addr: string | null, s: Snapshot): string {
  if (!addr) return `<div class="pempty"><div class="big">${I.grid}</div>Your on-chain records — honours, seats, rigs, attestations — appear here once a wallet is connected.</div>`
  const mine: { name: string; coll: string; cover: string; id: string }[] = []
  const a = addr.toLowerCase()
  for (const e of s.docket?.entries ?? []) {
    const ev = e.event as { recipient?: number[]; justice?: number[]; order?: string }
    const rec = (ev.recipient || ev.justice || []).map((b) => b.toString(16).padStart(2, '0')).join('')
    if (rec === a) mine.push({ id: e.kind === 'HonourConferred' ? 'honours' : 'bench', name: e.kind === 'HonourConferred' ? (ev.order || 'Honour') : 'Justice seat', coll: e.kind === 'HonourConferred' ? 'Elefantordenen' : 'Justices of the Bench', cover: s.collections.find((c) => c.id === (e.kind === 'HonourConferred' ? 'honours' : 'bench'))!.cover })
  }
  for (const m of s.miners?.miners ?? []) if (m.wallet.toLowerCase() === a) mine.push({ id: 'rigs', name: m.rig || 'rig', coll: "Miners' Rigs", cover: s.collections.find((c) => c.id === 'rigs')!.cover })
  const ew = s.earth?.attest_last?.anchor?.wallet || ''
  if (ew.includes(a)) mine.push({ id: 'earth', name: 'K⊕ attester', coll: 'Kristensen Earth', cover: s.collections.find((c) => c.id === 'earth')!.cover })
  if (!mine.length) return `<div class="pempty"><div class="big">${I.eye}</div>No on-chain records for ${fmt.short(addr, 6)} yet.<br><span class="muted">Mine a block, earn an honour, or anchor an attestation.</span></div>`
  return `<div class="pgrid">${mine.map((n) => `<button class="pnft" data-coll="${n.id}"><div class="img" style="background-image:url('${n.cover}')"></div><div class="m"><div class="n">${esc(n.name)}</div><div class="c">${esc(n.coll)}</div></div></button>`).join('')}</div>`
}

export function panelActivity(s: Snapshot): string {
  const rows: string[] = []
  const row = (ic: string, cls: string, main: string, sub: string, t: string, coll = '') => rows.push(`<button class="pact"${coll ? ` data-coll="${coll}"` : ''}><span class="ic ${cls}">${ic}</span><span class="w">${main}<small>${sub}</small></span><span class="t">${t}</span></button>`)
  for (const x of (s.recent?.blocks.slice(0, 8) ?? [])) row(I.grid, x.is_blue ? 'blue' : 'red', `Block ${fmt.int(x.height)}`, `${x.is_blue ? 'blue' : 'red'} · blue score ${fmt.int(x.blue_score)} · ${fmt.short(hex(x.producer), 4)}`, blockAgo(s, x.height), 'blocks')
  for (const m of (s.miners?.miners ?? []).slice(0, 3)) row(I.coins, '', esc(m.rig || fmt.short(m.wallet, 6)), `${fmt.hps(m.hash_rate)} · ${m.shielded ? 'shielded' : 'transparent'}`, fmt.ago(m.last_seen_secs_ago), 'rigs')
  for (const e of (s.docket?.entries ?? []).slice(-3).reverse()) row(I.book, 'gold', esc(e.kind.replace(/([A-Z])/g, ' $1').trim()), `docket #${e.seq} · ${fmt.short(e.leaf, 5)}`, e.height ? `blk ${fmt.int(e.height)}` : 'genesis', e.kind === 'HonourConferred' ? 'honours' : 'bench')
  const a = s.earth?.attest_last?.anchor
  if (a?.tx_hash) row(I.eye, 'gold', 'K⊕ attestation anchored', `${a.amount ?? '—'} glyphs · ${fmt.short(a.tx_hash, 6)}`, a.ts ? new Date(a.ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : '', 'earth')
  if (s.head.ok) row(I.swap, '', 'Finality certificate', `${s.head.finalityGate} · height ${fmt.int(s.head.finalityHeight)} · ${s.head.committee} validators`, `${fmt.int(s.head.lagBlocks)} blk`, 'blocks')
  return rows.join('') || `<div class="pempty">No activity read yet.</div>`
}

export function collectionModal(c: Collection, s: Snapshot): string {
  const items: string[] = []
  const item = (t: string, sub: string, m = '', h = '') => items.push(`<${h ? 'button' : 'div'} class="cm-item${h ? ' copyable' : ''}"${h ? ` data-copy="${esc(h)}" title="Click to copy ${esc(h.length === 64 ? 'this id' : 'this hash')}"` : ''}><div class="t">${esc(t)}</div><div class="s">${esc(sub)}</div>${m ? `<div class="m">${m}</div>` : ''}${h ? `<div class="h"><span class="hx">${esc(h)}</span><span class="cp">${I.copy}</span></div>` : ''}</${h ? 'button' : 'div'}>`)
  switch (c.id) {
    case 'rigs': for (const m of (s.miners?.miners ?? []).slice().sort((a, b) => b.hash_rate - a.hash_rate)) item(m.rig || fmt.short(m.wallet, 6), `${m.kind.toUpperCase()} · ${m.shielded ? 'shielded' : 'transparent'} · ${fmt.ago(m.last_seen_secs_ago)}`, fmt.hps(m.hash_rate) + ` <span class="muted">${s.miners && s.miners.net_hps ? (m.hash_rate / s.miners.net_hps * 100).toFixed(1) + '%' : ''}</span>`, m.wallet); break
    case 'honours': case 'bench': {
      const want = c.id === 'honours' ? 'HonourConferred' : 'JusticeAppointed'
      for (const e of (s.docket?.entries ?? []).filter((x) => x.kind === want)) { const ev = e.event as { order?: string; citation?: string; rank?: string; recipient?: number[]; justice?: number[]; approvals?: number; operator_cosigned?: boolean }; const who = hex(ev.justice ?? ev.recipient); item(ev.order ? `${ev.order} · ${fmt.short(who, 4)}` : `${(ev.rank || 'Justice').replace(/([A-Z])/g, ' $1').trim()} · ${fmt.short(who, 4)}`, ev.citation ? ev.citation.slice(0, 140) : (e.height ? `block ${fmt.int(e.height)}` : 'genesis bench'), `docket #${e.seq}${ev.approvals !== undefined ? ` · ${ev.approvals} approvals${ev.operator_cosigned ? ' · co-signed' : ''}` : ''}`, who || e.leaf) }
      break }
    case 'blocks': for (const b of (s.recent?.blocks ?? []).slice(0, 24)) item(`Block ${fmt.int(b.height)}`, `${b.is_blue ? 'blue' : 'red'} · blue score ${fmt.int(b.blue_score)}`, `producer ${fmt.short(hex(b.producer), 6)}`, hex(b.hash)); break
    case 'notes': { const h = s.head; const a = s.anchor; item('Notes in pool', 'sealed 32-byte notes', fmt.int(h.notes)); item(`Capacity (epoch ${a?.epoch ?? 0})`, 'Merkle leaves', fmt.int(h.capacity)); item('Free leaves', 'capacity − notes', fmt.int(h.capacity - h.notes) + ` · ${((h.capacity - h.notes) / Math.max(1, h.capacity) * 100).toFixed(1)}%`); item('Nullifiers', 'notes spent, ever', fmt.int(h.nullifiers) + ` · ${(h.nullifiers / Math.max(1, h.notes) * 100).toFixed(1)}% of notes`); item('Registered wallets', 'published viewing keys', fmt.int(h.registered)); item('Value locked', 'sum of shielded deposits', fmt.num(h.valueLocked, 4) + ' SIGIL'); item('Anchor', 'current Merkle root the proofs bind to', '', a?.anchor); break }
    case 'earth': { const a = s.earth?.attest_last?.anchor; if (a) { const row = a.memo && /:(\d+):/.test(a.memo) ? RegExp.$1 : null; item('Last attestation' + (row ? ` · row #${row}` : ''), a.ts ? new Date(a.ts).toLocaleString() : '', `${a.amount ?? '—'} glyphs + ${a.fee ? fmt.int(a.fee) : '—'} fee`, a.tx_hash || ''); item('Attestation digest', 'BLAKE3 over the row, in the memo', '', (a.memo || '').split(':').pop() || '—'); if (a.wallet) item('Attester wallet', 'published viewing key', '', a.wallet) } else item('No attestation read', 'sigil-earth offline?'); break }
    case 'treasury': { const n = s.nation; item('Treasury', 'nation welfare vault', fmt.num(s.head.treasurySigil, 4) + ' SIGIL', n?.treasury_wallet); item('Stipend', `paid in ${n?.payout_asset ?? 'USDS'} · every ${n ? fmt.int(n.claim_interval_blocks) : '—'} blocks`, n ? fmt.num(glyphsToSigil(n.stipend_glyphs), 2) + ' SIGIL' : '—'); item('Financed by', 'carve of the mining dev fee', n ? `${n.welfare_bps} of 750 bps` : '—'); item('Active since', 'activation height', n ? `block ${fmt.int(n.activation_height)}` : '—'); item('Authority', 'nation authority wallet', '', n?.authority_wallet); break }
    case 'rocky': { const r = s.rocky; if (r) { item(r.symbol, r.name, r.live ? (r.bootstrapped ? 'live · bootstrapped' : 'live · awaiting bootstrap') : 'gated', r.token); item('Total supply', 'minted so far', fmt.num(glyphsToSigil(r.total_supply, r.decimals), 4) + ' ROCKY'); item('Pool balance', 'cover pool', fmt.num(glyphsToSigil(r.pool_balance, r.decimals), 4) + ' ROCKY'); item('Fee', 'reflection fee', r.fee_bps + ' bps'); item('Policies', 'cover policies written', String(r.policies.length)); item('Owner', r.bootstrapped ? 'contract owner' : 'intended owner — bootstrap pending', '', (r as unknown as { owner?: string; intended_owner?: string }).owner || (r as unknown as { intended_owner?: string }).intended_owner || '—'); item('Contract', 'native sigil-vm contract', '', r.contract) } break }
    case 'bridge': { const b = s.bridge; item('Locks', 'SIGIL locked for the Polygon leg', String(b?.lock_count ?? 0)); item('Vault balance', 'what the vault holds now', b ? fmt.num(glyphsToSigil(b.vault_balance), 4) + ' SIGIL' : '—', b?.vault); item('Relayer', b?.paused ? 'bridge paused' : 'held — mints wait on the operator', b?.paused ? 'paused' : 'open', b?.relayer); break }
    case 'coins': { const free = Math.max(0, s.head.capacity - s.head.notes); item('No batch anchored', 'the collection is empty on chain', '0 coins'); item('Room in the pool', 'one note per coin · free leaves this epoch', fmt.int(free) + ' coins possible'); item('Tag', 'NTAG215 · 504 B · a note is 32 B', 'first tap wins'); item('Denomination', 'operator decision', '1–10 SIGIL each'); item('Wallet', 'mint + claim shipped in Android 1.11+', 'not yet tested on a tag'); break }
    default: item('Illustrative', 'no on-chain items for this collection', '—')
  }
  return `<div class="cm-head" style="background-image:url('${c.cover}')"><button class="ibtn x" id="modalClose">${I.x}</button></div>
    <div class="cm-title"><img src="${c.cover}" alt=""><div><h3>${esc(c.name)}${ver(c.verified)}</h3><div class="by">${esc(c.by || '')} ${prov(c.provenance)}</div></div></div>
    <div class="cm-stats"><div class="cell"><div class="k">Floor</div><div class="v">${esc(c.floor)}</div></div><div class="cell"><div class="k">Items</div><div class="v">${c.items === null ? '—' : fmt.int(c.items)}</div></div><div class="cell"><div class="k">Owners</div><div class="v">${c.owners === null ? '—' : fmt.int(c.owners)}</div></div><div class="cell"><div class="k">Volume</div><div class="v">${esc(c.volume)}</div></div></div>
    <div class="cm-tabs"><button class="on" data-cmtab="items">Items <span class="n">${items.length}</span></button><button data-cmtab="activity">Activity</button><button data-cmtab="info">Info</button></div>
    <div class="cm-body">
      <div class="cm-pane" data-pane="items"><div class="cm-items">${items.join('') || '<div class="pempty">Nothing to list.</div>'}</div></div>
      <div class="cm-pane" data-pane="activity" hidden>${collectionActivity(c, s)}</div>
      <div class="cm-pane" data-pane="info" hidden><p class="blurb">${esc(c.blurb)}</p><div class="td-grid">
        <div class="td-cell" style="grid-column: span 2"><div class="k">Provenance</div><div class="v">${prov(c.provenance)} — ${c.provenance === 'live' ? 'read from sigil-api this poll' : c.provenance === 'derived' ? 'calculated over live numbers' : 'illustrative, no chain source yet'}</div></div>
        <div class="td-cell"><div class="k">Category</div><div class="v">${esc(c.cat)}</div></div>
        <div class="td-cell"><div class="k">By</div><div class="v">${esc(c.by || '—')}</div></div>
        <div class="td-cell" style="grid-column: span 2"><div class="k">Source page</div><div class="v"><a class="mono" href="${c.link}" target="_blank" rel="noopener">${esc(c.link)}</a></div></div>
        <div class="td-cell two"><div class="k">Window changes</div><div class="v mono" style="white-space:nowrap">${['1h', '6h', '24h', '7d'].map((w) => `<span style="color:var(--muted)">${w}</span> ${fmt.pct(s.changes[c.id]?.[w] ?? null)}`).join(' &nbsp;')}</div></div>
        <div class="td-cell two"><div class="k">Last poll</div><div class="v mono" style="white-space:nowrap">${new Date(s.at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })} · h ${fmt.int(s.head.height)}</div></div>
      </div></div>
    </div>
    <div class="cm-foot"><a class="btn primary" href="${c.link}" target="_blank" rel="noopener">Open source page ↗</a><button class="btn ghost" id="modalClose2">Close</button></div>`
}

export function ticker(s: Snapshot): string {
  const items: string[] = []
  const it = (icon: string, label: string, val: string, cls = '', coll = '') => items.push(`<button class="tk-item ${cls}" tabindex="-1"${coll ? ` data-coll="${coll}"` : ''}><span class="tk-ic">${icon}</span><span class="tk-l">${label}</span><span class="tk-v">${val}</span></button>`)
  if (s.head.ok) it(I.coins, 'network', `${fmt.hps(s.head.netHps)} · ${s.head.liveMiners} rigs${s.head.blkPerSec ? ' · ' + s.head.blkPerSec.toFixed(1) + ' blk/s' : ''}`, 'gold', 'rigs')
  const behind = s.recent?.blocks.length && s.head.height ? Math.max(0, s.head.height - s.recent.blocks[0].height) : 0
  // the recent set's lag is one fact, said once — it used to trail every block item
  if (behind > 50) it(I.grid, 'recent set', `${fmt.int(behind)} blk behind the tip · ${s.head.blkPerSec ? '≈ ' + fmt.ago(behind / s.head.blkPerSec).replace(' ago', '') + ' old' : 'age unknown'}`, '', 'blocks')
  for (const b of (s.recent?.blocks ?? []).slice(0, 6)) it(I.grid, `block ${fmt.int(b.height)}`, `${b.is_blue ? 'blue' : 'red'} · score ${fmt.int(b.blue_score)} · ${fmt.short(hex(b.producer), 4)}`, b.is_blue ? 'blue' : 'red', 'blocks')
  for (const m of (s.miners?.miners ?? []).slice(0, 4)) it(I.coins, m.rig || fmt.short(m.wallet, 6), `${fmt.hps(m.hash_rate)} · ${fmt.ago(m.last_seen_secs_ago)}`, '', 'rigs')
  const a = s.earth?.attest_last?.anchor
  if (a?.tx_hash) it(I.eye, 'K⊕ attest', `${a.amount ?? '—'} glyphs · ${fmt.short(a.tx_hash, 6)}`, 'gold', 'earth')
  for (const e of (s.docket?.entries ?? []).slice(-2)) it(I.book, `docket #${e.seq}`, e.kind, 'gold', e.kind === 'HonourConferred' ? 'honours' : 'bench')
  if (s.head.ok) it(I.swap, 'finality', `${s.head.finalityGate} · ${fmt.int(s.head.lagBlocks)} blk behind tip`, '', 'blocks')
  if (!items.length) return ''
  const row = items.join('')
  return row + row // duplicated so the marquee loops seamlessly
}

function collectionActivity(c: Collection, s: Snapshot): string {
  const rows: string[] = []
  const row = (ic: string, cls: string, main: string, sub: string, t: string) => rows.push(`<div class="pact"><span class="ic ${cls}">${ic}</span><span class="w">${main}<small>${sub}</small></span><span class="t">${t}</span></div>`)
  switch (c.id) {
    case 'blocks': for (const b of (s.recent?.blocks ?? []).slice(0, 30)) row(I.grid, b.is_blue ? 'blue' : 'red', `Block ${fmt.int(b.height)}`, `${b.is_blue ? 'blue' : 'red'} · score ${fmt.int(b.blue_score)} · producer ${fmt.short(hex(b.producer), 6)}`, blockAgo(s, b.height)); break
    case 'rigs': for (const m of (s.miners?.miners ?? [])) row(I.coins, '', esc(m.rig || fmt.short(m.wallet, 6)), `${fmt.hps(m.hash_rate)} · ${m.kind.toUpperCase()} · ${m.shielded ? 'shielded' : 'transparent'}`, fmt.ago(m.last_seen_secs_ago)); if (s.miners) row(I.swap, '', 'Blocks accepted', `${fmt.int(s.miners.blocks_accepted)} blocks · ${fmt.int(s.miners.shares_accepted)} shares`, 'session'); break
    case 'honours': case 'bench': for (const e of (s.docket?.entries ?? []).slice().reverse()) { const ev = e.event as { rank?: string; order?: string; justice?: number[]; recipient?: number[] }; row(I.book, 'gold', esc(ev.order ? `${ev.order} conferred` : `${(ev.rank || 'Justice').replace(/([A-Z])/g, ' $1').trim()} appointed`), `${fmt.short(hex(ev.justice ?? ev.recipient), 6)} · docket #${e.seq} · leaf ${fmt.short(e.leaf, 6)}`, e.height ? `blk ${fmt.int(e.height)}` : 'genesis') } break
    case 'notes': row(I.eye, '', 'Nullifiers', `${fmt.int(s.head.nullifiers)} notes spent, ever`, 'total'); row(I.eye, '', 'Notes in pool', `${fmt.int(s.head.notes)} of ${fmt.int(s.head.capacity)}`, 'now'); break
    case 'earth': { const a = s.earth?.attest_last?.anchor; if (a?.tx_hash) row(I.eye, 'gold', 'Attestation anchored', `${a.amount ?? '—'} glyphs · tx ${fmt.short(a.tx_hash, 8)}`, a.ts ? new Date(a.ts).toLocaleString() : ''); break }
    default: break
  }
  return rows.join('') || '<div class="pempty">No activity the node reports for this collection.</div>'
}

export function legend(s: Snapshot): string {
  const live = s.collections.filter((c) => c.provenance === 'live').length
  const pretend = s.collections.filter((c) => c.provenance === 'pretend').length
  return `<div class="l"><strong style="color:var(--ink)">What is measured here</strong><a class="viewall" href="/downloads/sigil-vm-inventory.md" target="_blank" rel="noopener" style="margin-left:auto">Full inventory · what is real, what is mock</a></div>
    <div class="l">${prov('live')}<span>read from sigil-api on this poll (${live} collections, every strip figure, tokens SIGIL/USDS/ROCKY, drops, sales proofs)</span></div>
    <div class="l">${prov('derived')}<span>a stated calculation over live numbers (block rate, rig change vs. today's first sample, network hashrate window)</span></div>
    <div class="l">${prov('pretend')}<span>illustrative — no chain source yet (${pretend} collection${pretend === 1 ? '' : 's'}, wSIGIL3/SSHARE rows, USD prices, the cart)</span></div>
    <div class="l"><span class="mono">${s.offline ? 'node unreachable — showing shells only' : 'last poll ' + new Date(s.at).toLocaleTimeString()} · window changes sampled in this browser for ${s.sampleAgeMin < 60 ? s.sampleAgeMin + ' min' : (s.sampleAgeMin / 60).toFixed(1) + ' h'} (1h needs ≥24 min, 24h ≥ 9.6 h, 7d ≥ 2.8 d)</span></div>`
}

export { esc, prov, delta, glyphsToSigil }
