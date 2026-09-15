// ui.ts — pure renderers: snapshot → HTML strings. No fetches here.
import { fmt, glyphsToSigil } from './api'
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
const ver = (v: boolean) => (v ? '<span class="verified" title="on-chain record family">✓</span>' : '')

// ── shell ─────────────────────────────────────────────────────────────────
export function shell(): string {
  return `
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
      <div class="chain-chip" id="chainChip" title="sigil-g2 · sigil-api"><i class="d"></i><span class="t">sigil-g2</span><span id="chainHeight">—</span></div>
      <button class="ibtn" id="bellBtn" title="Activity">${I.bell}<i class="badge" id="bellBadge" hidden></i></button>
      <button class="ibtn" id="cartBtn" title="Cart (coming with listings)">${I.cart}</button>
      <button class="btn primary" id="walletBtn">${I.wallet}<span id="walletLbl">Connect wallet</span></button>
      <button class="ibtn" id="panelBtn" title="Toggle wallet panel">${I.panel}</button>
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
    <section id="market" class="section" style="margin-top:0">
      <div class="hero" id="hero"><div class="bg"></div><div class="veil"></div><div class="body"><div class="eyebrow"><span class="chip gold">reading the chain…</span></div><h1>SIGIL VM</h1></div></div>
      <div class="strip" id="strip"></div>
      <div id="foryou"></div>
    </section>
    <section id="featured" class="section">
      <div class="sec-head"><div><h2>Featured Collections</h2><div class="sub">Record families the chain can prove — ranked by items on chain.</div></div><div class="right"><div class="arrows" data-scroll="featuredRow"><button>${I.left}</button><button>${I.right}</button></div><a class="viewall" href="#trending">View all</a></div></div>
      <div class="row" id="featuredRow">${skeleton(5)}</div>
    </section>
    <section id="drops" class="section">
      <div class="sec-head"><div><h2>Featured Drops</h2><div class="sub">Activations gated by block height. Countdowns are measured from the live height and block rate.</div></div><div class="right"><div class="arrows" data-scroll="dropsRow"><button>${I.left}</button><button>${I.right}</button></div></div></div>
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
      <div class="trending" id="trendingTable"></div>
    </section>
    <section id="movers" class="section">
      <div class="sec-head"><div><h2>Top Movers Today</h2><div class="sub">Rigs by hashrate. Change is measured against the first sample this page took today (kept in your browser).</div></div><div class="right"><div class="arrows" data-scroll="moversRow"><button>${I.left}</button><button>${I.right}</button></div></div></div>
      <div class="row" id="moversRow">${skeleton(6, 'mover')}</div>
    </section>
    <section id="sales" class="section">
      <div class="sec-head"><div><h2>Highest Weekly Sales</h2><div class="sub">Settlements the chain can prove this week — each card carries its tx hash or docket leaf. No marketplace sales exist on SIGIL yet; nothing here is a price.</div></div><div class="right"><div class="arrows" data-scroll="salesRow"><button>${I.left}</button><button>${I.right}</button></div></div></div>
      <div class="row" id="salesRow">${skeleton(6, 'sale')}</div>
    </section>
    <section id="dex" class="section">
      <div class="sec-head"><div><h2>Swap &amp; Tokens</h2><div class="sub">The Quillon Graph DEX modules, on SIGIL: the swap module and the token list side by side. Signing happens in your wallet.</div></div><div class="right"><a class="viewall" href="/sigil-dex.html" target="_blank" rel="noopener">Trading desk</a></div></div>
      <div class="dex"><div id="swap"></div><div id="tokens"></div></div>
    </section>
    <div class="legend" id="legend"></div>
    <footer class="foot">
      <span>SIGIL VM · sigilgraph.org</span>
      <a href="/api.html" target="_blank" rel="noopener">API</a>
      <a href="/sigil-explorer.html" target="_blank" rel="noopener">Explorer</a>
      <a href="/sigil-wallet-tron-embedded.html" target="_blank" rel="noopener">Wallet</a>
      <a href="/sigil-dex.html" target="_blank" rel="noopener">DEX desk</a>
      <a href="/kristensen-board.html" target="_blank" rel="noopener">K board</a>
      <a href="/datacenter.html" target="_blank" rel="noopener">Datacenter</a>
      <span id="footTs" class="mono"></span>
    </footer>
  </main>
  <aside class="panel" id="panel">
    <div class="ph" id="panelHead"></div>
    <div class="ptabs" id="panelTabs"><button class="on" data-ptab="tokens">Tokens<span class="n" id="nTok"></span></button><button data-ptab="nfts">NFTs<span class="n" id="nNft"></span></button><button data-ptab="activity">Activity</button></div>
    <div class="pbody" id="panelBody"></div>
    <div class="pf"><button class="btn ghost" id="pSend">Send</button><button class="btn ghost" id="pReceive">Receive</button><button class="btn primary" id="pSwap">Swap</button></div>
  </aside>
  <div class="modal" id="modal"><div class="box" id="modalBox"></div></div>
  <div class="toasts" id="toasts"></div>`
}

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
    <div class="dots">${list.map((_, i) => `<i class="${i === idx % list.length ? 'on' : ''}"></i>`).join('')}</div>
    <div class="body">
      <div class="eyebrow">${prov(c.provenance)}<span class="chip gold">featured collection</span>${h.ok ? '<span class="chip live"><i class="d"></i>sigil-g2 · block ' + fmt.int(h.height) + '</span>' : '<span class="chip bad">node unreachable</span>'}</div>
      <h1>${esc(c.name)}${ver(c.verified)}</h1>
      <p>${esc(c.blurb)}</p>
      <div class="stats">
        <div><div class="k">Floor</div><div class="v">${esc(c.floor)}</div></div>
        <div><div class="k">Items</div><div class="v">${c.items === null ? '—' : fmt.int(c.items)}</div></div>
        <div><div class="k">Owners</div><div class="v">${c.owners === null ? '—' : fmt.int(c.owners)}</div></div>
        <div><div class="k">Volume</div><div class="v">${esc(c.volume)}</div></div>
      </div>
      <div class="cta"><a class="btn primary lg" href="${c.link}" target="_blank" rel="noopener">View collection</a><a class="btn ghost lg" href="#swap">Swap SIGIL</a></div>
    </div>
    <div class="thumbs">${list.map((x, i) => `<button data-hero="${i}" class="${i === idx % list.length ? 'on' : ''}" title="${esc(x.name)}"><img src="${x.cover}" alt=""></button>`).join('')}</div>`
}

export function strip(s: Snapshot): string {
  const h = s.head
  const cell = (k: string, v: string, small = '') => `<div class="cell"><div class="k">${k}</div><div class="v">${v}</div>${small ? `<div class="s">${small}</div>` : ''}</div>`
  return [
    cell('Height', fmt.int(h.height), h.blkPerSec ? `${h.blkPerSec.toFixed(1)} blk/s` : 'measuring rate…'),
    cell('Finality', h.finalityGate, `h ${fmt.int(h.finalityHeight)} · ${h.committee} validators`),
    cell('Supply', fmt.num(h.supplySigil) + ' SIGIL', `${(h.mintedPct).toFixed(2)}% of 21M`),
    cell('Hashrate', fmt.hps(h.netHps), `${h.liveMiners} live miners`),
    cell('Shielded pool', fmt.int(h.notes) + ' notes', `${fmt.num(h.valueLocked)} SIGIL locked · ${fmt.int(h.nullifiers)} spent`),
    cell('Treasury', fmt.num(h.treasurySigil) + ' SIGIL', 'nation welfare'),
  ].join('')
}

export function forYou(s: Snapshot): string {
  const h = s.head
  const lagS = h.blkPerSec ? h.lagBlocks / h.blkPerSec : null
  const rockyDrop = s.drops.find((d) => d.id === 'rocky')
  const tag = (t: string, cls = '') => `<span class="chip ${cls}">${t}</span>`
  const next: string[] = []
  if (rockyDrop) next.push(`<li>${tag(rockyDrop.provenance === 'live' ? 'MEASURED' : 'DESIGN', rockyDrop.provenance === 'live' ? 'live' : 'gold')} <b>ROCKY + GaugePush activate</b> — ${esc(rockyDrop.when)}. Cover policies against K⊕ excursions become buyable.</li>`)
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
    <div class="img" style="background-image:url('${c.cover}')"><span class="prov">${prov(c.provenance)}</span></div>
    <div class="meta">
      <div class="name">${esc(c.name)}${ver(c.verified)}</div>
      <div class="blurb">${esc(c.blurb)}</div>
      <div class="kv"><div><div class="k">Floor</div><div class="v">${esc(c.floor)}</div></div><div style="text-align:right"><div class="k">Items</div><div class="v">${c.items === null ? '—' : fmt.int(c.items)}</div></div></div>
    </div></a>`
}

export function dropCard(d: Drop): string {
  const st: Record<Drop['status'], string> = { live: 'chip live', minting: 'chip derived', upcoming: 'chip gold', blocked: 'chip bad' }
  const lbl: Record<Drop['status'], string> = { live: 'live', minting: 'minting', upcoming: 'upcoming', blocked: 'blocked' }
  return `<a class="card drop wide" href="${d.link}" target="_blank" rel="noopener">
    <div class="img" style="background-image:url('${d.cover}')"><span class="prov">${prov(d.provenance)}</span><span class="status ${st[d.status]}"><i class="d"></i>${lbl[d.status]}</span></div>
    <div class="meta">
      <div class="name">${esc(d.name)}</div>
      <div class="when">${esc(d.when)}</div>
      ${d.progress !== null ? `<div class="bar"><i style="width:${(d.progress * 100).toFixed(2)}%"></i></div>` : ''}
      <div class="blurb" style="margin-top:8px">${esc(d.detail)}</div>
    </div></a>`
}

export function moverCard(m: Mover): string {
  return `<div class="card mover" title="${esc(m.id)}">
    <div class="img" style="background-image:url('${m.cover}')"><span class="prov">${prov(m.provenance)}</span></div>
    <div class="meta"><div class="name">${esc(m.name)}</div><div class="blurb" style="height:auto">${esc(m.sub)}</div>
      <div class="kv"><div><div class="k">Hashrate</div><div class="v">${esc(m.value)}</div></div><div style="text-align:right"><div class="k">Today</div><div class="delta">${delta(m.change)}</div></div></div>
    </div></div>`
}

export function saleCard(x: Sale): string {
  return `<div class="card sale" data-proof="${esc(x.proof)}">
    <div class="img" style="background-image:url('${x.cover}')"><span class="prov">${prov(x.provenance)}</span></div>
    <div class="meta"><div class="name">${esc(x.name)}</div><div class="blurb" style="height:auto">${esc(x.collection)} · ${esc(x.when)}</div>
      <div class="price">${esc(x.price)}</div><div class="proof" title="${esc(x.proof)}">${esc(x.proof)}</div></div></div>`
}

// ── trending table ────────────────────────────────────────────────────────
export function trending(s: Snapshot, mode: 'trending' | 'top', win: string): string {
  const list = [...s.collections]
  // "trending" = live record families with the most activity (sales/items), "top" = by items
  list.sort((a, b) => mode === 'top' ? (b.items ?? -1) - (a.items ?? -1) : ((b.sales ?? 0) * 3 + (b.items ?? 0) * 0.001 + (b.provenance === 'live' ? 1 : 0)) - ((a.sales ?? 0) * 3 + (a.items ?? 0) * 0.001 + (a.provenance === 'live' ? 1 : 0)))
  const half = Math.ceil(list.length / 2)
  const table = (rows: Collection[], off: number) => `<table><thead><tr><th>#</th><th>Collection</th><th class="r">Floor</th><th class="r">${win} chg</th><th class="r">Volume</th><th class="r">Items</th><th class="r">Owners</th></tr></thead><tbody>
    ${rows.map((c, i) => `<tr data-coll="${c.id}" data-link="${c.link}"><td class="rank">${off + i + 1}</td><td><div class="coll"><img src="${c.cover}" alt=""><div><div class="n">${esc(c.name)}${ver(c.verified)}</div><div class="s">${prov(c.provenance)}</div></div></div></td>
      <td class="r num">${esc(c.floor)}</td><td class="r num">${delta(c.floorChange ?? c.volumeChange)}</td><td class="r num">${esc(c.volume)}</td><td class="r num">${c.items === null ? '—' : fmt.int(c.items)}</td><td class="r num">${c.owners === null ? '—' : fmt.int(c.owners)}</td></tr>`).join('')}
  </tbody></table>`
  return `<div class="tcol">${table(list.slice(0, half), 0)}</div><div class="tcol">${table(list.slice(half), half)}</div>`
}

// ── DEX: swap module (Quillon-shaped) ─────────────────────────────────────
export interface SwapState { mode: 'market' | 'limit'; from: string; to: string; amount: string; limitPrice: string; limitDir: 'buy' | 'sell'; slippage: number }

export function swapModule(s: Snapshot, st: SwapState, balances: Record<string, number>, q: { out: number; impact: number } | null): string {
  const tk = (id: string) => s.tokens.find((t) => t.id === id) || s.tokens[0]
  const from = tk(st.from), to = tk(st.to)
  const bal = balances[from.id] ?? 0
  const amt = Number(st.amount) || 0
  const pctv = bal > 0 ? Math.min(100, (amt / bal) * 100) : 0
  const noPool = !s.pools.length
  const sel = (which: 'from' | 'to', t: Token) => `<button class="tsel" data-sel="${which}"><img src="${t.icon}" alt=""><span>${esc(t.symbol)}</span><span class="car">▼</span></button>`
  const marketForm = `
    <div class="field"><label><span>From</span><span class="bal">Balance: ${bal.toFixed(4)}<b data-max="1">MAX</b></span></label>
      <div class="amt"><input id="swapAmt" type="number" inputmode="decimal" placeholder="0.0" value="${esc(st.amount)}" min="0" step="any">${sel('from', from)}</div></div>
    <div class="slider"><div class="lab"><span>Quick Select Amount</span><b>${pctv.toFixed(0)}%</b></div>
      <div class="track"><div class="fill" style="width:${pctv}%"></div><input id="swapRange" type="range" min="0" max="100" step="1" value="${pctv.toFixed(0)}"></div>
      <div class="quick">${[25, 50, 75, 100].map((p) => `<button data-pct="${p}">${p}%</button>`).join('')}</div></div>
    <div class="flip"><button id="swapFlip" title="Flip">${I.updown}</button></div>
    <div class="field"><label><span>To (Estimated)</span><span class="bal">${to.symbol} · ${(balances[to.id] ?? 0).toFixed(4)}</span></label>
      <div class="amt"><input id="swapOut" type="text" readonly placeholder="0.0" value="${q ? q.out.toFixed(6) : ''}">${sel('to', to)}</div></div>
    ${noPool
      ? `<div class="info warnbox">No liquidity pool exists on sigil-g2 for ${esc(from.symbol)}/${esc(to.symbol)} yet — <span class="mono">/v1/pools</span> is empty, so there is no quote to show. The module is wired to the constant-product formula and lights up the moment a pool is added.</div>`
      : `<div class="info"><div><span class="k">Rate</span><span class="v">1 ${esc(from.symbol)} ≈ ${q && amt > 0 ? (q.out / amt).toFixed(6) : '—'} ${esc(to.symbol)}</span></div><div><span class="k">Price impact</span><span class="v ${q && q.impact > 5 ? 'down' : ''}">${q ? q.impact.toFixed(2) + '%' : '—'}</span></div><div><span class="k">Fee</span><span class="v">0.30%</span></div><div><span class="k">Slippage</span><span class="v">${st.slippage.toFixed(1)}%</span></div></div>`}
    <button class="btn primary lg full swapbtn" id="swapGo" ${noPool || amt <= 0 ? 'disabled' : ''}>${amt <= 0 ? 'Enter an amount' : noPool ? 'No pool yet' : `Swap ${esc(from.symbol)} → ${esc(to.symbol)}`}</button>
    <div class="muted" style="font-size:11.5px;margin-top:8px;text-align:center">Signing and the STARK proof happen in your wallet — this page never sees a seed.</div>`
  const limitForm = `<div class="limit">
    <div class="pair"><div style="flex:1"><div class="sm">Sell</div>${sel('from', from)}<div class="sm" style="text-align:right;margin-top:4px">Balance: ${bal.toFixed(4)}</div></div><div class="arrow">→</div><div style="flex:1"><div class="sm">Buy</div>${sel('to', to)}</div></div>
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

// ── DEX: token table (Quillon "Available Tokens") ─────────────────────────
export interface TokenTableState { q: string; filter: 'all' | 'gainers' | 'losers'; sort: keyof Token | 'supply'; dir: 'asc' | 'desc' }
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
        <td><div class="act"><button class="sw" data-swap="${t.id}">Swap</button><button data-info="${t.id}">Info</button></div></td></tr>`).join('')}
    </tbody></table></div>
    <div class="muted" style="font-size:11.5px;margin-top:10px">Price, 1h/24h/7d and volume read “—” because sigil-g2 has no price oracle feeding these tokens yet (USDS oracle: not fed; ROCKY: gated). Supply, holders, age and liquidity are live from the node.</div>
  </div></div>`
}

// ── wallet panel ──────────────────────────────────────────────────────────
export function panelHead(addr: string | null, s: Snapshot, balances: Record<string, number>): string {
  if (!addr) return `<div class="who"><img src="${avatarSvg('nobody', '?')}" alt=""><div><div class="a">No wallet connected</div><div class="b">Open the gate to load your SIGIL wallet</div></div></div><div class="total"><div class="k">Portfolio</div><div class="v">—</div></div>`
  const native = balances['0'.repeat(64)] ?? 0
  return `<div class="who"><img src="${avatarSvg(addr, addr.slice(0, 1).toUpperCase())}" alt=""><div><div class="a">${fmt.short(addr, 6)}</div><div class="b">sigil-g2 · ${s.head.ok ? 'live' : 'offline'}</div></div><button class="ibtn x" id="pDisconnect" title="Forget">${I.x}</button></div>
    <div class="total"><div class="k">Portfolio</div><div class="v">${fmt.num(native, 4)}<small>SIGIL</small></div><div class="muted" style="font-size:11.5px">USD value unavailable — no oracle price on chain yet</div></div>`
}

export function panelTokens(addr: string | null, s: Snapshot, balances: Record<string, number>): string {
  if (!addr) return `<div class="pempty"><div class="big">👛</div>Connect a wallet to see your tokens.<br><br><a class="btn primary" href="/enter-sigil.html" target="_blank" rel="noopener">Open the gate</a></div>`
  return s.tokens.map((t) => `<div class="prow"><img src="${t.icon}" alt=""><div class="l"><div class="s">${esc(t.symbol)}</div><div class="n">${esc(t.name)}</div></div><div class="r">${balances[t.id] === undefined ? '<span class="muted">—</span>' : fmt.num(balances[t.id], 4)}<div class="u">${t.status === 'live' ? 'on chain' : t.status}</div></div></div>`).join('')
}

export function panelNfts(addr: string | null, s: Snapshot): string {
  if (!addr) return `<div class="pempty"><div class="big">🖼️</div>Your on-chain records — honours, seats, rigs, attestations — appear here once a wallet is connected.</div>`
  const mine: { name: string; coll: string; cover: string }[] = []
  const a = addr.toLowerCase()
  for (const e of s.docket?.entries ?? []) {
    const ev = e.event as { recipient?: number[]; justice?: number[]; order?: string }
    const rec = (ev.recipient || ev.justice || []).map((b) => b.toString(16).padStart(2, '0')).join('')
    if (rec === a) mine.push({ name: e.kind === 'HonourConferred' ? (ev.order || 'Honour') : 'Justice seat', coll: e.kind === 'HonourConferred' ? 'Elefantordenen' : 'Justices of the Bench', cover: s.collections.find((c) => c.id === (e.kind === 'HonourConferred' ? 'honours' : 'bench'))!.cover })
  }
  for (const m of s.miners?.miners ?? []) if (m.wallet.toLowerCase() === a) mine.push({ name: m.rig || 'rig', coll: "Miners' Rigs", cover: s.collections.find((c) => c.id === 'rigs')!.cover })
  const ew = s.earth?.attest_last?.anchor?.wallet || ''
  if (ew.includes(a)) mine.push({ name: 'K⊕ attester', coll: 'Kristensen Earth', cover: s.collections.find((c) => c.id === 'earth')!.cover })
  if (!mine.length) return `<div class="pempty"><div class="big">🔮</div>No on-chain records for ${fmt.short(addr, 6)} yet.<br><span class="muted">Mine a block, earn an honour, or anchor an attestation.</span></div>`
  return `<div class="pgrid">${mine.map((n) => `<div class="pnft"><div class="img" style="background-image:url('${n.cover}')"></div><div class="m"><div class="n">${esc(n.name)}</div><div class="c">${esc(n.coll)}</div></div></div>`).join('')}</div>`
}

export function panelActivity(s: Snapshot): string {
  const rows: string[] = []
  const b = s.recent?.blocks.slice(0, 6) ?? []
  for (const x of b) rows.push(`<div class="pact"><span class="t">#${fmt.int(x.height)}</span><span class="w">${x.is_blue ? 'blue' : 'red'} block · blue score ${fmt.int(x.blue_score)}</span></div>`)
  for (const e of (s.docket?.entries ?? []).slice(-3).reverse()) rows.push(`<div class="pact"><span class="t">docket #${e.seq}</span><span class="w">${esc(e.kind)}</span></div>`)
  const a = s.earth?.attest_last?.anchor
  if (a?.tx_hash) rows.push(`<div class="pact"><span class="t">${a.ts ? new Date(a.ts).toLocaleTimeString() : ''}</span><span class="w">K⊕ attest anchored · ${fmt.short(a.tx_hash, 6)}</span></div>`)
  return rows.join('') || `<div class="pempty">No activity read yet.</div>`
}

export function legend(s: Snapshot): string {
  const live = s.collections.filter((c) => c.provenance === 'live').length
  const pretend = s.collections.filter((c) => c.provenance === 'pretend').length
  return `<div class="l"><strong style="color:#fff">What is measured here</strong></div>
    <div class="l">${prov('live')}<span>read from sigil-api on this poll (${live} collections, every strip figure, tokens SIGIL/USDS/ROCKY, drops, sales proofs)</span></div>
    <div class="l">${prov('derived')}<span>a stated calculation over live numbers (block rate, rig change vs. today's first sample, network hashrate window)</span></div>
    <div class="l">${prov('pretend')}<span>illustrative — no chain source yet (${pretend} collections, wSIGIL3/SSHARE rows, USD prices, the cart)</span></div>
    <div class="l"><span class="mono">${s.offline ? 'node unreachable — showing shells only' : 'last poll ' + new Date(s.at).toLocaleTimeString()}</span></div>`
}

export { esc, prov, delta, glyphsToSigil }
