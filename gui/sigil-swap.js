/* SIGIL exchange-v2 — wSIGIL / USDC on Uniswap V2, Polygon Amoy (test) and Polygon PoS.
 *
 * What changed from exchange-v1/sigil-exchange.js (2026-10-15):
 *   - The pair is no longer hard-coded. It comes from sigil-swap-networks.json beside this file
 *     (or window.SIGIL_SWAP_NETWORKS), one entry per network, so the new Amoy pair plugs in by
 *     editing data, not code.
 *   - Before ANY signature the page checks the route on chain, in the visitor's own browser:
 *     contracts exist, router.factory() is the configured factory, factory.getPair(USDC, wSIGIL)
 *     is the configured pair, the pair's two tokens are exactly those two, decimals match. On
 *     Polygon PoS the factory and router must also be the canonical Uniswap V2 deployment
 *     (baked in below, NOT read from the config: a dead clone at Ethereum's addresses passes a
 *     self-consistency check, 2026-08-27).
 *   - Review happens inside the dialog (no dependency on the wallet page's sigilConfirm), so the
 *     same module runs in the wallet modal and on a public page (#sigil-swap-inline).
 *   - The receipt reads the ACTUAL amount received from the swap's Transfer log.
 * Unchanged on purpose: exact-amount approval (never unlimited), 30 s quote expiry, the reviewed
 * minimum is never lowered after approval, 15 % price-impact refusal, the bridge-modal wrapper.
 * No keys, no backend, no dependencies. Reads go through the connected wallet when it is on the
 * right chain, otherwise through the network's public read RPCs.
 */
(function () {
  'use strict';
  var MM = window.SigilMM;
  if (!MM || typeof MM.request !== 'function' || typeof MM.ensureChain !== 'function') {
    if (window.console) console.warn('[sigil-swap] needs exchange-v2/sigil-metamask.js (request + ensureChain)');
    return;
  }
  if (document.getElementById('exchange-modal')) return;   // v1 already mounted: never run both

  var SCRIPT = (document.currentScript && document.currentScript.src) || '';
  var BASE = SCRIPT ? SCRIPT.replace(/[?#].*$/, '').replace(/[^/]*$/, '') : '/exchange-v2/';
  var VER = (SCRIPT.match(/[?&]v=([^&#]+)/) || [])[1] || '';

  // Canonical Uniswap V2 on Polygon PoS (verified 2026-08-27: canonical factory 43,953 pairs vs
  // 274 on the dead clone). Lower-case. A network listed here REFUSES any other router/factory.
  var CANON = {
    '0x89': { router: '0xedf6066a2b290c185783862c7f4776a2c8077ad1', factory: '0x9e5a52f57b3038f1b8eee45f28b3c1967e22799c' }
  };
  var SEL = {
    factory: '0xc45a0155', getPair: '0xe6a43905', token0: '0x0dfe1681', token1: '0xd21220a7',
    reserves: '0x0902f1ac', decimals: '0x313ce567', symbol: '0x95d89b41', balanceOf: '0x70a08231',
    allowance: '0xdd62ed3e', approve: '0x095ea7b3', amountsOut: '0xd06ca61f', swapExact: '0x38ed1739'
  };
  var TRANSFER = '0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef';
  var IMPACT_MAX = 15, QUOTE_TTL = 30000;

  var S = {
    nets: [], net: null, sell: false, busy: false, executing: false, quote: null, version: 0,
    route: null, routeFor: null, bal: null, native: null, reviewing: false, returnFocus: null, inline: false
  };
  var $ = function (id) { return document.getElementById(id); };
  function el(tag, cls, text) { var e = document.createElement(tag); e.className = cls || ''; if (text) e.textContent = text; return e; }
  function esc(s) { return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]; }); }
  function lc(a) { return String(a || '').toLowerCase(); }
  function isAddr(a) { return /^0x[0-9a-fA-F]{40}$/.test(String(a || '')); }
  function pad(h) { return String(h).replace(/^0x/, '').padStart(64, '0'); }
  function n32(v) { return pad(BigInt(v).toString(16)); }
  function a32(a) { return pad(lc(a)); }
  function word(hex, i) { return String(hex || '').replace(/^0x/, '').slice(i * 64, i * 64 + 64); }
  function wordAddr(hex, i) { var w = word(hex, i); return w.length === 64 ? '0x' + w.slice(24) : ''; }
  function wordInt(hex, i) { var w = word(hex, i); return w ? BigInt('0x' + w) : 0n; }
  function abiString(hex) {
    var d = String(hex || '').replace(/^0x/, '');
    try {
      if (d.length >= 128 && parseInt(d.slice(0, 64), 16) === 32) {
        var n = parseInt(d.slice(64, 128), 16), b = d.slice(128, 128 + n * 2), s = '';
        for (var i = 0; i < b.length; i += 2) s += String.fromCharCode(parseInt(b.slice(i, i + 2), 16));
        return decodeURIComponent(escape(s));
      }
      var t = ''; for (var j = 0; j < 64 && j < d.length; j += 2) { var c = parseInt(d.slice(j, j + 2), 16); if (c) t += String.fromCharCode(c); }
      return t;
    } catch (_) { return ''; }
  }
  function units(s, dp) {
    s = String(s).trim().replace(',', '.');
    if (!/^\d+(\.\d+)?$/.test(s) || (s.split('.')[1] || '').length > dp) throw new Error('Use a positive amount with at most ' + dp + ' decimals.');
    var p = s.split('.'); return BigInt(p[0]) * 10n ** BigInt(dp) + BigInt((p[1] || '').padEnd(dp, '0') || '0');
  }
  function fmt(n, dp, keep) {
    n = BigInt(n); var d = 10n ** BigInt(dp), f = (n % d).toString().padStart(dp, '0').slice(0, keep == null ? dp : keep).replace(/0+$/, '');
    return (n / d).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',') + (f ? '.' + f : '');
  }
  function plain(n, dp) { return fmt(n, dp).replace(/,/g, ''); }
  function num(n, dp) { return Number(n) / Math.pow(10, dp); }
  function sig(x) { if (!isFinite(x) || x === 0) return '0'; var a = Math.abs(x); return a >= 1 ? x.toLocaleString(undefined, { maximumFractionDigits: 4 }) : x.toPrecision(4).replace(/0+$/, '').replace(/\.$/, ''); }
  function short(a) { return a ? a.slice(0, 6) + '…' + a.slice(-4) : ''; }

  // ── network config ──────────────────────────────────────────────────────
  function validNet(n) {
    if (!n || !/^[a-z0-9-]{2,24}$/.test(n.key || '') || !n.chain || !/^0x[0-9a-f]+$/.test(n.chain.chainId || '')) return false;
    if (!Array.isArray(n.readRpcs) || !n.readRpcs.length || !n.readRpcs.every(function (u) { return /^https:\/\//.test(u) || /^http:\/\/(127\.0\.0\.1|localhost)[:/]/.test(u); })) return false;
    if (!n.quote || !isAddr(n.quote.address) || !(n.quote.decimals >= 0)) return false;
    if (n.wsigil && (!isAddr(n.wsigil.address) || !(n.wsigil.decimals >= 0))) return false;
    if (n.pair && !isAddr(n.pair)) return false;
    var d = n.dex || {}; if ((d.router && !isAddr(d.router)) || (d.factory && !isAddr(d.factory))) return false;
    if (n.explorer && !/^https:\/\//.test(n.explorer)) return false;
    return true;
  }
  async function loadConfig() {
    var cfg = window.SIGIL_SWAP_NETWORKS;
    if (!cfg) {
      var bucket = VER || String(Math.floor(Date.now() / 300000));   // 5-minute cache bucket
      var r = await fetch(BASE + 'sigil-swap-networks.json?v=' + encodeURIComponent(bucket), { cache: 'no-store' });
      if (!r.ok || !/json/.test(r.headers.get('content-type') || '')) throw new Error('network list missing (HTTP ' + r.status + ')');
      cfg = await r.json();
    }
    var nets = (cfg.networks || []).filter(function (n) { var ok = validNet(n); if (!ok && window.console) console.warn('[sigil-swap] dropped invalid network entry', n && n.key); return ok; });
    if (!nets.length) throw new Error('no valid network in the list');
    var pick = null; try { pick = localStorage.getItem('sigil-swap-net'); } catch (_) {}
    S.nets = nets;
    S.net = nets.filter(function (n) { return n.key === pick; })[0] || nets.filter(function (n) { return n.key === cfg.default; })[0] || nets[0];
  }
  function onNet() { var st = MM.getState(); return !!(st.connected && S.net && st.chainId === S.net.chain.chainId); }
  function ready() { return !!(S.route && S.route.ok && S.routeFor === S.net.key); }
  function W() { return S.net.wsigil; }
  function Q() { return S.net.quote; }
  function symW() { return (S.route && S.route.symW) || (W() && W().symbol) || 'wSIGIL'; }
  function symQ() { return (S.route && S.route.symQ) || Q().symbol; }
  function path() { return S.sell ? [W().address, Q().address] : [Q().address, W().address]; }
  function syms() { return S.sell ? [symW(), symQ()] : [symQ(), symW()]; }
  function dps() { return S.sell ? [W().decimals, Q().decimals] : [Q().decimals, W().decimals]; }
  function txLink(h) { return S.net.explorer ? S.net.explorer.replace(/\/$/, '') + '/tx/' + h : ''; }
  function addrLink(a) { return S.net.explorer ? S.net.explorer.replace(/\/$/, '') + '/address/' + a : ''; }

  // ── chain reads: the wallet when it is on this chain, else the public read RPCs ──
  async function rpc(method, params, net) {
    net = net || S.net;
    var st = MM.getState();
    if (st.connected && st.chainId === net.chain.chainId) {
      try { return await MM.request(method, params); } catch (e) { if (method !== 'eth_call' && method !== 'eth_getCode' && method !== 'eth_getBalance') throw e; }
    }
    var last = null;
    for (var i = 0; i < net.readRpcs.length; i++) {
      try {
        var r = await fetch(net.readRpcs[i], { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: method, params: params }) });
        var j = await r.json();
        if (j && Object.prototype.hasOwnProperty.call(j, 'result')) return j.result;
        last = new Error((j && j.error && j.error.message) || 'RPC returned no result');
      } catch (e) { last = e; }
    }
    throw last || new Error('every ' + net.label + ' RPC failed');
  }
  function call(to, data, net) { return rpc('eth_call', [{ to: to, data: data }, 'latest'], net); }

  // ── the route check: nothing is signed until every line is ✓ ────────────
  async function verifyRoute(net) {
    var checks = [], missing = [];
    function add(label, ok, detail) { checks.push({ label: label, ok: ok, detail: detail || '' }); return ok; }
    if (!net.dex || !net.dex.router || !net.dex.factory) missing.push(net.dex && net.dex.name ? net.dex.name + ' router + factory' : 'DEX router + factory');
    if (!net.wsigil) missing.push('wSIGIL token');
    if (!net.pair) missing.push('wSIGIL / ' + net.quote.symbol + ' pair');
    if (missing.length) return { ok: false, status: 'missing', missing: missing, checks: checks };
    var R = lc(net.dex.router), F = lc(net.dex.factory), P = lc(net.pair), Wa = lc(net.wsigil.address), Qa = lc(net.quote.address);
    var canon = CANON[net.chain.chainId];
    if (canon && (R !== canon.router || F !== canon.factory)) {
      add('Canonical ' + (net.dex.name || 'Uniswap V2') + ' deployment', false, 'the list names ' + short(R) + ' / ' + short(F) + ', not the canonical router and factory');
      return { ok: false, status: 'bad', checks: checks };
    }
    if (canon) add('Canonical ' + (net.dex.name || 'Uniswap V2') + ' deployment', true, 'router ' + short(R) + ' · factory ' + short(F));
    var codes = await Promise.all([R, P, Wa, Qa].map(function (a) { return rpc('eth_getCode', [a, 'latest'], net); }));
    if (!add('Contracts exist on ' + net.label, codes.every(function (c) { return c && c !== '0x'; }), codes.map(function (c, i) { return ['router', 'pair', 'wSIGIL', net.quote.symbol][i] + (c && c !== '0x' ? ' ✓' : ' ✗ no code'); }).join(' · ')))
      return { ok: false, status: 'bad', checks: checks };
    var out = await Promise.all([
      call(R, SEL.factory, net), call(F, SEL.getPair + a32(Qa) + a32(Wa), net),
      call(P, SEL.token0, net), call(P, SEL.token1, net),
      call(Qa, SEL.decimals, net), call(Wa, SEL.decimals, net),
      call(Qa, SEL.symbol, net), call(Wa, SEL.symbol, net), call(P, SEL.reserves, net)
    ]);
    var rf = lc(wordAddr(out[0], 0)), gp = lc(wordAddr(out[1], 0)), t0 = lc(wordAddr(out[2], 0)), t1 = lc(wordAddr(out[3], 0));
    var dq = Number(wordInt(out[4], 0)), dw = Number(wordInt(out[5], 0)), sq = abiString(out[6]), sw = abiString(out[7]);
    var ok = true;
    ok = add('Router belongs to the factory', rf === F, 'router.factory() = ' + short(rf)) && ok;
    ok = add('Factory knows this pair', gp === P, 'getPair(' + (sq || net.quote.symbol) + ', ' + (sw || 'wSIGIL') + ') = ' + short(gp)) && ok;
    ok = add('Pair holds exactly these two tokens', (t0 === Qa && t1 === Wa) || (t0 === Wa && t1 === Qa), 'token0 ' + short(t0) + ' · token1 ' + short(t1)) && ok;
    ok = add('Decimals match', dq === net.quote.decimals && dw === net.wsigil.decimals, (sq || '?') + ' ' + dq + ' · ' + (sw || '?') + ' ' + dw) && ok;
    var r0 = wordInt(out[8], 0), r1 = wordInt(out[8], 1), rQ = t0 === Qa ? r0 : r1, rW = t0 === Qa ? r1 : r0;
    ok = add('Pool has liquidity on both sides', rQ > 0n && rW > 0n, fmt(rQ, dq, 6) + ' ' + (sq || 'USDC') + ' · ' + fmt(rW, dw, 6) + ' ' + (sw || 'wSIGIL')) && ok;
    return { ok: ok, status: ok ? 'ok' : 'bad', checks: checks, token0: t0, rQ: rQ, rW: rW, symQ: sq || net.quote.symbol, symW: sw || net.wsigil.symbol, at: Date.now() };
  }
  async function reserves() {
    var res = await call(S.net.pair, SEL.reserves), r0 = wordInt(res, 0), r1 = wordInt(res, 1), q = lc(Q().address);
    return S.route.token0 === q ? { rQ: r0, rW: r1 } : { rQ: r1, rW: r0 };
  }
  async function amountsOut(amt, pp) {
    var d = String(await call(S.net.dex.router, SEL.amountsOut + n32(amt) + n32(64) + n32(pp.length) + pp.map(a32).join(''))).replace(/^0x/, '');
    var len = parseInt(d.slice(64, 128), 16), outs = [];
    for (var i = 0; i < len; i++) outs.push(BigInt('0x' + d.slice(128 + i * 64, 192 + i * 64)));
    return outs;
  }
  async function waitMined(hash, timeoutMs) {
    var until = Date.now() + (timeoutMs || 180000);
    while (Date.now() < until) {
      var r = null; try { r = await rpc('eth_getTransactionReceipt', [hash]); } catch (_) {}
      if (r && r.blockNumber) { if (r.status && BigInt(r.status) === 0n) throw new Error('Transaction reverted on chain: ' + hash); return r; }
      await new Promise(function (ok) { setTimeout(ok, 2000); });
    }
    throw new Error('Timed out waiting for ' + hash + '. It may still land; check the explorer.');
  }

  // ── markup ──────────────────────────────────────────────────────────────
  (function css() {
    if (document.querySelector('link[data-sigil-swap]')) return;
    var l = document.createElement('link'); l.rel = 'stylesheet'; l.href = BASE + 'sigil-swap.css' + (VER ? '?v=' + VER : ''); l.setAttribute('data-sigil-swap', '');
    document.head.appendChild(l);
  })();
  function row(k, id) { return '<div class="ex-row"><span>' + k + '</span><strong id="' + id + '">—</strong></div>'; }
  var hasBridge = !!$('xchain-modal');
  function aside(active) {
    var a = el('aside', 'ex-aside');
    a.innerHTML = '<div class="ex-brand">◈ SIGIL / EXCHANGE</div><h2>Your assets.<br>Across networks.</h2>' +
      '<p>Trade wrapped SIGIL against USDC on Polygon' + (hasBridge ? ', or move native SIGIL across the bridge.' : '.') + '</p>' +
      '<nav><button class="ex-nav" data-ex-route="swap" ' + (active === 'swap' ? 'aria-current="page"' : '') + '>⇄ &nbsp; Swap<small data-ex-swapnet>wSIGIL / USDC</small></button>' +
      (hasBridge ? '<button class="ex-nav" data-ex-route="bridge" ' + (active === 'bridge' ? 'aria-current="page"' : '') + '>↗ &nbsp; Bridge<small>Native SIGIL ↔ wrapped SIGIL</small></button>' : '') +
      '</nav><div class="ex-network" data-ex-netbox>Loading networks…</div>';
    a.querySelector('[data-ex-route="swap"]').onclick = function () { if (S.busy || bridgeBusy) return; if (hasBridge) window.closeXChain(); open(); };
    var b = a.querySelector('[data-ex-route="bridge"]'); if (b) b.onclick = function () { if (S.busy || bridgeBusy) return; close(); window.openXChain('to_polygon'); };
    return a;
  }
  var inlineHost = $('sigil-swap-inline');
  var m = el('div'); m.id = 'exchange-modal';
  if (inlineHost) { S.inline = true; m.className = 'ex-inline'; } else m.style.cssText = 'display:none;position:fixed;inset:0;z-index:10008;align-items:center;justify-content:center';
  m.innerHTML = '<div class="ex-dialog" role="' + (S.inline ? 'region' : 'dialog') + '"' + (S.inline ? '' : ' aria-modal="true"') + ' aria-labelledby="exTitle"><main class="ex-main">' +
    '<div class="ex-top"><h1 id="exTitle">Swap wSIGIL</h1>' + (S.inline ? '' : '<button class="ex-close" id="exClose" aria-label="Close swap">×</button>') + '</div>' +
    '<div class="ex-nets" id="exNets" role="group" aria-label="Network"></div>' +
    '<div class="ex-banner" id="exBanner" hidden></div>' +
    '<div class="ex-segment"><button id="exBuy" aria-pressed="true">Buy wSIGIL</button><button id="exSell" aria-pressed="false">Sell wSIGIL</button></div>' +
    '<div class="ex-token"><div class="ex-token-head"><label for="exAmount">You pay</label><span id="exBalance">Connect to see balance</span></div>' +
    '<div class="ex-token-body"><input id="exAmount" inputmode="decimal" autocomplete="off" placeholder="0.00" aria-describedby="exMessage"><span class="ex-symbol" id="exIn">USDC</span></div>' +
    '<div class="ex-percents"><button data-pct="25">25%</button><button data-pct="50">50%</button><button data-pct="100">Max</button></div></div>' +
    '<button class="ex-flip" id="exFlip" aria-label="Reverse swap direction">↓</button>' +
    '<div class="ex-token"><div class="ex-token-head"><span>You receive · estimate</span><span id="exOutNet">—</span></div>' +
    '<div class="ex-token-body"><output id="exOutput">—</output><span class="ex-symbol" id="exOut">wSIGIL</span></div></div>' +
    '<div class="ex-settings"><span>Slippage tolerance</span><select id="exSlippage" aria-label="Slippage tolerance"><option value="50">0.5%</option><option value="100" selected>1.0%</option><option value="200">2.0%</option></select></div>' +
    '<div class="ex-rows">' + row('Price', 'exPrice') + row('Minimum received', 'exMin') + row('Price impact', 'exImpact') + row('Pool liquidity · estimated', 'exLiquidity') + row('Liquidity provider fee', 'exFee') + row('Your POL for gas', 'exGas') + '</div>' +
    '<details class="ex-route" id="exRoute"><summary id="exRouteSum">Checking the route on chain…</summary><ul id="exRouteList"></ul></details>' +
    '<div class="ex-risk" id="exRisk">Reading the pool…</div>' +
    '<div class="ex-review" id="exReview" hidden></div>' +
    '<button class="ex-action" id="exGo" disabled>Loading…</button>' +
    '<div class="ex-progress" aria-label="Transaction steps"><span id="exStep1">1 · Review</span><span id="exStep2">2 · Approve</span><span id="exStep3">3 · Swap</span><span id="exStep4">4 · Confirm</span></div>' +
    '<div class="ex-message" id="exMessage" role="status" aria-live="polite"></div>' +
    '<div id="exReceipt" class="ex-receipt" hidden></div>' +
    '<div class="ex-faucets" id="exFaucets" hidden></div>' +
    '<div class="ex-foot" id="exFoot"></div>' +
    '</main></div>';
  m.firstChild.prepend(aside('swap'));
  (inlineHost || document.body).appendChild(m);
  var legacyOpen = window.openSwap;
  var nativeHeading = $('swapClose') && $('swapClose').previousElementSibling;
  if (nativeHeading && nativeHeading.children.length === 2) { nativeHeading.firstElementChild.textContent = 'Native SIGIL swap'; nativeHeading.lastElementChild.textContent = 'Native network · available pools load from the node'; }

  function say(s, bad) { $('exMessage').textContent = s; $('exMessage').classList.toggle('ex-error', !!bad); }
  function step(n) { for (var i = 1; i <= 4; i++) $('exStep' + i).classList.toggle('on', i === n); }

  function paintNetwork() {
    var n = S.net;
    $('exNets').innerHTML = S.nets.map(function (x) {
      return '<button data-net="' + esc(x.key) + '" aria-pressed="' + (x === n) + '">' + esc(x.short || x.label) + (x.testnet ? ' <i>test</i>' : '') + '</button>';
    }).join('');
    $('exNets').querySelectorAll('[data-net]').forEach(function (b) { b.onclick = function () { useNet(b.getAttribute('data-net')); }; });
    $('exNets').hidden = S.nets.length < 2;   // one network: no tab row
    var ban = $('exBanner'); ban.hidden = !n.notice; ban.className = 'ex-banner ' + (n.testnet ? 'is-test' : 'is-real'); ban.innerHTML = '<b>' + (n.testnet ? 'TEST MONEY' : 'REAL MONEY') + '</b> ' + esc(n.notice || '');
    document.querySelectorAll('[data-ex-swapnet]').forEach(function (e) { e.textContent = 'wSIGIL / ' + n.quote.symbol + ' · ' + (n.short || n.label); });
    var box = m.querySelector('[data-ex-netbox]');
    if (box) box.innerHTML = '<b>' + esc(n.label) + ' · ' + parseInt(n.chain.chainId, 16) + '</b>Gas is paid in ' + esc(n.chain.nativeCurrency.symbol) + '.' +
      (n.wsigil && n.explorer ? '<br><a href="' + esc(addrLink(n.wsigil.address)) + '" target="_blank" rel="noopener noreferrer">Inspect ' + esc(symW()) + ' ↗</a>' : '') +
      (n.pair && n.explorer ? '<br><a href="' + esc(addrLink(n.pair)) + '" target="_blank" rel="noopener noreferrer">Inspect the pool ↗</a>' : '') +
      '<br>Bridge and market contracts can differ. Check the destination token.';
    $('exOutNet').textContent = n.short || n.label;
    var f = $('exFaucets');
    f.hidden = !(n.testnet && n.faucets && n.faucets.length);
    f.innerHTML = f.hidden ? '' : '<span>Free test money:</span> ' + n.faucets.filter(function (x) { return /^https:\/\//.test(x.url || ''); }).map(function (x) { return '<a href="' + esc(x.url) + '" target="_blank" rel="noopener noreferrer">' + esc(x.label) + ' ↗</a>'; }).join(' · ');
    $('exFoot').innerHTML = esc(n.dex && n.dex.name || 'Uniswap V2') + ' · ' + esc(n.label) + ' · ' + esc(n.chain.nativeCurrency.symbol) + ' required for gas. Approvals are for the exact amount, never unlimited.' +
      '<br>Native SIGIL lives on its own network.' + (hasBridge ? ' <button class="ex-link" id="exBridgeLink">Open bridge ↗</button>' : '') + (legacyOpen ? ' &nbsp;·&nbsp; <button class="ex-link" id="exNative">Native pools ↗</button>' : '') +
      (ready() && n.wsigil ? ' &nbsp;·&nbsp; <button class="ex-link" id="exWatch">Add ' + esc(symW()) + ' to wallet</button>' : '');
    if ($('exBridgeLink')) $('exBridgeLink').onclick = function () { close(); window.openXChain('to_polygon'); };
    if ($('exNative')) $('exNative').onclick = function () { close(); legacyOpen(); };
    if ($('exWatch')) $('exWatch').onclick = watchAsset;
    $('exIn').textContent = syms()[0]; $('exOut').textContent = syms()[1];
    $('exBuy').textContent = 'Buy ' + symW(); $('exSell').textContent = 'Sell ' + symW();
  }
  function paintRoute() {
    var r = S.route, sum = $('exRouteSum'), list = $('exRouteList');
    if (!r || S.routeFor !== S.net.key) { sum.textContent = 'Checking the route on chain…'; list.innerHTML = ''; return; }
    if (r.status === 'missing') {
      sum.innerHTML = '… Not deployed on ' + esc(S.net.label) + ' yet';
      list.innerHTML = r.missing.map(function (x) { return '<li class="ex-wait">' + esc(x) + ' · address not published yet</li>'; }).join('');
      $('exRoute').open = true; return;
    }
    if (r.status === 'error') { sum.innerHTML = '✗ Could not read ' + esc(S.net.label); list.innerHTML = '<li class="ex-bad">' + esc(r.error) + '</li>'; $('exRoute').open = true; return; }
    sum.innerHTML = r.ok ? '✓ Route checked in your browser · ' + r.checks.length + ' of ' + r.checks.length : '✗ Route failed a check. Do not sign anything.';
    list.innerHTML = r.checks.map(function (c) { return '<li class="' + (c.ok ? 'ex-ok' : 'ex-bad') + '"><b>' + (c.ok ? '✓' : '✗') + ' ' + esc(c.label) + '</b><span>' + esc(c.detail) + '</span></li>'; }).join('');
    $('exRoute').open = !r.ok;
  }
  function paintPool() {
    var r = S.route;
    if (!ready()) { $('exPrice').textContent = '—'; $('exLiquidity').textContent = '—'; return; }
    var q = Q(), w = W(), p = num(r.rQ, q.decimals) / num(r.rW, w.decimals);
    $('exPrice').textContent = '1 ' + symW() + ' = ' + sig(p) + ' ' + symQ();
    $('exLiquidity').textContent = '≈ ' + sig(2 * num(r.rQ, q.decimals)) + ' ' + symQ() + (S.net.testnet ? ' (test)' : '');
  }
  function action() {
    var b = $('exGo'), st = MM.getState(), q = S.quote;
    function set(t, dis) { b.textContent = t; b.disabled = !!dis; }
    if (S.busy) return;
    if (!S.net) return set('Loading…', true);
    if (!S.route || S.routeFor !== S.net.key) return set('Checking route…', true);
    if (S.route.status === 'missing') return set('Pair not deployed yet', true);
    if (!S.route.ok) return set(S.route.status === 'error' ? 'Retry' : 'Route failed checks', S.route.status !== 'error');
    if (!st.connected) return set('Connect wallet');
    if (!onNet()) return set('Switch to ' + S.net.label);
    if (!q) return set($('exAmount').value ? 'Refresh quote' : 'Enter an amount', !$('exAmount').value);
    if (q.impact >= IMPACT_MAX) return set('Reduce amount · impact above ' + IMPACT_MAX + '%', true);
    if (S.bal !== null && q.amt > S.bal) return set('Not enough ' + q.sym[0], true);
    if (S.native === 0n) return set('You need ' + S.net.chain.nativeCurrency.symbol + ' for gas', true);
    set(S.reviewing ? 'Confirm swap in wallet' : 'Review swap');
  }
  function invalidate() {
    S.version++; S.quote = null; S.reviewing = false; $('exReview').hidden = true; $('exReceipt').hidden = true; $('exReceipt').innerHTML = '';
    ['exOutput', 'exMin', 'exImpact'].forEach(function (i) { $(i).textContent = '—'; }); $('exFee').textContent = '0.30%';
    action();
  }
  async function balances() {
    var st = MM.getState(), acct = st.account, sell = S.sell, key = S.net.key;
    S.bal = null; S.native = null; $('exGas').textContent = '—';
    if (!acct || !ready()) { $('exBalance').textContent = acct ? 'Balance —' : 'Connect to see balance'; action(); return; }
    $('exBalance').textContent = 'Reading balance…';
    try {
      var r = await Promise.all([call(sell ? W().address : Q().address, SEL.balanceOf + a32(acct)), rpc('eth_getBalance', [acct, 'latest'])]);
      if (MM.getState().account !== acct || S.sell !== sell || S.net.key !== key) return;
      S.bal = BigInt(r[0] || '0x0'); S.native = BigInt(r[1] || '0x0');
      $('exBalance').textContent = 'Balance ' + fmt(S.bal, dps()[0], 8);
      $('exGas').textContent = fmt(S.native, 18, 6) + ' ' + S.net.chain.nativeCurrency.symbol + (S.native === 0n ? ' · needed for gas' : '');
    } catch (_) { $('exBalance').textContent = 'Balance unavailable'; }
    action();
  }
  async function checkRoute(force) {
    var net = S.net;
    if (!force && S.route && S.routeFor === net.key && Date.now() - (S.route.at || 0) < 60000) { paintRoute(); paintPool(); action(); return; }
    S.route = null; S.routeFor = null; paintRoute(); action();
    var r;
    try { r = await verifyRoute(net); } catch (e) { r = { ok: false, status: 'error', error: (e && e.message) || String(e), checks: [] }; }
    if (S.net !== net) return;
    S.route = r; S.routeFor = net.key;
    S.riskText = r.status === 'missing' ? 'Nothing to trade on ' + net.label + ' until the pair is deployed. This page fills in by itself once its address is published.'
      : !r.ok ? 'The route did not pass its on-chain checks, so this page will not ask you to sign anything.'
      : net.testnet ? 'Test pool: tokens are free and worthless. Your trade still moves the price like a real one; the estimate includes the 0.30% pool fee.'
      : 'Small pool: your trade changes the exchange rate. The estimate includes the 0.30% pool fee; gas is additional.';
    $('exRisk').textContent = S.riskText;
    paintNetwork(); paintRoute(); paintPool(); action(); balances();
    if ($('exAmount').value) quote();
  }
  async function quote() {
    invalidate(); if (!ready()) return;
    var version = S.version, dp = dps(), pp = path(), sym = syms(), amt;
    try { amt = units($('exAmount').value, dp[0]); if (amt <= 0n) throw new Error('Enter an amount greater than zero.'); }
    catch (e) { if ($('exAmount').value) say(e.message, true); else say(''); return; }
    say('Fetching a fresh quote from ' + S.net.label + '…');
    try {
      var all = await Promise.all([amountsOut(amt, pp), reserves()]);
      if (version !== S.version || S.busy) return;
      var out = all[0][all[0].length - 1], rQ = all[1].rQ, rW = all[1].rW;
      if (out <= 0n || rQ <= 0n || rW <= 0n) throw new Error('The pool cannot fill this amount.');
      S.route.rQ = rQ; S.route.rW = rW; paintPool();
      var rIn = S.sell ? rW : rQ, rOut = S.sell ? rQ : rW;
      var impact = Math.max(0, 100 * (1 - Number(out * rIn) / Number(amt * rOut)));
      var slip = Number($('exSlippage').value), min = out * BigInt(10000 - slip) / 10000n;
      if (min <= 0n) throw new Error('Amount is too small to protect with a minimum output.');
      S.quote = { amt: amt, out: out, min: min, slip: slip, path: pp, dp: dp, sym: sym, impact: impact, time: Date.now(), version: version, net: S.net.key };
      $('exOutput').textContent = fmt(out, dp[1], 6); $('exMin').textContent = fmt(min, dp[1], 12) + ' ' + sym[1]; $('exImpact').textContent = impact.toFixed(2) + '%';
      $('exRisk').textContent = S.riskText || '';
      if (impact >= IMPACT_MAX) $('exRisk').textContent = 'High price impact. This amount consumes a large part of the pool. Reduce the amount before swapping.';
      say('Quote ready · expires after 30 seconds.');
    } catch (e) { if (version === S.version) say('Quote unavailable: ' + ((e && e.message) || e), true); }
    action();
  }
  function showReview() {
    var q = S.quote, n = S.net;
    $('exReview').innerHTML = '<div class="ex-review-h">Review · ' + esc(n.label) + (n.testnet ? ' · test money' : ' · real money') + '</div>' +
      '<div class="ex-row"><span>You pay</span><strong>' + esc(fmt(q.amt, q.dp[0])) + ' ' + esc(q.sym[0]) + '</strong></div>' +
      '<div class="ex-row"><span>You receive at least</span><strong>' + esc(fmt(q.min, q.dp[1])) + ' ' + esc(q.sym[1]) + '</strong></div>' +
      '<div class="ex-row"><span>Price impact · slippage</span><strong>' + q.impact.toFixed(2) + '% · ' + (q.slip / 100) + '%</strong></div>' +
      '<div class="ex-row"><span>Pool</span><strong>' + esc(short(lc(n.pair))) + '</strong></div>' +
      '<p>Your wallet asks up to twice: first to approve exactly ' + esc(fmt(q.amt, q.dp[0])) + ' ' + esc(q.sym[0]) + ' (skipped if already approved), then to swap. ' + esc(n.chain.nativeCurrency.symbol) + ' gas is additional.</p>' +
      '<button class="ex-link" id="exReviewCancel">Cancel</button>';
    $('exReview').hidden = false; S.reviewing = true; step(1);
    $('exReviewCancel').onclick = function () { if (S.busy) return; S.reviewing = false; $('exReview').hidden = true; step(0); say('Review cancelled. No swap requested.'); action(); };
    say('Check the review, then confirm. The quote expires 30 seconds after it was fetched.');
    action();
  }
  function lock(on) { S.busy = on; m.querySelectorAll('input,select,button').forEach(function (e) { e.disabled = on; }); }
  async function execute(q) {
    var acct = MM.getState().account, net = S.net, R = net.dex.router, tokenIn = q.path[0], tokenOut = q.path[q.path.length - 1];
    var cur = BigInt(await call(tokenIn, SEL.allowance + a32(acct) + a32(R)) || '0x0');
    if (cur < q.amt) {
      step(2); say('Confirm the exact token approval in your wallet.');
      var ap = await MM.request('eth_sendTransaction', [{ from: acct, to: tokenIn, data: SEL.approve + a32(R) + n32(q.amt) }]);
      say('Approval submitted. Waiting for ' + net.label + ' to confirm it.');
      await waitMined(ap);
    }
    if (MM.getState().account !== acct || !onNet() || S.net !== net) throw new Error('Wallet or network changed. Review the swap again.');
    var latest = await amountsOut(q.amt, q.path);
    if (latest[latest.length - 1] < q.min) throw new Error('Price moved below your minimum after approval. Get a fresh quote; the approval may already be confirmed.');
    step(3); say('Confirm the swap in your wallet.');
    var deadline = Math.floor(Date.now() / 1000) + 900;
    var data = SEL.swapExact + n32(q.amt) + n32(q.min) + n32(160) + a32(acct) + n32(deadline) + n32(q.path.length) + q.path.map(a32).join('');
    var tx = await MM.request('eth_sendTransaction', [{ from: acct, to: R, data: data }]);
    step(4); say('Swap submitted. Waiting for ' + net.label + ' to confirm it.');
    $('exReceipt').hidden = false; $('exReceipt').innerHTML = 'Submitted · confirmation pending' + (txLink(tx) ? '<br><a href="' + esc(txLink(tx)) + '" target="_blank" rel="noopener noreferrer">Track transaction ↗</a>' : '');
    var rc = await waitMined(tx), got = 0n, me = pad(lc(acct));
    (rc.logs || []).forEach(function (lg) {
      if (lc(lg.address) === lc(tokenOut) && lg.topics && lc(lg.topics[0]) === TRANSFER && lc(String(lg.topics[2] || '').replace(/^0x/, '')) === me) got += BigInt(lg.data || '0x0');
    });
    return { tx: tx, got: got };
  }
  async function go() {
    if (S.busy || S.executing) return; S.executing = true;
    try {
      if (!S.route || S.routeFor !== S.net.key || S.route.status === 'error') { await checkRoute(true); return; }
      if (!ready()) return;
      if (!MM.getState().connected) { await MM.connect(); await balances(); if ($('exAmount').value) await quote(); return; }
      if (!onNet()) { await MM.ensureChain(S.net.chain); await balances(); if ($('exAmount').value) await quote(); return; }
      var q = S.quote;
      if (!q || q.net !== S.net.key || Date.now() - q.time > QUOTE_TTL) { await quote(); if (S.quote) say('Quote refreshed. Review the current minimum, then continue.'); return; }
      if (q.impact >= IMPACT_MAX) return;
      if (S.bal === null) { await balances(); if (S.bal === null) throw new Error('Could not read your balance. Retry when ' + S.net.label + ' is reachable.'); }
      if (q.amt > S.bal) throw new Error('Amount exceeds your ' + q.sym[0] + ' balance.');
      if (!S.reviewing) { showReview(); return; }
      lock(true); $('exGo').textContent = 'Waiting for wallet…';
      var r = await execute(q);
      $('exReview').hidden = true; S.reviewing = false;
      $('exReceipt').hidden = false;
      $('exReceipt').innerHTML = '✓ Swap confirmed on ' + esc(S.net.label) + '.<br>You received <b>' + esc(fmt(r.got, q.dp[1])) + ' ' + esc(q.sym[1]) + '</b> (minimum was ' + esc(fmt(q.min, q.dp[1])) + ').' +
        (txLink(r.tx) ? '<br><a href="' + esc(txLink(r.tx)) + '" target="_blank" rel="noopener noreferrer">View transaction ↗</a>' : '');
      say('Confirmed. Balances are refreshing.'); $('exAmount').value = ''; S.quote = null;
      lock(false); await checkRoute(true);
      if (window.SigilMarket && window.SigilMarket.refresh) window.SigilMarket.refresh();
    } catch (e) {
      S.reviewing = false; $('exReview').hidden = true;
      say(e && e.code === 4001 ? 'Rejected in your wallet. If an approval was already submitted, you can see it in the explorer.' : ((e && e.message) || String(e)), true);
    } finally { S.executing = false; if (S.busy) lock(false); action(); }
  }
  async function watchAsset() {
    if (!ready() || !MM.getState().connected) { say('Connect a wallet first.', true); return; }
    try { if (!onNet()) await MM.ensureChain(S.net.chain); await MM.request('wallet_watchAsset', { type: 'ERC20', options: { address: W().address, symbol: symW().slice(0, 11), decimals: W().decimals } }); say(symW() + ' added to your wallet list.'); }
    catch (e) { say(e && e.code === 4001 ? 'Not added.' : 'Your wallet did not add the token: ' + ((e && e.message) || e), true); }
  }
  function useNet(key) {
    if (S.busy) return;
    var n = S.nets.filter(function (x) { return x.key === key; })[0]; if (!n || n === S.net) return;
    S.net = n; try { localStorage.setItem('sigil-swap-net', key); } catch (_) {}
    $('exAmount').value = ''; say(''); step(0); invalidate(); paintNetwork(); checkRoute(false);
  }
  function setSide(sell) {
    if (S.busy) return; S.sell = sell; $('exAmount').value = '';
    $('exBuy').setAttribute('aria-pressed', String(!sell)); $('exSell').setAttribute('aria-pressed', String(sell));
    if (S.net) paintNetwork(); invalidate(); say(''); balances();
  }
  var booted = null;
  function boot() {
    if (!booted) booted = loadConfig().then(function () { paintNetwork(); return checkRoute(false); }).catch(function (e) {
      booted = null; $('exRisk').textContent = 'The swap could not load its network list: ' + ((e && e.message) || e) + '. Nothing can be signed.'; $('exGo').textContent = 'Unavailable'; $('exGo').disabled = true;
    });
    return booted;
  }
  function open() {
    if (bridgeBusy) return;
    if (!S.inline) { if (hasBridge) window.closeXChain(); S.returnFocus = document.activeElement; m.style.display = 'flex'; document.body.style.overflow = 'hidden'; }
    boot().then(function () { if (S.net) checkRoute(false); });
    $('exAmount').focus();
  }
  function close() { if (S.busy || S.inline) return; m.style.display = 'none'; document.body.style.overflow = ''; if (S.returnFocus && S.returnFocus.focus) S.returnFocus.focus(); }
  window.openSwap = open;
  window.SigilSwap = { open: open, close: close, use: useNet, state: function () { return { net: S.net && S.net.key, route: S.route && S.route.status, quote: !!S.quote, busy: S.busy }; } };
  if ($('exClose')) $('exClose').onclick = close;
  if (!S.inline) m.onclick = function (e) { if (e.target === m) close(); };
  $('exBuy').onclick = function () { setSide(false); }; $('exSell').onclick = function () { setSide(true); }; $('exFlip').onclick = function () { setSide(!S.sell); };
  var timer; $('exAmount').oninput = function () { invalidate(); clearTimeout(timer); timer = setTimeout(quote, 300); };
  $('exSlippage').onchange = quote; $('exGo').onclick = go;
  m.querySelectorAll('[data-pct]').forEach(function (b) { b.onclick = function () { if (S.bal === null) { say('Connect a wallet and wait for its balance.', true); return; } $('exAmount').value = plain(S.bal * BigInt(b.dataset.pct) / 100n, dps()[0]); quote(); }; });
  setInterval(function () { if (S.quote && !S.busy && Date.now() - S.quote.time > QUOTE_TTL) { invalidate(); say('Quote expired. Refresh before reviewing.'); } }, 1000);
  MM.on('change', function () { if (S.busy) return; invalidate(); balances(); if ($('exAmount').value) quote(); });
  if (S.inline) { boot(); if (MM.resume) MM.resume().catch(function () {}); }

  // ── bridge modal wrapper: kept from exchange-v1 unchanged except the market token it names ──
  var bridgeBusy = false;
  var bridge = $('xchain-modal'), box = bridge && bridge.querySelector('.xc-box');
  if (box) {
    var originalBridge = window.openXChain, originalClose = window.closeXChain, originalGo = window.xcGo, originalDir = window.xcSetDir;
    box.prepend(aside('bridge')); box.setAttribute('role', 'dialog'); box.setAttribute('aria-modal', 'true'); box.setAttribute('aria-label', 'Bridge SIGIL and Polygon');
    var direct = bridge.querySelector('[id="uniGo"]'); if (direct) { var oldPanel = direct.parentElement; oldPanel.hidden = true; oldPanel.style.display = 'none'; }
    var note = el('div', 'ex-foot', 'Bridge wraps native SIGIL at 1:1. It does not exchange SIGIL for USDC. A submitted lock still requires settlement and a relayer mint.'); $('xcForm').prepend(note);
    var route = el('div', 'ex-risk'); route.id = 'exBridgeSummary'; $('xcForm').insertBefore(route, $('xcGo'));
    $('xcAmt').setAttribute('aria-label', 'Bridge amount'); $('xcDest').setAttribute('aria-label', 'Destination wallet address'); $('xcMsg').setAttribute('role', 'status'); $('xcMsg').setAttribute('aria-live', 'polite');
    bridge.querySelector('.xc-x').setAttribute('aria-label', 'Close bridge');
    bridge.querySelectorAll('.xc-half').forEach(function (e) { e.setAttribute('role', 'button'); e.tabIndex = 0; e.onkeydown = function (k) { if (k.key === 'Enter' || k.key === ' ') { k.preventDefault(); e.click(); } }; });
    var summary = function () {
      var back = bridge.dataset.dir === 'to_sigil', t = MM.TOKEN.address, mk = S.net && S.net.wsigil;
      route.textContent = (back ? 'Polygon → native SIGIL' : 'Native SIGIL → Polygon') + ' · 1:1 wrapping · bridge token ' + t + '.' +
        (mk ? ' The market trades ' + (mk.symbol || 'wSIGIL') + ' (' + mk.address + ') on ' + S.net.label + '; these can be different contracts.' : '');
    };
    window.xcSetDir = function (d) { if (bridgeBusy) return; originalDir(d); summary(); };
    window.openXChain = function (d) { if (S.busy || bridgeBusy) return; close(); S.returnFocus = document.activeElement; originalBridge(d); document.body.style.overflow = 'hidden'; summary(); $('xcAmt').focus(); };
    window.closeXChain = function () { if (bridgeBusy) return; originalClose(); document.body.style.overflow = ''; if (S.returnFocus && S.returnFocus.focus) S.returnFocus.focus(); };
    window.openBridge = function () { window.openXChain('to_polygon'); }; window.openBridgeFrom = window.openBridge;
    window.xcGo = async function () {
      if (bridgeBusy || window.__exchangeBridgeRunning) return; window.__exchangeBridgeRunning = true; var back = bridge.dataset.dir === 'to_sigil', dest = $('xcDest').value.trim(), amount;
      try {
        amount = MM.parseAmount8($('xcAmt').value); if (amount <= 0n) throw new Error('Enter an amount greater than zero.');
        if (!back && amount * 100n > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error('Amount exceeds the bridge API safe precision limit. Use a smaller transfer.');
        if (!(back ? /^[0-9a-f]{64}$/i : /^0x[0-9a-f]{40}$/i).test(dest) || /^0x0{40}$/i.test(dest) || /^0{64}$/.test(dest)) throw new Error('Enter a valid non-zero destination address for ' + (back ? 'SIGIL.' : 'Polygon.'));
        if (back && (!MM.burnEnabled || !MM.burnEnabled())) throw new Error('Return bridge is disabled. Do not burn wrapped tokens.');
        if (!back) { var raw = window.SIGIL_RAW; if (raw == null) throw new Error('Native balance is unavailable. Refresh your wallet.'); if (amount > BigInt(raw) / 100n) throw new Error('Amount exceeds your available native balance.'); }
        if (!back && !(await window.sigilConfirm('Bridge ' + fmt(amount, 8) + ' native SIGIL to Polygon?\n\nRecipient: ' + dest + '\nBridge token: ' + MM.TOKEN.address + '\n\nThis locks SIGIL before settlement and relayer minting. It does not buy wSIGIL.'))) return;
        bridgeBusy = true; bridge.querySelectorAll('input,button,select').forEach(function (e) { e.dataset.exDisabled = e.disabled ? '1' : '0'; e.disabled = true; });
        await originalGo();
      } catch (e) { $('xcMsg').textContent = e.message; $('xcMsg').style.color = '#ffa5b2'; }
      finally { bridgeBusy = false; window.__exchangeBridgeRunning = false; bridge.querySelectorAll('[data-ex-disabled]').forEach(function (e) { e.disabled = e.dataset.exDisabled === '1'; delete e.dataset.exDisabled; }); if (back && !MM.burnEnabled()) $('xcGo').disabled = true; }
    };
    window.uniTrade = open;   // every visible Polygon swap uses the same reviewed flow
  }
  document.addEventListener('keydown', function (e) {
    var active = (!S.inline && m.style.display === 'flex') ? m : bridge && bridge.style.display === 'flex' ? bridge : null; if (!active) return;
    if ($('ask-modal') && $('ask-modal').style.display !== 'none') return;
    if (e.key === 'Escape') { e.stopImmediatePropagation(); if (active === m) close(); else window.closeXChain(); }
    if (e.key === 'Tab') {
      var nodes = Array.from(active.querySelectorAll('button,input,select,a[href],summary,[tabindex="0"]')).filter(function (n) { return !n.disabled && n.getClientRects().length; }); if (!nodes.length) return;
      var first = nodes[0], last = nodes[nodes.length - 1];
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); } else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    }
  }, true);
})();
