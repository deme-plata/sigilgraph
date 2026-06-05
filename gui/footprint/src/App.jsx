import React, { useState, useEffect, useRef } from 'react'
import data from './data.json'

// ── formatting ───────────────────────────────────────────────────────────────
function human(bytes) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  let v = bytes, i = 0
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++ }
  return { n: v, u: u[i], s: `${v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} ${u[i]}` }
}
function sci(n) {
  const e = Math.floor(Math.log10(n))
  return `${(n / 10 ** e).toFixed(2)}×10${supr(e)}`
}
function supr(e) { const m = { '-': '⁻', 0: '⁰', 1: '¹', 2: '²', 3: '³', 4: '⁴', 5: '⁵', 6: '⁶', 7: '⁷', 8: '⁸', 9: '⁹' }; return String(e).split('').map(c => m[c] || c).join('') }

// ── count-up number ──────────────────────────────────────────────────────────
function CountUp({ to, render, dur = 1400, delay = 0 }) {
  const [v, setV] = useState(0)
  useEffect(() => {
    let raf, t0
    const id = setTimeout(() => {
      const tick = (t) => { if (!t0) t0 = t; const p = Math.min(1, (t - t0) / dur); setV(to * (1 - Math.pow(1 - p, 3))); if (p < 1) raf = requestAnimationFrame(tick) }
      raf = requestAnimationFrame(tick)
    }, delay)
    // safety net: guarantee the final value even where rAF is throttled
    // (e.g. headless render-verify under virtual-time) so screenshots are real.
    const settle = setTimeout(() => setV(to), delay + dur + 120)
    return () => { clearTimeout(id); clearTimeout(settle); cancelAnimationFrame(raf) }
  }, [to])
  return <>{render(v)}</>
}

const TIERS = {
  json:    { c: 'var(--json)',    key: 'json_rs_bytes',      pk: 'json',         name: 'JSON snapshot', tag: 'today' },
  full:    { c: 'var(--full)',    key: 'fluxdb_full_bytes',  pk: 'fluxdb_full',  name: 'SIGIL full node', tag: 'archival' },
  history: { c: 'var(--history)', key: 'fluxdb_history_bytes', pk: 'fluxdb_history', name: 'SIGIL wallet node', tag: 'history-prune' },
  pruned:  { c: 'var(--pruned)',  key: 'fluxdb_pruned_bytes', pk: 'fluxdb_pruned', name: 'SIGIL light node', tag: 'state-only' },
}
const p5 = data.projection_100y.find((p) => p.cadence === '5s')

// ── growth chart (log-scale SVG) ─────────────────────────────────────────────
function GrowthChart() {
  const W = 1000, H = 340, PL = 64, PR = 18, PT = 18, PB = 34
  const c = data.curve
  const xs = c.map((d) => d.height)
  const xmin = 0, xmax = Math.max(...xs)
  const allY = c.flatMap((d) => Object.values(TIERS).map((t) => d[t.key]))
  const ymin = Math.min(...allY.filter((v) => v > 0)), ymax = Math.max(...allY)
  const lymin = Math.log10(ymin), lymax = Math.log10(ymax)
  const X = (h) => PL + ((h - xmin) / (xmax - xmin)) * (W - PL - PR)
  const Y = (b) => PT + (1 - (Math.log10(b) - lymin) / (lymax - lymin)) * (H - PT - PB)
  const path = (t) => c.map((d, i) => `${i ? 'L' : 'M'}${X(d.height).toFixed(1)},${Y(d[t.key]).toFixed(1)}`).join(' ')
  const ticks = [1e6, 1e7, 1e8].filter((v) => v >= ymin / 2 && v <= ymax * 2)
  return (
    <svg viewBox={`0 0 ${W} ${H}`} style={{ width: '100%', height: 'auto' }}>
      {ticks.map((tk) => (
        <g key={tk}>
          <line className="gridline" x1={PL} x2={W - PR} y1={Y(tk)} y2={Y(tk)} />
          <text className="axis-lbl" x={PL - 8} y={Y(tk) + 3} textAnchor="end">{human(tk).s}</text>
        </g>
      ))}
      {[0, 5000, 10000, 15000, 20000].map((h) => (
        <text key={h} className="axis-lbl" x={X(h)} y={H - 10} textAnchor="middle">{h / 1000}k</text>
      ))}
      {Object.values(TIERS).map((t) => {
        const d = path(t)
        return <path key={t.name} className="series draw" d={d} stroke={t.c}
          style={{ strokeDasharray: 3000, strokeDashoffset: 3000, animation: `dash 2s 0.2s forwards` }} />
      })}
      <style>{`@keyframes dash { to { stroke-dashoffset: 0; } }`}</style>
    </svg>
  )
}

// ── section wrapper — animates in on mount, but is visible by default (no
// scroll-gating), so content never gets stuck invisible if animations are
// throttled (slow JS, headless render-verify). ─────────────────────────────
function Reveal({ children, delay = 0, style }) {
  return <section style={{ animationDelay: `${delay}ms`, ...style }}>{children}</section>
}

export default function App() {
  const pb = data.per_block_on_disk
  const shrink = data.shrink_vs_json
  const ladder = [
    { k: 'json', label: 'serde_json', v: pb.json, x: 1 },
    { k: 'full', label: 'flux-full', v: pb.fluxdb_full, x: shrink.fluxdb_full },
    { k: 'history', label: 'flux-history', v: pb.fluxdb_history, x: shrink.fluxdb_history },
    { k: 'pruned', label: 'flux-pruned', v: pb.fluxdb_pruned, x: shrink.fluxdb_pruned },
  ]
  const maxv = Math.max(...ladder.map((l) => l.v))
  const [grow, setGrow] = useState(false)
  useEffect(() => { const t = setTimeout(() => setGrow(true), 250); return () => clearTimeout(t) }, [])

  return (
    <>
      <div className="topbar">
        <div className="cube">◈</div>
        <div className="brand">SIGIL · <b>Graph Footprint</b></div>
        <div className="badge">● measured, not estimated</div>
      </div>

      <div className="wrap">
        <div className="hero">
          <div className="kicker">flux-db store · chronos 20,000 real blocks</div>
          <h1>How big is the SIGIL graph<br />in <span className="g">100 years?</span></h1>
          <p>
            We replaced the JSON snapshot with a real <b>flux-db</b> block store (bincode + LZ4 SSTs),
            then drove 20,000 deterministic blocks through it with <b>chronos</b> and measured the actual
            bytes on disk. Below: the 100-year graph at a 5-second cadence, across four storage tiers.
          </p>

          <div style={{ marginTop: 26, display: 'inline-flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', padding: '10px 16px', borderRadius: 12, border: '1px solid var(--full)', background: 'rgba(129,140,248,.10)', fontSize: 14 }}>
            <span style={{ fontSize: 11, fontWeight: 700, color: '#0a0a0f', background: 'var(--full)', padding: '3px 9px', borderRadius: 999 }}>★ DECISION</span>
            <span><b>SIGIL ships full-node only.</b> <span style={{ color: 'var(--dim)' }}>Pruning is an off-by-default lever — <code style={{ fontFamily: "'JetBrains Mono',monospace", color: 'var(--accent-bright)' }}>SIGIL_RETENTION</code>, reversible, no rebuild.</span></span>
          </div>

          <div className="cards">
            {['json', 'full', 'history', 'pruned'].map((k, i) => {
              const t = TIERS[k], val = p5[t.key], h = human(val)
              const chosen = k === 'full'
              const lever = k === 'history' || k === 'pruned'
              return (
                <div className="card" key={k} style={{
                  '--c': t.c, animationDelay: `${0.3 + i * 0.1}s`,
                  opacity: lever ? 0.6 : 1,
                  boxShadow: chosen ? '0 0 0 1px var(--full), 0 0 26px rgba(129,140,248,.28)' : 'none'
                }}>
                  <div className="lbl">{t.name} <span className="x" style={{ color: t.c }}>· {t.tag}</span>
                    {chosen && <span style={{ display: 'block', marginTop: 5, fontSize: 10, fontWeight: 700, color: '#0a0a0f', background: 'var(--full)', padding: '2px 7px', borderRadius: 999, width: 'fit-content' }}>★ SIGIL DEFAULT</span>}
                    {lever && <span style={{ display: 'block', marginTop: 5, fontSize: 10, fontWeight: 600, color: 'var(--dim)', border: '1px solid var(--line)', padding: '2px 7px', borderRadius: 999, width: 'fit-content' }}>off by default</span>}
                  </div>
                  <div className="val">
                    <CountUp to={h.n} delay={350 + i * 110} render={(v) => v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} />
                    <span style={{ fontSize: 16, marginLeft: 4 }}>{h.u}</span>
                  </div>
                  <div className="sub">{k === 'json' ? "today's path" : <>{(pb.json / pb[t.pk]).toFixed(1)}× smaller than JSON</>}</div>
                </div>
              )
            })}
          </div>
        </div>

        {/* LADDER */}
        <Reveal>
          <div className="h2">Per-block, on disk</div>
          <div className="lead">The win is the <span style={{ color: 'var(--accent-bright)' }}>encoding</span>, not the compressor <small>— measured over {data.blocks_produced.toLocaleString()} blocks</small></div>
          <div className="panel">
            {ladder.map((l) => (
              <div className="bar-row" key={l.k}>
                <div className="name" style={{ color: TIERS[l.k].c }}>{l.label}</div>
                <div className="bar-track">
                  <div className="bar-fill" style={{ width: grow ? `${Math.max(6, (l.v / maxv) * 100)}%` : 0, background: TIERS[l.k].c }}>
                    {l.v.toFixed(0)} B
                  </div>
                </div>
                <div className="bar-meta">{l.x === 1 ? <b>baseline</b> : <><b>{l.x.toFixed(1)}×</b> smaller</>}</div>
              </div>
            ))}
            <div className="chart-note">
              High-entropy hashes + the two 292-byte SQIsign signatures sit at the entropy floor, so LZ4 barely helps the full block —
              going from JSON <i>text</i> to <b>binary</b> is what shrinks it 3×. Pruning the witness data is what gets you to 18×.
            </div>
          </div>
        </Reveal>

        {/* GROWTH CHART */}
        <Reveal delay={60}>
          <div className="h2">Real on-disk growth</div>
          <div className="lead">20,000 SIGIL blocks on disk <small>(stored via the flux-db engine; log scale, includes LSM overhead — SSTs, bloom filters, WAL)</small></div>
          <div className="panel">
            <div className="chart-legend">
              {Object.values(TIERS).map((t) => (
                <span key={t.name}><span className="dot" style={{ background: t.c }} />{t.name}</span>
              ))}
            </div>
            <GrowthChart />
            <div className="chart-note">Every tier grows perfectly linearly — no surprise blow-up. The y-gap between lines is the whole story: ~26× from JSON+RS down to pruned.</div>
          </div>
        </Reveal>

        {/* PROJECTION TABLE */}
        <Reveal delay={60}>
          <div className="h2">100-year projection</div>
          <div className="lead">Measured per-block rates × blocks-per-century</div>
          <div className="panel" style={{ overflowX: 'auto' }}>
            <table>
              <thead>
                <tr><th>block cadence</th><th>blocks / 100y</th><th>JSON+RS</th><th>flux-full</th><th>flux-history</th><th>flux-pruned</th></tr>
              </thead>
              <tbody>
                {data.projection_100y.map((p) => (
                  <tr key={p.cadence} className={p.cadence === '5s' ? 'hot' : ''}>
                    <td>{p.cadence}{p.cadence === '5s' && <span className="pill">Quillon-like</span>}</td>
                    <td>{sci(p.blocks_100y)}</td>
                    <td style={{ color: 'var(--json)' }}>{human(p.json_rs_bytes).s}</td>
                    <td style={{ color: 'var(--full)' }}>{human(p.fluxdb_full_bytes).s}</td>
                    <td style={{ color: 'var(--history)' }}>{human(p.fluxdb_history_bytes).s}</td>
                    <td style={{ color: 'var(--pruned)' }}>{human(p.fluxdb_pruned_bytes).s}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Reveal>

        {/* TIERS / INTEGRITY */}
        <Reveal delay={60}>
          <div className="h2">Storage model — SIGIL ships full-node only</div>
          <div className="lead">Full is the decision. Pruning is a reversible lever, off by default <small>— flip via <code style={{ fontFamily: "'JetBrains Mono',monospace", color: 'var(--accent-bright)' }}>SIGIL_RETENTION</code>, no code change</small></div>
          <div className="tiers">
            <div className="tier" style={{ '--tc': 'var(--full)', boxShadow: '0 0 0 1px var(--full), 0 0 28px rgba(129,140,248,.22)' }}>
              <h4>Full <span style={{ color: 'var(--dim)', fontWeight: 400, fontSize: 14 }}>· archival node</span>
                <span style={{ fontSize: 11, fontWeight: 700, color: '#0a0a0f', background: 'var(--full)', padding: '3px 9px', borderRadius: 999, marginLeft: 4 }}>★ SIGIL DEFAULT</span>
                <span className="sz">{human(pb.fluxdb_full).s}/blk</span></h4>
              <div className="desc">Header + both SQIsign sigs + VDF/STARK proofs + transition + events. The complete record — the network's memory + its independent auditor. No data-availability dependency on anyone.</div>
              <div className="caps"><span className="cap y">✓ re-verify from genesis</span><span className="cap y">✓ full tx history offline</span><span className="cap y">✓ bootstraps new peers</span><span className="cap y">✓ self-sufficient</span></div>
            </div>
            <div className="tier" style={{ '--tc': 'var(--history)', opacity: .82 }}>
              <h4>History-prune <span style={{ color: 'var(--dim)', fontWeight: 400, fontSize: 14 }}>· wallet / indexer node</span>
                <span style={{ fontSize: 10.5, fontWeight: 600, color: 'var(--dim)', border: '1px solid var(--line)', padding: '2px 8px', borderRadius: 999, marginLeft: 4 }}>lever · off by default</span>
                <span className="sz">{human(pb.fluxdb_history).s}/blk</span></h4>
              <div className="desc">Drops the crypto witness (the ~600 B of signatures/proofs) but keeps the transition + events. It trusts its own past verification, but can still serve everything a wallet UI needs.</div>
              <div className="caps"><span className="cap n">✗ re-verify producer sig</span><span className="cap y">✓ full tx history</span><span className="cap y">✓ state + inclusion proofs</span></div>
            </div>
            <div className="tier" style={{ '--tc': 'var(--pruned)', opacity: .82 }}>
              <h4>State-only <span style={{ color: 'var(--dim)', fontWeight: 400, fontSize: 14 }}>· light state node</span>
                <span style={{ fontSize: 10.5, fontWeight: 600, color: 'var(--dim)', border: '1px solid var(--line)', padding: '2px 8px', borderRadius: 999, marginLeft: 4 }}>lever · off by default</span>
                <span className="sz">{human(pb.fluxdb_pruned).s}/blk</span></h4>
              <div className="desc">Keeps only the consensus-committed core: the 4 state roots, parent-hash chain, and identity. The 18× archival floor — but it depends on an archive + a proof to do everything.</div>
              <div className="caps"><span className="cap n">✗ re-verify (alone)</span><span className="cap n">✗ tx history offline</span><span className="cap y">✓ tx history via authenticated fetch</span><span className="cap y">✓ verify any tx given a proof</span><span className="cap y">✓ serve state</span></div>
            </div>
          </div>
          <div className="chart-note" style={{ marginTop: 16 }}>
            <b style={{ color: 'var(--text)' }}>SIGIL's choice: full node, full stop.</b> A pruned node <i>can</i> do everything — but only by depending on someone else keeping the data + trusting an aggregate proof.
            Full nodes carry their own memory and re-verify from genesis, beholden to no one. At {human(pb.fluxdb_full).s}/block that's ~1.21 TB per century at a 5s cadence — a single disk. Self-sufficiency is cheap; we buy it.
            <br />But the decision is <b style={{ color: 'var(--accent-bright)' }}>agile</b>, not welded: the pruning machinery stays in the code behind one switch (<code style={{ fontFamily: "'JetBrains Mono',monospace", color: 'var(--accent-bright)' }}>SIGIL_RETENTION=full|history|pruned</code>, default full, unknown values never silently prune). Change your mind later — flip the env, no rebuild.
          </div>
        </Reveal>

        <div className="foot">
          <b>Method.</b> <code>chronos_sim</code> drives <b>{data.blocks_produced.toLocaleString()}</b> real deterministic blocks from
          <code>sigil_chronos::SigilSimNode</code> (genesis applied, roots through the real state chokepoint, one transfer + coinbase per block)
          into the <code>sigil_node::store::BlockStore</code> (flux-db, bincode, LZ4). Phase-0 zeroed crypto placeholders are replaced with
          high-entropy bytes so signatures don't compress unrealistically. Disk numbers are real <code>du</code> after compaction, including LSM overhead.
          <br />Round-trip verified · <code>store::tests::put_get_roundtrip_and_tip ✓</code> · generated {new Date(data.generated_at * 1000).toISOString().slice(0, 16).replace('T', ' ')} UTC.
        </div>
      </div>
    </>
  )
}
