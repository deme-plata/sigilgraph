/* ══ KRISTENSEN K-GAUGE — global site topbar · v3 (2026-09-14) ════════════════════════════
   A self-injecting chip + dropdown that reads the K-gauge from the node's OWN live
   same-origin /v1/* endpoints. No node change, no build step, no framework: drop
   <script src="/kgauge.js" defer></script> into any page on this origin and it appears.

   v3 replaces the legacy v1 modal (ΔH = reject ratio + peer churn, "K*≈1 is the
   Margolus–Levitin boundary"). Both claims were retired after the 2026-09-14 battle test
   (sigilgraph.org/downloads/sigil-kparam-battle-2026-09-14.pdf): v1's ΔH detected topology
   change, not disagreement (0.00 → 17.74 when one peer of five dropped, nothing in dispute),
   and the ħ form cannot land in its own ladder (floor 7.87 for any physical energy).

   What is computed here, identically to the wallet chip and the flux_sigil_kgauge MCP tool:

     ΔH_c  = 0.20·tip + 0.35·state-root + 0.20·finality + 0.25·semantic   (state DISAGREEMENT, [0,1], lower bound)
     K_fix = 2π·√( ΔH_c · τ_d · w_S )      τ_d = (1 + min(d,512)/512)/2  — persistence in BLOCKS
                                            w_S = 1 − H_norm/4 ∈ [¾,1]   — proposer entropy as a bounded weight
     K_C   = 2π·√( ΔH_c · (τ/τ₀) · [ε + (1−ε)·H_norm] )   τ = 512/block-rate (seconds), ε = 0.1  — the v2 gauge

   K_fix is the headline: no seconds, no ħ, no ε; a chain that speeds up does not "cool", one
   key forking against itself reads worst, rotating keys moves it ≤ 15 %, zero disagreement
   reads 0 for any key count. K_C stays beside it (its τ/τ₀ makes the same disagreement read
   ~4× lower at 110 blk/s than at 8 blk/s). Same ladder for both: <1 stable · 1–3 elevated · ≥3 critical.
   Every channel says whether it was measured or is unavailable; a missing source is never a zero. */
(function () {
  "use strict";
  if (window.__kgInstalled) return;          // idempotent: safe on a page that also embeds it
  window.__kgInstalled = true;

  var API = (function () { try { return window.__SIGIL_API_BASE || ''; } catch (e) { return ''; } })();
  function u(p) { return (API || '') + p; }
  var FINAL_DEPTH = 512, TAU0 = 100, EPS = 0.1, W = { tip: 0.20, state: 0.35, fin: 0.20, conf: 0.25 };

  var CSS = [
    "#kgChip{position:fixed;top:10px;right:12px;z-index:2147483000;display:flex;align-items:center;gap:7px;",
    "padding:6px 11px;border-radius:999px;cursor:pointer;font-family:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,monospace;",
    "font-size:12px;background:rgba(18,14,32,.82);border:1px solid rgba(139,92,246,.45);",
    "color:#e2e8f0;backdrop-filter:blur(8px);-webkit-backdrop-filter:blur(8px);user-select:none;transition:border-color .2s}",
    "#kgChip:hover{border-color:#c084fc}",
    "#kgChip .kgK{font-weight:700;color:#5eead4}",
    "#kgChip .kgKc{font-size:10px;color:#c084fc;opacity:.9}",
    "#kgChip .kgSig{color:#8b5cf6;font-size:14px;font-weight:800}",
    "#kgChip .kgReg{font-size:9.5px;text-transform:uppercase;letter-spacing:.12em;padding:1px 7px;border-radius:999px}",
    ".kg-stable{background:rgba(16,185,129,.18);color:#34d399;border:1px solid rgba(16,185,129,.35)}",
    ".kg-elevated{background:rgba(245,158,11,.18);color:#fbbf24;border:1px solid rgba(245,158,11,.35)}",
    ".kg-critical{background:rgba(239,68,68,.18);color:#f87171;border:1px solid rgba(239,68,68,.4)}",
    ".kg-unknown{background:rgba(120,130,150,.18);color:#94a3b8;border:1px solid rgba(120,130,150,.35)}",
    ".kg-blind{background:rgba(148,163,184,.16);color:#cbd5e1;border:1px dashed rgba(203,213,225,.55)}",
    "#kgPanel{position:fixed;top:52px;right:12px;z-index:2147483000;width:400px;max-width:calc(100vw - 24px);",
    "max-height:calc(100vh - 70px);overflow-y:auto;display:none;font-family:'JetBrains Mono',ui-monospace,monospace;",
    "background:#12101c;border:1px solid rgba(139,92,246,.4);border-radius:14px;",
    "box-shadow:0 24px 70px rgba(0,0,0,.85),0 0 42px rgba(139,92,246,.16);color:#e2e8f0}",
    "#kgPanel.on{display:block}",
    "#kgPanel .kgHd{padding:13px 15px;border-bottom:1px solid rgba(255,255,255,.06);display:flex;justify-content:space-between;align-items:flex-start;gap:10px}",
    "#kgPanel .kgTitle{font-size:11px;text-transform:uppercase;letter-spacing:.16em;color:rgba(255,255,255,.42)}",
    "#kgPanel .kgForm{font-size:10px;color:rgba(255,255,255,.6);margin-top:3px;line-height:1.5}",
    "#kgPanel .kgForm i{font-style:normal;color:#5eead4}",
    "#kgPanel .kgBig{font-size:26px;font-weight:800;line-height:1;text-align:right}",
    "#kgPanel .kgStar{font-size:10px;color:rgba(255,255,255,.55);text-align:right;margin-top:2px;white-space:nowrap}",
    "#kgPanel .kgSecT{padding:8px 15px 0;font-size:9px;text-transform:uppercase;letter-spacing:.14em;color:rgba(255,255,255,.35)}",
    "#kgPanel .kgGrid{padding:8px 15px 11px;display:grid;grid-template-columns:1fr 1fr;gap:8px 14px;border-bottom:1px solid rgba(255,255,255,.06)}",
    "#kgPanel .kgCell .kgLab{font-size:9px;color:rgba(255,255,255,.4);text-transform:uppercase;letter-spacing:.08em}",
    "#kgPanel .kgCell .kgLab i{font-style:normal;text-transform:none}",
    "#kgPanel .kgCell .kgVal{font-size:13px;font-weight:600;color:#cfe9f2}",
    "#kgPanel .kgCell .kgVal.kgNa{color:#fbbf24;font-weight:500}",
    "#kgPanel .kgCell .kgSub{font-size:8.5px;color:rgba(255,255,255,.32);line-height:1.35}",
    "#kgPanel .kgSec{padding:11px 15px;border-bottom:1px solid rgba(255,255,255,.06);font-size:11px;line-height:1.5}",
    "#kgPanel .kgSec b{color:#c084fc;font-weight:600}",
    "#kgPanel .kgProv{font-size:9.5px;color:rgba(255,255,255,.5);line-height:1.45}",
    "#kgPanel .kgProv .kgMeas{color:#34d399}",
    "#kgPanel .kgProv .kgUnav{color:#fbbf24}",
    "#kgPanel .kgFoot{padding:9px 15px;font-size:9px;color:rgba(255,255,255,.3);line-height:1.4}",
    "#kgPanel .kgX{cursor:pointer;color:rgba(255,255,255,.4);font-size:16px;line-height:1}",
    "@media (max-width:520px){#kgChip{top:6px;right:6px;font-size:11px;padding:5px 9px}#kgPanel{top:44px;right:6px}}"
  ].join('');

  var HTML =
    '<div id="kgChip" title="Kristensen consensus gauge — K_fix (headline) beside K_C: how far the network\'s views are from one shared state, and for how many blocks">' +
      '<span class="kgSig">&#922;</span><span class="kgK" id="kgChipK">&mdash;</span>' +
      '<span class="kgReg kg-unknown" id="kgChipReg">measuring</span>' +
      '<span class="kgKc" id="kgChipKc" title="K_C — the v2 gauge (seconds-scaled)">K<sub>C</sub> &mdash;</span>' +
    '</div>' +
    '<div id="kgPanel">' +
      '<div class="kgHd"><div>' +
        '<div class="kgTitle">Kristensen consensus gauge</div>' +
        '<div class="kgForm"><i>K<sub>fix</sub></i> = 2&pi;&middot;&radic;(&Delta;H<sub>c</sub> &middot; &tau;<sub>d</sub> &middot; w<sub>S</sub>)<br>' +
        'K<sub>C</sub> = 2&pi;&middot;&radic;(&Delta;H<sub>c</sub> &middot; &tau;/&tau;&#8320; &middot; [&epsilon; + (1&minus;&epsilon;)&middot;H<sub>norm</sub>])</div>' +
      '</div><div>' +
        '<div class="kgBig" id="kgBigK" style="color:#94a3b8">&mdash;</div>' +
        '<div class="kgStar" id="kgStar">K<sub>C</sub> &mdash;</div>' +
        '<span class="kgX" id="kgX">&#10005;</span>' +
      '</div></div>' +
      '<div class="kgSec"><div><b>Regime:</b> <span id="kgRegimeTxt">&mdash;</span></div>' +
        '<div style="margin-top:3px"><b>&Delta;H<sub>c</sub> state disagreement</b> <span id="kgDHc">&mdash;</span> &middot; <b>dominant:</b> <span id="kgDom">&mdash;</span></div></div>' +
      '<div class="kgSecT">the four disagreement channels (weights)</div><div class="kgGrid" id="kgGridState"></div>' +
      '<div class="kgSecT">persistence &middot; proposer structure &middot; time</div><div class="kgGrid" id="kgGridProp"></div>' +
      '<div class="kgSecT">network &middot; confidence</div><div class="kgGrid" id="kgGridNet"></div>' +
      '<div class="kgSec kgProv" id="kgProv"></div>' +
      '<div class="kgSec" style="color:rgba(255,255,255,.72)"><b>What this measures.</b> Not "is the chain fast" and not "did a peer connect", ' +
        'but how far the network\'s views are from <i>one shared state</i>, and for how many blocks that has been so. ' +
        '&Delta;H<sub>c</sub> is disagreement the nodes can actually see (a fork in the DAG, a different state root at the ' +
        'same height, a settled-height gap, a refused claim); &tau;<sub>d</sub> is how many blocks it has persisted, ' +
        'measured against the 512-block finality depth; w<sub>S</sub> says how concentrated the producers are, and it is ' +
        'bounded so that one key forking against itself is the <i>worst</i> case and rotating keys buys nothing. ' +
        'K<sub>fix</sub> has no seconds and no &#8463; in it: the Margolus&ndash;Levitin count (how many physically distinguishable ' +
        'steps a round spends) is a real number, but it is an <i>agreement cost</i>, not a stress level &mdash; it lives on the ' +
        'Kristensen Time page, never on this ladder. The 2026-09-14 battle test that forced this change is at ' +
        '<a href="/downloads/sigil-kparam-battle-2026-09-14.pdf" style="color:#c084fc">/downloads/sigil-kparam-battle-2026-09-14.pdf</a>.</div>' +
      '<div class="kgFoot" id="kgFoot">tip &mdash; &middot; peers &mdash; &middot; sampled over &mdash; s</div>' +
      '<div class="kgFoot" id="kgFort" style="border-top:1px solid rgba(255,255,255,.08);color:rgba(255,255,255,.55);font-size:10px"></div>' +
    '</div>';

  function install() {
    var st = document.createElement('style'); st.textContent = CSS; document.head.appendChild(st);
    var host = document.createElement('div'); host.innerHTML = HTML;
    while (host.firstChild) document.body.appendChild(host.firstChild);

    var panelOpen = false;
    window.__kgToggle = function (v) {
      panelOpen = (v === undefined) ? !panelOpen : v;
      document.getElementById('kgPanel').classList.toggle('on', panelOpen);
      if (panelOpen) refresh();
    };
    document.getElementById('kgChip').addEventListener('click', function () { window.__kgToggle(); });
    document.getElementById('kgX').addEventListener('click', function () { window.__kgToggle(false); });
    document.addEventListener('keydown', function (e) { if (e.key === 'Escape') window.__kgToggle(false); });
    document.addEventListener('click', function (e) {
      var p = document.getElementById('kgPanel'), c = document.getElementById('kgChip');
      if (panelOpen && p && !p.contains(e.target) && !c.contains(e.target)) window.__kgToggle(false);
    }, true);

    setTimeout(refresh, 2500);
    setInterval(function () { if (!document.hidden) refresh(); }, 90000);
  }

  function getJSON(path) {
    return fetch(u(path), { cache: 'no-store' }).then(function (r) { return r.json(); }).catch(function () { return null; });
  }
  function shannonBits(counts) {
    var n = 0, i; for (i in counts) n += counts[i];
    if (n <= 0) return 0;
    var H = 0; for (i in counts) { var pi = counts[i] / n; if (pi > 0) H -= pi * Math.log2(pi); }
    return Math.max(0, H);
  }
  function resolutionBits(n) { return n < 2 ? 0 : shannonBits({ a: n - 1, b: 1 }); }
  // stale_height / duplicate / no_tip = network timing (noise); verify_mismatch / non_canonical_wallet =
  // an invalid or competing claim about state (semantic). Anything new is 'unclassified' and only widens the band.
  function rejectClass(k) {
    if (k === 'stale_height' || k === 'duplicate' || k === 'already_known' || k === 'late' || k === 'no_tip') return 'noise';
    if (k === 'verify_mismatch' || k === 'non_canonical_wallet' || k === 'invalid_signature' || k === 'bad_proof' || k === 'invalid_state' || k === 'state_root_mismatch' || k === 'conflicting_spend' || k === 'wrong_parent' || k === 'consensus_rule') return 'semantic';
    return 'unclassified';
  }
  function kC(d, tauRatio, hNorm) { var plur = EPS + (1 - EPS) * Math.min(1, Math.max(0, hNorm)); return 2 * Math.PI * Math.sqrt(Math.max(0, d) * Math.max(0, tauRatio) * plur); }
  function tauD(persist) { return (1 + Math.min(FINAL_DEPTH, Math.max(0, persist)) / FINAL_DEPTH) / 2; }
  function wS(hNorm) { return 1 - Math.min(1, Math.max(0, hNorm)) / 4; }
  function kFix(d, persist, hNorm) { return 2 * Math.PI * Math.sqrt(Math.min(1, Math.max(0, d)) * tauD(persist) * wS(hNorm)); }
  function regimeOf(k) { if (!isFinite(k)) return 'unknown'; if (k >= 3) return 'critical'; if (k >= 1) return 'elevated'; return 'stable'; }

  /* ── The WITNESS term W — absence of conflict is not presence of consensus ──
     Viktor, 2026-09-22: "et dødt nervesystem kan også være stille."
     K on this gauge is a LOWER BOUND built from disagreement that was OBSERVED, so
     K = 0 has two causes and the chip printed the same word for both: many
     independent nodes agreed, or nobody was there to disagree. W does not measure
     disagreement — it measures whether a disagreement COULD have been seen. It is a
     MINIMUM of its senses, never a product: a chain is only as witnessed as its
     weakest sense, and a minimum names the sense that failed.
     Measured against three real freezes (09-08, 09-15, 09-22) in which K read 0.00
     "stable" throughout. Mirrors fluxc-mcp handlers/sigil_kgauge.rs::witness_of. */
  var CERT_FRESH_TAUS = 2, CERT_BLIND_TAUS = 20, CERT_FRESH_MIN_SECS = 120, WITNESS_BLIND_BELOW = 0.34;
  function certFreshness(ageSecs, tauFin) {
    if (ageSecs === null || ageSecs === undefined || !isFinite(ageSecs)) return null;
    var tau = (isFinite(tauFin) && tauFin > 0) ? tauFin : 100;
    var fresh = Math.max(CERT_FRESH_TAUS * tau, CERT_FRESH_MIN_SECS);
    var blind = Math.max(CERT_BLIND_TAUS * tau, fresh * 2);
    if (ageSecs <= fresh) return 1; if (ageSecs >= blind) return 0;
    return 1 - (ageSecs - fresh) / (blind - fresh);
  }
  function witnessOf(m) {
    var clock = (m.blocksAdded > 0) ? 1 : 0;
    var cert = certFreshness(m.certAgeSecs, m.tauFin);
    var channels = Math.max(0, Math.min(1, 1 - (m.missing || 0)));
    /* A "solo" gate is the writer attesting to its own chain, not a quorum of
       witnesses. Capped hard rather than scaled — that is the exact shape the
       gauge was blind to. NOTE: voters COUNTS SIGNERS, NOT FAILURE DOMAINS; on
       2026-09-22 two of three nodes were processes on one host. Upper bound only. */
    var solo = !m.certBft || m.certGate === 'solo' || (m.certVoters || 0) <= 1;
    var indep = solo ? 0 : Math.max(0, Math.min(1, m.certVoters / Math.max(1, Math.max(m.certCommittee || 0, m.certVoters))));
    var senses = [['clock', clock], ['certificate', cert === null ? 0 : cert], ['independence', indep], ['channels', channels]];
    var weakest = senses[0][0], w = Infinity;
    for (var i = 0; i < senses.length; i++) if (senses[i][1] < w) { w = senses[i][1]; weakest = senses[i][0]; }
    var why;
    if (!clock) why = 'the clock is stopped — no block was added in the window, so no disagreement could have been observed. K = 0 here means "nothing was seen", not "all agree".';
    else if (cert === null) why = 'the node serves no finality certificate — unknown is not health.';
    else if (cert === 0) why = 'the finality certificate is frozen (' + (m.certAgeSecs / 3600).toFixed(1) + ' h old' + (m.certLag != null ? ', ' + m.certLag.toLocaleString() + ' blocks behind the tip' : '') + ') while the tip advances. Blocks are settling on the 512-depth fallback, which is a timeout, not a quorum. K = 0 here is the sound of nobody voting.';
    else if (solo) why = 'gate "' + m.certGate + '", bft ' + m.certBft + ', ' + m.certVoters + ' signer(s) — a replicated single writer attesting to its own chain. Absence of conflict here is guaranteed by construction.';
    else why = 'weakest sense is ' + weakest + ' at ' + w.toFixed(2) + '.';
    return {w: w, weakest: weakest, blind: w < WITNESS_BLIND_BELOW, clock: clock, cert: cert, indep: indep, channels: channels, why: why};
  }
  /* The reflex arc. An OBSERVED disagreement is always reported at its own severity —
     a low W never suppresses a high K. But a K that would read "stable" on a blind
     gauge reads "blind" instead, because that is a statement about the instrument. */
  function regimeWitnessed(k, wit) { var r = regimeOf(k); return (r === 'stable' && wit && wit.blind) ? 'blind' : r; }
  // Ω = 1−e^(−peers/n_total), n_total = max(peers+2, 8) — saturates at 0.632, so the labels live inside that range.
  function confidenceOf(om) { return om >= 0.55 ? 'high' : (om >= 0.40 ? 'medium' : 'low'); }
  function fmt(x, d) { return (typeof x === 'number' && isFinite(x)) ? x.toFixed(d === undefined ? 4 : d) : '—'; }
  function rejMap(arr) { var m = {}; (arr || []).forEach(function (r) { if (r && r[0] != null) m[String(r[0])] = (m[String(r[0])] || 0) + ((r[1] | 0) || 0); }); return m; }

  var lastPeers = null;
  // 2026-09-22: the certificate is the sensor this gauge did not have. It sampled
  // recent/miners/topology only, so a finality certificate frozen for 24 h could not
  // move any reading — the gauge was not blind by an arithmetic bug, it was blind
  // because nobody asked. Absent (older node) must read as UNKNOWN, never as healthy.
  function sample() { return Promise.all([getJSON('/v1/dagknight/recent'), getJSON('/v1/mining/miners'), getJSON('/v1/network/topology'), getJSON('/v1/finality/certificate')]); }

  function compute() {
    var t0 = Date.now();
    return sample().then(function (a) {
      return new Promise(function (res) {
        setTimeout(function () { sample().then(function (b) { res([a, b, (Date.now() - t0) / 1000]); }); }, 6000);
      });
    }).then(function (z) {
      var A = z[0], B = z[1], tauW = z[2];
      var recentB = (B[0] && (B[0].data || B[0])) || {}; var all = recentB.blocks || [];
      var minersA = (A[1] && A[1].data) || {}, minersB = (B[1] && B[1].data) || {};
      var topoA = (A[2] && A[2].data) || {}, topoB = (B[2] && B[2].data) || {};
      // ── the certificate (B[3]); absent on an older node -> every field null ──
      var certB = (B[3] && (B[3].certificate || (B[3].data && B[3].data.certificate))) || null;
      var certAgeSecs = (certB && certB.certified_at_ms) ? Math.max(0, (Date.now() - certB.certified_at_ms) / 1000) : null;
      var certVoters = 0;
      if (certB && Array.isArray(certB.votes)) {
        var ids = {}; certB.votes.forEach(function (v) { if (v && v.validator_id) ids[v.validator_id] = 1; });
        certVoters = Object.keys(ids).length || certB.votes.length;
      }
      var hA = minersA.height || 0, hB = minersB.height || 0;
      var inWin = all.filter(function (bk) { return (bk.height || 0) > hA; }); var windowOnly = inWin.length >= 2; var blocks = windowOnly ? inWin : all;
      var nB = blocks.length || 1;
      // ── proposer structure ──
      var counts = {}; blocks.forEach(function (bk) { var p = bk.producer; var key = Array.isArray(p) ? p.slice(0, 6).join('-') : String(p); counts[key] = (counts[key] || 0) + 1; });
      var ds = shannonBits(counts), distinct = Object.keys(counts).length, dominant = 0; for (var k in counts) if (counts[k] > dominant) dominant = counts[k];
      var hNorm = distinct >= 2 ? Math.min(1, Math.max(0, ds / Math.log2(distinct))) : 0; var nEff = Math.pow(2, ds); var domShare = blocks.length ? dominant / nB : 0;
      // ── DAG shape: merges = the frontier had >1 tip; red = colouring refused the block ──
      var merging = blocks.filter(function (bk) { return Array.isArray(bk.merge_parents) && bk.merge_parents.length > 0; }).length;
      var red = blocks.filter(function (bk) { return bk.is_blue === false; }).length;
      // ── rejects by kind, delta-over-delta (both counters are cumulative) ──
      var rA = rejMap(minersA.rejects), rB = rejMap(minersB.rejects), byKind = {}, noise = 0, semantic = 0, uncl = 0;
      for (var kk in rB) { var d = Math.max(0, rB[kk] - (rA[kk] || 0)); if (d > 0) { var c = rejectClass(kk); byKind[kk] = { delta: d, cls: c }; if (c === 'noise') noise += d; else if (c === 'semantic') semantic += d; else uncl += d; } }
      var subDelta = Math.max(0, (minersB.shares_accepted || 0) - (minersA.shares_accepted || 0)); var denom = subDelta + noise + semantic + uncl;
      var rate = function (x) { return denom > 0 ? x / denom : 0; };
      // ── peers (diagnostic) + peer heights (finality channel) ──
      var peersA = (topoA.peer_count != null ? topoA.peer_count : (lastPeers != null ? lastPeers : 0));
      var peersB = (topoB.peer_count != null ? topoB.peer_count : peersA); lastPeers = peersB;
      var churn = (peersA > 0) ? Math.abs(peersB - peersA) / peersA : 0;
      var ph = []; var phm = topoB.peer_heights || {}; for (var pk in phm) { var hv = phm[pk]; if (pk !== 'last' && typeof hv === 'number' && hv > 0) ph.push(hv); }
      // state-root verdicts: the node compared each peer's wallet_state_root (at the peer's tip height) with its own block there
      var sv = []; var pvm = topoB.peer_views || {}; for (var pv in pvm) { var m = pvm[pv] && pvm[pv].state_root_matches_local; if (m === true || m === false) sv.push(m); }
      // ── the four channels ──
      var dTip = merging / nB;
      var dConf = Math.min(1, red / nB + rate(semantic));
      // peers publish their SETTLED chain.height(); compare with OUR settled height (local_view), never the mining frontier (~512 ahead)
      var hLocal = (topoB.local_view && typeof topoB.local_view.height === 'number') ? topoB.local_view.height : hB;
      // a peer > 2 finality-depths away is CATCHING UP, not disagreeing about what is final: shown, excluded
      var conv = ph.filter(function (h) { return Math.abs(h - hLocal) <= 2 * FINAL_DEPTH; }); var syncing = ph.length - conv.length;
      var dFin = conv.length ? conv.reduce(function (s, h) { return s + Math.min(1, Math.abs(h - hLocal) / FINAL_DEPTH); }, 0) / conv.length : null;
      var dState = sv.length ? sv.filter(function (ok) { return !ok; }).length / sv.length : null;
      var ch = [
        { n: 'tip divergence', w: W.tip, v: dTip, b: merging + '/' + blocks.length + ' merge-parent blocks' },
        { n: 'state-root mismatch', w: W.state, v: dState, b: sv.length ? (sv.filter(function (ok) { return !ok; }).length + '/' + sv.length + ' comparable peer(s) disagree') : 'no comparable peer view (peers must run node ≥ 2026-09-07)' },
        { n: 'finality divergence', w: W.fin, v: dFin, b: conv.length ? conv.length + ' converged peer(s), spine gap ' + conv.map(function (h) { return Math.abs(h - hLocal); }).join('/') + ' blk ÷ 512' + (syncing ? ' · ' + syncing + ' syncing (excluded)' : '') : (syncing ? syncing + ' peer(s) still syncing — catching up is not disagreement' : 'no peer heartbeat yet (node ≥ 2026-09-07, sigil-top ≥ 8.0.11 publish one)') },
        { n: 'semantic conflicts', w: W.conf, v: dConf, b: red + ' red blk + ' + semantic + ' verify_mismatch' }
      ];
      var dh = 0, missing = 0; ch.forEach(function (c) { if (c.v == null) missing += c.w; else dh += c.w * c.v; });
      // ── τ = real convergence time (K_C only) ──
      var obsBps = (tauW > 0) ? Math.max(0, (hB - hA)) / tauW : 0; var tauFin = obsBps > 0 ? FINAL_DEPTH / obsBps : tauW; var tauRatio = tauFin / TAU0;
      // ── Ω → confidence band ──
      var nTotal = Math.max(peersB + 2, 8); var omega = 1 - Math.exp(-peersB / nTotal);
      var seen = Math.min(1, dh + missing + rate(uncl) * W.conf); var dHi = Math.min(1, seen + (1 - omega) * (1 - seen)); var dLo = dh * omega;
      var K = kC(dh, tauRatio, hNorm), Klo = kC(dLo, tauRatio, hNorm), Khi = kC(dHi, tauRatio, hNorm);
      // ── K_fix: persistence in blocks, entropy as a bounded weight ──
      var gapBlk = conv.length ? conv.reduce(function (s, h) { return s + Math.abs(h - hLocal); }, 0) / conv.length : 0;
      var persist = Math.min(FINAL_DEPTH, gapBlk + merging + red);
      var Kfix = kFix(dh, persist, hNorm), KfixHi = kFix(dHi, persist, hNorm), KfixLo = kFix(dLo, persist, hNorm);
      // ── legacy v1 (footer only, labelled) ──
      var dhV1 = rate(noise + semantic + uncl) + churn; var KstarV1 = 2 * Math.PI * Math.sqrt(dhV1 * tauW * ds);
      var dom = dh > 0 ? 'state disagreement (ΔHc)' : (ds > 0 ? 'proposer plurality (Δs) — not disagreement' : 'none (quiet chain)');
      return { K: K, Klo: Klo, Khi: Khi, Kfix: Kfix, KfixHi: KfixHi, KfixLo: KfixLo, persist: persist, gapBlk: gapBlk, merging: merging, red: red,
        tauD: tauD(persist), wS: wS(hNorm), dh: dh, missing: missing, ch: ch, ds: ds, hNorm: hNorm, nEff: nEff, domShare: domShare, distinct: distinct,
        resBits: resolutionBits(blocks.length), tauFin: tauFin, tauRatio: tauRatio, tauW: tauW, obsBps: obsBps, omega: omega, conf: confidenceOf(omega),
        peers: peersB, churn: churn, rejRate: rate(noise + semantic + uncl), rejSem: rate(semantic), noise: noise, semantic: semantic, uncl: uncl, byKind: byKind,
        KstarV1: KstarV1, dhV1: dhV1, dom: dom, tip: hB, blocks: blocks.length, windowOnly: windowOnly, svLen: sv.length, phLen: ph.length, syncing: syncing,
        blocksAdded: Math.max(0, hB - hA), certAgeSecs: certAgeSecs, certHeight: certB ? certB.height : null,
        certLag: (certB && certB.height != null) ? Math.max(0, hB - certB.height) : null,
        certVoters: certVoters, certCommittee: certB ? (certB.committee_size || 0) : 0,
        certBft: certB ? !!certB.bft : false, certGate: certB ? (certB.gate || 'unknown') : 'absent' };
    });
  }

  function cell(lab, sub, val, note, na) { return '<div class="kgCell"><div class="kgLab">' + lab + ' · ' + sub + '</div><div class="kgVal' + (na ? ' kgNa' : '') + '">' + val + '</div><div class="kgSub">' + note + '</div></div>'; }
  function colOf(reg) { return reg === 'critical' ? '#f87171' : reg === 'elevated' ? '#fbbf24' : '#34d399'; }

  function render(m) {
    /* board contract (2026-09-14): the last reading is published for other pages, offline included */
    try {
      var wl = m ? witnessOf(m) : null;
      window.__kgLast = m ? {t: Date.now(), Kfix: m.Kfix, KfixLo: m.KfixLo, KfixHi: m.KfixHi, K: m.K, regime: regimeWitnessed(m.Kfix, wl), regimeC: regimeWitnessed(m.K, wl), regimeUnwitnessed: regimeOf(m.Kfix), W: wl.w, blind: wl.blind, weakestSense: wl.weakest, witnessWhy: wl.why, dh: m.dh, missing: m.missing, peers: m.peers, tip: m.tip, tauW: m.tauW} : {t: Date.now(), offline: true};
      document.dispatchEvent(new CustomEvent('kgauge', {detail: window.__kgLast}));
    } catch (e) {}
    var cr = document.getElementById('kgChipReg');
    if (!m) { cr.textContent = 'node offline'; cr.className = 'kgReg kg-unknown'; return; }
    var wit = witnessOf(m);
    var reg = regimeWitnessed(m.Kfix, wit), regC = regimeWitnessed(m.K, wit);
    document.getElementById('kgChipK').textContent = fmt(m.Kfix, 2);
    cr.textContent = reg; cr.className = 'kgReg kg-' + reg;
    cr.title = wit.blind
      ? ('BLIND — W = ' + wit.w.toFixed(2) + ' (weakest sense: ' + wit.weakest + '). ' + wit.why +
         '\nThis reading is about the instrument, not the chain: K = ' + fmt(m.Kfix, 2) + ' would have printed "' + regimeOf(m.Kfix) + '".')
      : ('witnessed — W = ' + wit.w.toFixed(2) + ' (weakest sense: ' + wit.weakest + '). A K near zero here is evidence of agreement, not of silence.');
    document.getElementById('kgChipKc').innerHTML = 'K<sub>C</sub> ' + fmt(m.K, 2);
    var bk = document.getElementById('kgBigK'); bk.textContent = fmt(m.Kfix, 3); bk.style.color = colOf(reg);
    document.getElementById('kgStar').innerHTML = 'K<sub>C</sub> ' + fmt(m.K, 3) + ' [' + fmt(m.Klo, 2) + ', ' + fmt(m.Khi, 2) + '] · ' + regC;
    document.getElementById('kgRegimeTxt').innerHTML = wit.blind
      ? ('<span style="color:#cbd5e1">blind</span> — W = ' + wit.w.toFixed(2) + ', weakest sense <b>' + wit.weakest + '</b>. ' + wit.why +
         ' The gauge could not have seen a disagreement, so K<sub>fix</sub> = ' + fmt(m.Kfix, 2) + ' is not evidence of agreement. Absence of conflict is not presence of consensus.')
      : ('<span style="color:' + colOf(reg) + '">' + reg + '</span> (K<sub>fix</sub> &lt;1 stable · 1–3 elevated · ≥3 critical; K<sub>C</sub> reads ' + regC + ' on the same ladder) · witnessed, W = ' + wit.w.toFixed(2));
    document.getElementById('kgDHc').textContent = '= ' + fmt(m.dh, 4) + ' (lower bound, ' + Math.round(m.missing * 100) + '% unmeasured)';
    document.getElementById('kgDom').textContent = m.dom;
    document.getElementById('kgGridState').innerHTML = m.ch.map(function (c) { return cell(c.n, 'w ' + c.w.toFixed(2), c.v == null ? 'n/a' : fmt(c.v, 3), c.b, c.v == null); }).join('');
    document.getElementById('kgGridProp').innerHTML =
      cell('<i>d</i>', 'persistence, blocks', fmt(m.persist, 0) + ' blk', 'spine gap ' + fmt(m.gapBlk, 0) + ' + ' + m.merging + ' merge + ' + m.red + ' red · capped at 512') +
      cell('<i>τ<sub>d</sub></i>', '(1 + d/512)/2', fmt(m.tauD, 3), 'persistence factor ½ … 1 — blocks, not seconds') +
      cell('entropy', '<i>Δs</i>', fmt(m.ds, 3) + ' bits', 'step ' + fmt(m.resBits, 4) + ' over ' + m.blocks + ' blk · N_eff ' + fmt(m.nEff, 2)) +
      cell('<i>w<sub>S</sub></i>', '1 − H_norm/4', fmt(m.wS, 3), 'H_norm ' + fmt(m.hNorm, 3) + ' · ' + m.distinct + ' producer(s), dominant ' + (m.domShare * 100).toFixed(1) + '%') +
      cell('<i>τ</i>', 'finality time (K_C only)', fmt(m.tauFin, 0) + ' s', '512 ÷ ' + fmt(m.obsBps, 2) + '/s · τ/τ₀ ' + fmt(m.tauRatio, 2) + ' — this is why K_C moves with block rate') +
      cell('K<sub>fix</sub> band', 'low … high', fmt(m.KfixLo, 2) + ' … ' + fmt(m.KfixHi, 2), 'unmeasured weight ' + Math.round(m.missing * 100) + '% + unseen network');
    var kinds = Object.keys(m.byKind).map(function (k) { return k + ' ' + m.byKind[k].delta + ' (' + m.byKind[k].cls + ')'; }).join(', ') || 'none in window';
    document.getElementById('kgGridNet').innerHTML =
      cell('<i>Ω</i>', 'observer coverage', fmt(m.omega, 3), m.conf + ' confidence · 1−e^(−peers/n)') +
      cell('peers', 'count', String(m.peers), 'churn ' + fmt(m.churn, 3) + (m.syncing ? ' · ' + m.syncing + ' syncing' : '') + ' — topology, not disagreement') +
      cell('block rate', 'observed', fmt(m.obsBps, 2) + ' /s', 'sampled over ' + fmt(m.tauW, 1) + ' s') +
      cell('rejects', 'by kind', m.semantic + ' sem / ' + m.noise + ' noise', kinds);
    document.getElementById('kgProv').innerHTML =
      '<b style="color:#c084fc">Provenance.</b> ' +
      '<span class="kgMeas">measured</span>: proposer entropy, DAG merge-parents (tip divergence), red blocks + verify_mismatch (semantic conflicts), block rate, rejects by kind, peers/churn (diagnostic). ' +
      (m.svLen ? '<span class="kgMeas">measured</span>: state-root agreement over ' + m.svLen + ' peer(s). ' : '<span class="kgUnav">unavailable</span>: state-root agreement — no connected peer sent a comparable heartbeat (sigil-node ≥ 2026-09-07 publishes one; miners do not), so that channel widens the upper bound rather than reading 0. ') +
      (m.phLen ? '<span class="kgMeas">measured</span>: finality gap over ' + m.phLen + ' peer height(s). ' : '<span class="kgUnav">unavailable</span>: finality gap (no peer heights). ') +
      (m.windowOnly ? 'Blocks counted are exactly those produced inside the window. ' : 'Fewer than 2 blocks landed in the window; DAG channels use the recent-block feed. ') +
      'Identical arithmetic to the wallet chip and the flux_sigil_kgauge MCP tool (K_fix / K_C columns). ' +
      '<span style="color:rgba(255,255,255,.35)">Legacy v1 K* (reject + churn detector, retired 2026-09-14, not a Margolus–Levitin number): ' + fmt(m.KstarV1, 3) + ' with ΔH ' + fmt(m.dhV1, 3) + '.</span>';
    (function(){ var f = document.getElementById('kgFort'); if (!f) return;
      var band = m.Kfix < 1 ? 'the nodes agree and nothing persists' : m.Kfix < 3 ? 'a disagreement is showing in one channel or persisting — watch the tip and state-root cells' : 'a real split — check the tip and state-root channels, then look for reject spam or a stuck follower';
      f.innerHTML = '<b style="color:#c084fc">K<sub>fix</sub> — fortolkning.</b> Today ' + fmt(m.Kfix, 2) + ' (' + reg + '): ' + band + '. K<sub>fix</sub> = 2π√(ΔH<sub>c</sub>·τ<sub>d</sub>·w<sub>S</sub>): disagreement between this node and its peers over four channels, persistence in blocks, proposer entropy as a ¾–1 weight. Not a speed gauge; not fooled by one key wearing many names; a missing channel widens the bound, it never reads zero. <span style="color:#c084fc">Dansk:</span> hvor meget noderne er uenige om, hvad der er sandt — over 3 er en reel splittelse. Earth\'s K⊕ sits beside this on <a href="/kristensen-board.html" style="color:#c084fc">the board</a>.'; })();
    document.getElementById('kgFoot').textContent =
      'tip ' + m.tip + ' · ' + m.peers + ' peers · ' + m.blocks + ' blocks · sampled over ' + fmt(m.tauW, 1) + ' s · gauge v3 (K_fix + K_C)';
  }

  var busy = false, haveReading = false;
  function refresh() {
    if (busy) return; busy = true;
    // Only claim "measuring" when there is nothing to show: blanking a good badge for the 6 s of
    // the next window made a healthy chain read as unmeasured every time someone looked at it.
    if (!haveReading) document.getElementById('kgChipReg').textContent = 'measuring…';
    compute().then(function (m) { haveReading = !!m; render(m); busy = false; })
             .catch(function (e) { try { console.error('kgauge v3 refresh failed:', e && (e.stack || e.message || e)); } catch (_) {} busy = false; haveReading = false; render(null); });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', install);
  else install();
})();
