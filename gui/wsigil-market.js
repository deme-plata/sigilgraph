/* ══════════════════════════════════════════════════════════════════════════
   wsigil-market.js — the real wSIGIL/USDC market for the wallet's Swap tab.

   WHY THIS IS A SEPARATE FILE (read before folding it into the wallet):
   sigil-wallet-tron-embedded.html is a 500 KB file that several agents edit
   concurrently. Keeping this here means the wallet needs exactly two hunks
   (a <script src> and the ticker fallback), so a concurrent rewrite of that
   file costs two lines to re-apply instead of the whole feature.

   WHAT IT REPLACES: swapRenderTicker() used to fall back to six hardcoded
   symbols — SIGIL/USDT, SIGIL/USDS, USDS/wQUG, CLAI/USDS, PACI/USDS,
   SCAL/USDS — rendered with a "listed" badge. None of those markets exist.
   sigil-g1 has no native pools at all (/v1/pools returns []), so the native
   AMM section of that modal has nothing to trade. The ONE real market this
   wallet can reach today is wSIGIL/USDC on Uniswap V2 (Polygon), and it is
   what this module shows and executes.

   HONESTY RULES BAKED IN:
     · Every number comes from the pair's own getReserves() — no cached
       constants, no seeded price. If the read fails the UI says so.
     · The quote uses the real UniswapV2Library.getAmountOut (997/1000) and
       TRUNCATES to the output token's decimals, so the impact figure is what
       the chain would actually pay, not an idealised one.
     · This pool is ~$0.002 deep. Price impact is shown as a length on the
       constant-product curve because at this depth it is the dominant term.
   ══════════════════════════════════════════════════════════════════════════ */
(function () {
  'use strict';

  var PAIR   = '0x06895e77a192ce525c7bb5f25c52d2ce19c052d1';
  var WSIG   = '0xc224602c32f5c7f68d3ef002ae4c99e4c7df25b7';
  var USDC   = '0x3c499c542cef5e3811e1192ce70d8cc03d5c3359';
  var ROUTER = '0xedf6066a2b290c185783862c7f4776a2c8077ad1';   // canonical Uniswap V2 on Polygon
  var CHAIN  = '0x89';
  var RPCS   = ['https://polygon-bor-rpc.publicnode.com', 'https://polygon-rpc.com', 'https://1rpc.io/matic'];

  var D_WSIG = 18, D_USDC = 6;
  var SEL_SUPPLY   = '0x18160ddd';
  var SEL_RESERVES = '0x0902f1ac', SEL_BALOF = '0x70a08231',
      SEL_APPROVE  = '0x095ea7b3', SEL_SWAP  = '0x38ed1739';

  var S = { rW: 0n, rU: 0n, price: 0, ok: false, err: '', acct: null,
            balW: 0n, balU: 0n, v: 0, cap: 0.5, busy: false, supply: null, tried: false };

  /* ── plumbing ─────────────────────────────────────────────────────────── */
  function pad32(x) {
    var h = (typeof x === 'string') ? x.replace(/^0x/, '').toLowerCase() : BigInt(x).toString(16);
    while (h.length < 64) h = '0' + h;
    return h;
  }
  function mmProvider() {
    try {
      if (window.SigilMM && window.SigilMM.getState && window.SigilMM.getState().connected) {
        var st = window.SigilMM.getState();
        if (st && st.account) S.acct = st.account;
      }
    } catch (e) {}
    return window.ethereum || null;
  }
  async function rawCall(to, data) {
    // try the injected wallet first (no CORS, no rate limit), then public RPCs
    var p = window.ethereum;
    if (p) { try { return await p.request({ method: 'eth_call', params: [{ to: to, data: data }, 'latest'] }); } catch (e) {} }
    var last;
    for (var i = 0; i < RPCS.length; i++) {
      try {
        var r = await fetch(RPCS[i], { method: 'POST', headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_call', params: [{ to: to, data: data }, 'latest'] }) });
        var j = await r.json();
        if (j && j.result) return j.result;
        last = (j && j.error && j.error.message) || 'empty result';
      } catch (e) { last = e && e.message; }
    }
    throw new Error(last || 'no Polygon RPC reachable');
  }
  var num = function (v, d) { return Number(v) / Math.pow(10, d); };
  function toUnits(x, d) {
    if (!isFinite(x) || x <= 0) return 0n;
    var s = x.toFixed(d).split('.');
    return BigInt(s[0] + (s[1] || '').padEnd(d, '0').slice(0, d));
  }
  function usd(n) { return '$' + (n < 0.01 ? n.toFixed(6) : n.toFixed(4)); }
  // exactly UniswapV2Library.getAmountOut — 0.30% fee
  function amountOut(inAmt, rIn, rOut) {
    if (inAmt <= 0n || rIn <= 0n || rOut <= 0n) return 0n;
    var wf = inAmt * 997n;
    return (wf * rOut) / (rIn * 1000n + wf);
  }

  /* ── live reads ───────────────────────────────────────────────────────── */
  async function refresh() {
    try {
      var r = await rawCall(PAIR, SEL_RESERVES);
      var b = r.replace(/^0x/, '');
      // token0 is USDC, token1 is wSIGIL — Uniswap sorts by ADDRESS, not by
      // name. Verified on chain; do not "fix" this ordering by intuition.
      S.rU = BigInt('0x' + b.slice(0, 64));
      S.rW = BigInt('0x' + b.slice(64, 128));
      S.price = num(S.rU, D_USDC) / num(S.rW, D_WSIG);
      S.ok = S.rW > 0n && S.rU > 0n; S.err = '';
    } catch (e) { S.ok = false; S.err = (e && e.message) || 'pool read failed'; }
    // Circulating wrapped supply. This is the number the dashboard card used to
    // ASSERT as "nothing has been locked/minted yet" in static copy — which went
    // stale the moment the first lock settled and then sat there lying. Read it.
    try { S.supply = BigInt(await rawCall(WSIG, SEL_SUPPLY) || '0x0'); }
    catch (e) { S.supply = null; }
    if (S.acct) {
      try {
        S.balW = BigInt(await rawCall(WSIG, SEL_BALOF + pad32(S.acct)) || '0x0');
        S.balU = BigInt(await rawCall(USDC, SEL_BALOF + pad32(S.acct)) || '0x0');
      } catch (e) {}
    }
    S.tried = true;
    paint();
    paintBridgeCards();
    if (window.swapRenderTicker) { try { window.swapRenderTicker(); } catch (e) {} }
  }

  /* ── the ticker entry the wallet asks us for ──────────────────────────── */
  // wSIGIL/USDC genuinely IS a listed Uniswap V2 market, so "listed" before
  // the reserves come back and "live" after are both true statements.
  window.SigilMarket = {
    tickerPairs: function () { return [{ pair: 'wSIGIL/USDC', fee: 30, live: S.ok }]; },
    refresh: refresh,
    state: function () { return { price: S.price, ok: S.ok, err: S.err,
                                  wsigil: S.rW.toString(), usdc: S.rU.toString(),
                                  supply: S.supply === null ? null : S.supply.toString() }; },
    // Paint the dashboard bridge card(s) on demand — the card is outside the
    // swap modal, so it needs its own entry point.
    paintBridgeCards: function () { paintBridgeCards(); }
  };

  /* ── styles, in the wallet's own idiom (Orbitron / JetBrains Mono / cyan) ─ */
  var CSS = [
    '.wm-wrap{border:1px solid rgba(33,212,253,.28);border-radius:13px;background:linear-gradient(165deg,rgba(8,20,30,.9),rgba(4,8,12,.85));padding:12px 13px;display:flex;flex-direction:column;gap:10px}',
    '.wm-hd{display:flex;align-items:center;gap:8px}',
    '.wm-ttl{font-family:Orbitron,sans-serif;font-weight:700;letter-spacing:1px;color:#6df3ff;font-size:11.5px}',
    '.wm-sub{font-family:"JetBrains Mono",monospace;font-size:9.5px;color:#5a93a8}',
    '.wm-px{margin-left:auto;text-align:right}',
    '.wm-px b{display:block;font-family:Orbitron,sans-serif;font-size:15px;color:#00e0c6;line-height:1.1}',
    '.wm-px i{font-family:"JetBrains Mono",monospace;font-size:9px;color:#5a93a8;font-style:normal}',
    '.wm-curve{border:1px solid rgba(33,212,253,.14);border-radius:10px;background:rgba(2,6,10,.6);padding:6px 8px 2px}',
    '.wm-curve svg{display:block;width:100%;height:auto}',
    '.wm-leg{display:flex;gap:12px;font-family:"JetBrains Mono",monospace;font-size:8.5px;color:#4f7f92;padding:3px 1px 1px}',
    '.wm-leg s{width:7px;height:7px;border-radius:50%;display:inline-block;margin-right:4px;text-decoration:none}',
    '.wm-ends{display:flex;justify-content:space-between;font-family:"JetBrains Mono",monospace;font-size:9.5px;letter-spacing:.5px;color:#5a93a8;text-transform:uppercase}',
    '.wm-ends b{display:block;font-family:Rajdhani,sans-serif;font-size:11.5px;letter-spacing:0;text-transform:none;color:#8fb3c2;font-weight:600;margin-top:1px}',
    '.wm-ends .hot{color:#6df3ff}.wm-ends .hot b{color:#cfe9f2}',
    '.wm-slot{position:relative;height:34px}',
    '.wm-base{position:absolute;top:14px;left:0;right:0;height:5px;border-radius:99px;background:linear-gradient(90deg,rgba(139,92,246,.45),rgba(33,212,253,.10) 45%,rgba(33,212,253,.10) 55%,rgba(39,117,202,.5));opacity:.55}',
    '.wm-fill{position:absolute;top:14px;height:5px;border-radius:99px;transition:left .09s,width .09s}',
    '.wm-zero{position:absolute;top:8px;left:50%;width:2px;height:17px;margin-left:-1px;background:rgba(255,255,255,.26);border-radius:2px}',
    '.wm-knob{position:absolute;top:5px;width:23px;height:23px;margin-left:-11.5px;border-radius:50%;background:#06121c;border:2px solid #21d4fd;box-shadow:0 0 9px rgba(33,212,253,.6);pointer-events:none;transition:left .09s;z-index:3}',
    '.wm-slot input{position:absolute;top:5px;left:0;width:100%;height:23px;opacity:0;cursor:grab;z-index:4;margin:0;-webkit-appearance:none;appearance:none;background:none}',
    '.wm-slot input::-webkit-slider-thumb{-webkit-appearance:none;width:26px;height:26px}',
    '.wm-amt{font-family:Orbitron,sans-serif;font-size:23px;font-weight:700;letter-spacing:-.5px;line-height:1.1;color:#cfe9f2;transition:color .2s}',
    '.wm-amt.idle{color:#3d5a6b}',
    '.wm-amt u{font-family:"JetBrains Mono",monospace;font-size:9.5px;color:#5a93a8;margin-left:7px;text-decoration:none;letter-spacing:0}',
    '.wm-rows{display:flex;flex-direction:column}',
    '.wm-row{display:flex;justify-content:space-between;gap:10px;padding:6px 0;border-bottom:1px solid rgba(33,212,253,.07);font-family:"JetBrains Mono",monospace;font-size:10.5px}',
    '.wm-row:last-child{border-bottom:0}.wm-row span{color:#5a93a8}.wm-row b{color:#cfe9f2;text-align:right}',
    '.wm-row b.g{color:#00e0c6}.wm-row b.w{color:#fbbf24}.wm-row b.r{color:#ff6b6b}',
    '.wm-meter{height:4px;border-radius:99px;background:rgba(255,255,255,.08);overflow:hidden}',
    '.wm-meter i{display:block;height:100%;width:0;border-radius:99px;transition:width .18s,background .18s}',
    '.wm-go{border:1px solid rgba(33,212,253,.45);background:linear-gradient(90deg,rgba(33,212,253,.2),rgba(0,224,198,.13));color:#6df3ff;font-family:Orbitron,sans-serif;font-weight:700;font-size:11.5px;letter-spacing:1px;padding:11px;border-radius:10px;cursor:pointer;width:100%}',
    '.wm-go:disabled{opacity:.35;cursor:not-allowed}',
    '.wm-note{font-family:"JetBrains Mono",monospace;font-size:9.5px;color:#4f7f92;line-height:1.6}',
    '.wm-note b{color:#8fb3c2}.wm-note em{color:#fbbf24;font-style:normal}',
    '.wm-msg{font-family:"JetBrains Mono",monospace;font-size:10px;min-height:12px;color:#8fb3c2;word-break:break-all}',
    '.wm-native{font-family:"JetBrains Mono",monospace;font-size:9.5px;color:#fbbf24;background:rgba(251,191,36,.07);border:1px solid rgba(251,191,36,.22);border-radius:9px;padding:8px 10px;line-height:1.6}'
  ].join('');

  var HTML =
    '<div class="wm-wrap" id="wmWrap">' +
      '<div class="wm-hd"><div style="font-size:15px">◈</div>' +
        '<div><div class="wm-ttl">POLYGON MARKET</div>' +
        '<div class="wm-sub">wSIGIL / USDC · Uniswap V2 · 0.30% fee</div></div>' +
        '<div class="wm-px"><b id="wmPx">—</b><i id="wmDepth">reading the pool…</i></div></div>' +
      '<div class="wm-curve"><svg id="wmCurve" viewBox="0 0 424 132"></svg>' +
        '<div class="wm-leg"><span><s style="background:#5a93a8"></s>x · y = k</span>' +
        '<span><s style="background:#00e0c6"></s>pool now</span>' +
        '<span><s style="background:#fbbf24"></s>after your trade</span></div></div>' +
      '<div class="wm-ends"><div id="wmEndL">◄ sell<b>wSIGIL → USDC</b></div>' +
        '<div id="wmEndR" style="text-align:right">buy ►<b>USDC → wSIGIL</b></div></div>' +
      '<div class="wm-slot"><div class="wm-base"></div><div class="wm-fill" id="wmFill"></div>' +
        '<div class="wm-zero"></div><div class="wm-knob" id="wmKnob" style="left:50%"></div>' +
        '<input type="range" id="wmRange" min="-100" max="100" step="1" value="0" aria-label="sell or buy wSIGIL"></div>' +
      '<div style="display:flex;align-items:center;gap:10px">' +
        '<div class="wm-amt idle" id="wmAmt">0<u>idle — slide either way</u></div></div>' +
      '<div class="wm-rows" id="wmRows"></div>' +
      '<div class="wm-meter"><i id="wmImpact"></i></div>' +
      '<button class="wm-go" id="wmGo" disabled>MOVE THE SLIDER</button>' +
      '<div class="wm-msg" id="wmMsg"></div>' +
      '<div class="wm-note" id="wmNote"></div>' +
    '</div>';

  /* ── the constant-product curve ───────────────────────────────────────────
     Axes are the two reserves. The pool can only ever sit ON this hyperbola,
     so a trade is a SLIDE along it — which is what price impact physically is.
     At $0.002 depth the slide is huge, and that is the honest picture.        */
  var CV = { w: 424, h: 132, l: 34, r: 8, t: 10, b: 20 };
  function drawCurve(rW2, rU2, active) {
    var svg = document.getElementById('wmCurve'); if (!svg) return;
    if (!S.ok) { svg.innerHTML = '<text x="12" y="70" style="font:10px monospace;fill:#5a93a8">pool unreadable — ' + esc(S.err || '') + '</text>'; return; }
    var x1 = num(S.rW, D_WSIG), y1 = num(S.rU, D_USDC), k = x1 * y1;
    var x2 = active ? num(rW2, D_WSIG) : x1, y2 = active ? num(rU2, D_USDC) : y1;
    var xlo = Math.max(k / 1e12, Math.min(x1, x2) * 0.55), xhi = Math.max(x1, x2) * 1.85;
    var ylo = k / xhi, yhi = k / xlo;
    var PX = function (x) { return CV.l + (x - xlo) / (xhi - xlo) * (CV.w - CV.l - CV.r); };
    var PY = function (y) { return CV.h - CV.b - (y - ylo) / (yhi - ylo) * (CV.h - CV.t - CV.b); };
    var pts = [], i, x;
    for (i = 0; i <= 120; i++) { x = xlo + (xhi - xlo) * i / 120; pts.push(PX(x).toFixed(1) + ',' + PY(k / x).toFixed(1)); }
    var seg = '';
    if (active && x2 !== x1) {
      var a = Math.min(x1, x2), bb = Math.max(x1, x2), sp = [];
      for (i = 0; i <= 36; i++) { x = a + (bb - a) * i / 36; sp.push(PX(x).toFixed(1) + ',' + PY(k / x).toFixed(1)); }
      seg = '<polyline points="' + sp.join(' ') + '" fill="none" stroke="#fbbf24" stroke-width="3" stroke-linecap="round"/>';
    }
    var dot = function (x, y, c, r) {
      return '<circle cx="' + PX(x).toFixed(1) + '" cy="' + PY(y).toFixed(1) + '" r="' + r + '" fill="' + c + '" stroke="#04080c" stroke-width="1.6"/>';
    };
    svg.innerHTML =
      '<line x1="' + CV.l + '" y1="' + (CV.h - CV.b) + '" x2="' + (CV.w - CV.r) + '" y2="' + (CV.h - CV.b) + '" stroke="rgba(33,212,253,.16)"/>' +
      '<line x1="' + CV.l + '" y1="' + CV.t + '" x2="' + CV.l + '" y2="' + (CV.h - CV.b) + '" stroke="rgba(33,212,253,.16)"/>' +
      '<polyline points="' + pts.join(' ') + '" fill="none" stroke="rgba(143,179,194,.5)" stroke-width="1.4"/>' + seg +
      (active ? dot(x2, y2, '#fbbf24', 5) : '') + dot(x1, y1, '#00e0c6', 4.5) +
      '<text x="2" y="' + (CV.t + 7) + '" style="font:8.5px monospace;fill:#4f7f92">USDC</text>' +
      '<text x="' + CV.l + '" y="' + (CV.h - 6) + '" style="font:8.5px monospace;fill:#4f7f92">wSIGIL reserve →</text>';
  }
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]; }); }

  /* ── paint ────────────────────────────────────────────────────────────── */
  var EASE = 2.4;                       // fine control near the middle
  function amountOfSlider() { return S.cap * Math.pow(Math.abs(S.v) / 100, EASE); }
  function row(k, v, c) { return '<div class="wm-row"><span>' + k + '</span><b class="' + (c || '') + '">' + v + '</b></div>'; }

  function paint() {
    var $ = function (id) { return document.getElementById(id); };
    if (!$('wmWrap')) return;
    $('wmPx').textContent = S.ok ? usd(S.price) : '—';
    $('wmDepth').textContent = S.ok
      ? (num(S.rU, D_USDC) * 2).toFixed(6).replace(/^/, '$') + ' pool depth'
      : (S.err || 'pool unreadable');

    var buying = S.v > 0, amt = amountOfSlider(), pct = 50 + S.v / 2;
    $('wmKnob').style.left = pct + '%';
    var f = $('wmFill');
    if (S.v === 0) { f.style.width = '0'; }
    else if (S.v < 0) { f.style.left = pct + '%'; f.style.width = (50 - pct) + '%'; f.style.background = 'linear-gradient(270deg,#8b5cf6,rgba(139,92,246,.2))'; }
    else { f.style.left = '50%'; f.style.width = (pct - 50) + '%'; f.style.background = 'linear-gradient(90deg,rgba(39,117,202,.2),#2775ca)'; }
    $('wmEndL').className = S.v < 0 ? 'hot' : '';
    $('wmEndR').className = S.v > 0 ? 'hot' : '';
    $('wmEndR').style.textAlign = 'right';

    var el = $('wmAmt');
    el.className = 'wm-amt' + (S.v === 0 ? ' idle' : '');
    el.innerHTML = (S.v === 0 ? '0' : amt.toLocaleString(undefined, { maximumFractionDigits: 8 })) +
      '<u>' + (S.v === 0 ? 'idle — sell left, buy right' : (buying ? 'USDC to spend' : 'wSIGIL to sell')) + '</u>';
    el.style.color = S.v === 0 ? '' : (buying ? '#6df3ff' : '#b794f6');

    var rows = [], out = 0n, rW2 = S.rW, rU2 = S.rU, inU = 0n;
    if (S.v !== 0 && S.ok) {
      if (buying) { inU = toUnits(amt, D_USDC); out = amountOut(inU, S.rU, S.rW); rU2 = S.rU + inU; rW2 = S.rW - out; }
      else        { inU = toUnits(amt, D_WSIG); out = amountOut(inU, S.rW, S.rU); rW2 = S.rW + inU; rU2 = S.rU - out; }
    }
    if (S.v === 0 || !S.ok) {
      rows.push(row('Spot price', S.ok ? usd(S.price) + ' / SIGIL' : '—'));
      rows.push(row('Pool holds', S.ok ? (num(S.rW, D_WSIG).toFixed(4) + ' wSIGIL · ' + num(S.rU, D_USDC).toFixed(6) + ' USDC') : '—'));
      $('wmImpact').style.width = '0';
    } else {
      var inH  = buying ? num(inU, D_USDC) : num(inU, D_WSIG);
      var outH = buying ? num(out, D_WSIG) : num(out, D_USDC);
      var ideal = buying ? (S.price ? inH / S.price : 0) : inH * S.price;
      var imp = ideal > 0 ? Math.max(0, (1 - outH / ideal) * 100) : 0;
      var newP = num(rU2, D_USDC) / num(rW2, D_WSIG);
      var cls = imp > 25 ? 'r' : imp > 5 ? 'w' : 'g';
      rows.push(row(buying ? 'You spend' : 'You sell', buying ? amt.toFixed(6) + ' USDC' : amt.toFixed(8) + ' wSIGIL'));
      rows.push(row('You receive', buying ? outH.toFixed(8) + ' wSIGIL' : outH.toFixed(6) + ' USDC', 'g'));
      rows.push(row('Effective price', outH > 0 ? usd(buying ? inH / outH : outH / inH) + ' / SIGIL' : '—'));
      rows.push(row('Price impact', imp.toFixed(2) + '%', cls));
      rows.push(row('Pool price after', usd(newP) + ' (' + (newP >= S.price ? '+' : '') + ((newP / S.price - 1) * 100).toFixed(1) + '%)', cls));
      if (S.acct) {
        var have = buying ? num(S.balU, D_USDC) : num(S.balW, D_WSIG);
        rows.push(row(buying ? 'Your USDC' : 'Your wSIGIL', buying ? have.toFixed(6) : have.toFixed(8), amt > have ? 'r' : ''));
      }
      var m = $('wmImpact');
      m.style.width = Math.min(100, imp).toFixed(1) + '%';
      m.style.background = imp > 25 ? '#ff6b6b' : imp > 5 ? '#fbbf24' : '#00e0c6';
    }
    $('wmRows').innerHTML = rows.join('');
    drawCurve(rW2, rU2, S.v !== 0 && S.ok);

    var b = $('wmGo');
    if (S.busy) { b.disabled = true; }
    else if (!S.ok) { b.disabled = true; b.textContent = 'POOL UNREADABLE'; }
    else if (S.v === 0) { b.disabled = true; b.textContent = 'MOVE THE SLIDER'; }
    else if (!S.acct) { b.disabled = false; b.textContent = 'CONNECT METAMASK'; }
    else {
      var have2 = buying ? num(S.balU, D_USDC) : num(S.balW, D_WSIG);
      b.disabled = amt > have2 || out === 0n;
      b.textContent = amt > have2 ? ('NOT ENOUGH ' + (buying ? 'USDC' : 'wSIGIL'))
                    : (buying ? '⇄ BUY wSIGIL' : '⇄ SELL wSIGIL');
    }
    $('wmNote').innerHTML =
      'Two transactions: an ERC-20 <b>approve</b> to the router, then <b>swapExactTokensForTokens</b>. ' +
      'To get wSIGIL in the first place use <b>Bridge</b> — this pool cannot mint it. ' +
      (S.ok ? 'The pool is only <em>' + usd(num(S.rU, D_USDC) * 2) + '</em> deep in total, so a large trade moves the price rather than paying it.' : '');
  }

  /* ── execution (MetaMask only — no SIGIL key is ever touched here) ─────── */
  function msg(t, c) { var e = document.getElementById('wmMsg'); if (e) { e.innerHTML = t; e.style.color = c || '#8fb3c2'; } }

  async function connect() {
    if (window.SigilMM && window.SigilMM.connect) {
      await window.SigilMM.connect();
      if (window.SigilMM.ensurePolygon) await window.SigilMM.ensurePolygon();
      var st = window.SigilMM.getState();
      S.acct = st && st.account;
    } else {
      var p = mmProvider();
      if (!p) throw new Error('No browser wallet detected. Install MetaMask, then reload.');
      var a = await p.request({ method: 'eth_requestAccounts' });
      S.acct = a && a[0];
      var cid = await p.request({ method: 'eth_chainId' });
      if (cid !== CHAIN) await p.request({ method: 'wallet_switchEthereumChain', params: [{ chainId: CHAIN }] });
    }
    if (!S.acct) throw new Error('no account returned by the wallet');
    msg('connected ' + S.acct.slice(0, 6) + '…' + S.acct.slice(-4), '#00e0c6');
    await refresh();
  }

  async function sendTx(to, data) {
    var p = mmProvider();
    if (!p) throw new Error('no wallet provider');
    var h = await p.request({ method: 'eth_sendTransaction', params: [{ from: S.acct, to: to, data: data }] });
    msg('sent <a href="https://polygonscan.com/tx/' + h + '" target="_blank" rel="noreferrer" style="color:#6df3ff">' + h.slice(0, 14) + '…</a>', '#fbbf24');
    for (;;) {
      var r = null;
      try { r = await p.request({ method: 'eth_getTransactionReceipt', params: [h] }); } catch (e) {}
      if (r) {
        if (r.status !== '0x1') throw new Error('transaction reverted on chain');
        return r;
      }
      await new Promise(function (s) { setTimeout(s, 2500); });
    }
  }

  async function go() {
    if (!S.acct) { return connect(); }
    var buying = S.v > 0, amt = amountOfSlider();
    var tokenIn  = buying ? USDC : WSIG;
    var tokenOut = buying ? WSIG : USDC;
    var units = toUnits(amt, buying ? D_USDC : D_WSIG);
    if (units === 0n) { msg('amount rounds to zero', '#fbbf24'); return; }
    S.busy = true; paint();
    try {
      msg('approving ' + (buying ? 'USDC' : 'wSIGIL') + ' to the router…', '#6df3ff');
      await sendTx(tokenIn, SEL_APPROVE + pad32(ROUTER) + pad32(units));
      var dl = BigInt(Math.floor(Date.now() / 1000) + 900);
      msg(buying ? 'buying wSIGIL…' : 'selling wSIGIL…', '#6df3ff');
      await sendTx(ROUTER, SEL_SWAP + pad32(units) + pad32(0n) + pad32(160n) + pad32(S.acct) +
                           pad32(dl) + pad32(2n) + pad32(tokenIn) + pad32(tokenOut));
      msg('swap complete — re-reading the pool', '#00e0c6');
      S.v = 0; var rg = document.getElementById('wmRange'); if (rg) rg.value = 0;
    } catch (e) {
      msg('✗ ' + ((e && e.message) || String(e)), '#ff6b6b');
    }
    S.busy = false;
    await refresh();
  }

  /* ── dashboard bridge card ────────────────────────────────────────────── */
  /* The Polygon Bridge card on the dashboard used to carry a hardcoded
     sentence claiming nothing had ever been minted. That claim was false by
     2026-08-27 (supply 1.0 wSIGIL, and the USDC/wSIGIL pair had already
     traded), and it stayed on screen because a static string cannot notice
     the chain moving underneath it. Everything below is READ, never asserted:
     if Polygon is unreachable the card says so rather than inventing a state. */
  function fmtAmt(v, d, max) {
    if (v === null || v === undefined) return null;
    var n = Number(v) / Math.pow(10, d);
    return n.toLocaleString(undefined, { maximumFractionDigits: max === undefined ? 6 : max });
  }
  function bridgeMarkup() {
    // "not measured yet" and "measured, and it failed" are different states and
    // must not render the same. Painting the failure text before the first read
    // has even returned is the same mistake as the hardcoded copy this replaces.
    if (!S.tried) return '<span style="color:#5a93a8">reading Polygon…</span>';
    if (!S.ok && S.supply === null) {
      return '<span style="color:#5a93a8">could not reach Polygon — pool and supply unknown right now' +
             (S.err ? ' (' + String(S.err).replace(/[<>&]/g, '') + ')' : '') + '</span>';
    }
    var out = [];
    if (S.supply !== null) {
      out.push('<b style="color:#00e0c6">' + fmtAmt(S.supply, D_WSIG, 4) + '</b> wSIGIL minted');
    }
    if (S.ok) {
      out.push('pool <b style="color:#6df3ff">' + fmtAmt(S.rW, D_WSIG, 4) + '</b> wSIGIL / <b style="color:#6df3ff">' +
               fmtAmt(S.rU, D_USDC, 6) + '</b> USDC');
      out.push('<b style="color:#00e0c6">' + S.price.toLocaleString(undefined, { maximumFractionDigits: 8 }) +
               '</b> USDC per wSIGIL');
    } else {
      out.push('<span style="color:#5a93a8">pool unreadable</span>');
    }
    return out.join(' · ') +
      ' · <a href="https://polygonscan.com/address/' + PAIR + '" target="_blank" rel="noreferrer" ' +
      'style="color:#6df3ff;text-decoration:none;border-bottom:1px dotted rgba(109,243,255,.5)">↗ Uniswap V2 pair</a>';
  }
  function paintBridgeCards() {
    var m = bridgeMarkup();
    ['polyMktDesk', 'polyMktPhone'].forEach(function (id) {
      var el = document.getElementById(id);
      if (el) el.innerHTML = m;
    });
  }
  function bridgeCardVisible() {
    return ['polyMktDesk', 'polyMktPhone'].some(function (id) {
      var el = document.getElementById(id);
      return el && el.offsetParent !== null;
    });
  }

  /* ── mount ────────────────────────────────────────────────────────────── */
  function mount() {
    if (document.getElementById('wmWrap')) return true;
    var form = document.getElementById('swapForm');
    if (!form) return false;
    var st = document.createElement('style'); st.textContent = CSS; document.head.appendChild(st);
    var host = document.createElement('div'); host.innerHTML = HTML;
    var station = host.firstChild;                 // insertBefore MOVES it, so grab it first
    form.insertBefore(station, form.firstChild);

    // Be honest about the section underneath: sigil-g1 has no native pools.
    // Anchor off `station`, not host.firstChild — host is empty by now, and
    // reading it back gives null, which appends the note to the very bottom
    // of the form where nobody sees it.
    if (!(window.__swapPools || []).length) {
      var n = document.createElement('div');
      n.className = 'wm-native';
      n.innerHTML = 'Below: the <b>native</b> sigil-g1 AMM. It lists no pools yet — <code>/v1/pools</code> returns an empty set — so there is nothing to trade there today. The Polygon market above is the live one.';
      form.insertBefore(n, station.nextSibling);
    }

    var rg = document.getElementById('wmRange');
    rg.addEventListener('input', function (e) {
      var v = Number(e.target.value) || 0;
      S.v = Math.abs(v) < 4 ? 0 : v;          // detent at idle so 0 is reachable
      paint();
    });
    rg.addEventListener('dblclick', function (e) { e.target.value = 0; S.v = 0; paint(); });
    document.getElementById('wmGo').addEventListener('click', function () {
      go().catch(function (e) { msg('✗ ' + ((e && e.message) || String(e)), '#ff6b6b'); });
    });

    // adopt an already-connected MetaMask session from the wallet's own chip
    try {
      if (window.SigilMM) {
        if (window.SigilMM.resume) window.SigilMM.resume().then(function () {
          var s = window.SigilMM.getState(); if (s && s.account) { S.acct = s.account; refresh(); }
        }).catch(function () {});
        if (window.SigilMM.on) window.SigilMM.on('change', function (s) {
          S.acct = (s && s.account) || null; refresh();
        });
      }
    } catch (e) {}
    paint();
    return true;
  }

  function boot() {
    if (!mount()) { setTimeout(boot, 400); return; }
    refresh();
    paintBridgeCards();
    setInterval(function () {
      var m = document.getElementById('swap-modal');
      var modalOpen = m && getComputedStyle(m).display !== 'none';
      // Refresh while EITHER surface is on screen. The bridge card lives on the
      // dashboard, outside the modal, so gating purely on the modal left it
      // showing whatever it had at boot.
      if (modalOpen || bridgeCardVisible()) refresh();
    }, 15000);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
})();
