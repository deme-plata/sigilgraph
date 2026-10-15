/* Shared hosted + [W] wallet exchange. No wallet keys, router API or dependencies. */
(function () {
  'use strict';
  var MM = window.SigilMM;
  if (!MM || document.getElementById('exchange-modal')) return;
  var W = '0x3fced760b0de6d57f96835c6110b1227941ec2e9';
  var U = '0x3c499c542cef5e3811e1192ce70d8cc03d5c3359';
  var PAIR = '0x7e9c5d73104ed2fc2bc55139ac0999036eaf41d7';
  var S = { sell: false, busy: false, quote: null, version: 0, balances: null, returnFocus: null };
  var $ = function (id) { return document.getElementById(id); };
  function el(tag, cls, text) { var e = document.createElement(tag); e.className = cls || ''; if (text) e.textContent = text; return e; }
  function esc(s) { return String(s).replace(/[&<>"']/g, function (c) { return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]; }); }
  function units(s, dp) {
    s = String(s).trim().replace(',', '.');
    if (!/^\d+(\.\d+)?$/.test(s) || (s.split('.')[1] || '').length > dp) throw new Error('Use a positive amount with at most ' + dp + ' decimals.');
    var p = s.split('.'); return BigInt(p[0]) * 10n ** BigInt(dp) + BigInt((p[1] || '').padEnd(dp, '0') || '0');
  }
  function fmt(n, dp, keep) { var d = 10n ** BigInt(dp), f = (n % d).toString().padStart(dp, '0').slice(0, keep || dp).replace(/0+$/, ''); return (n / d).toString() + (f ? '.' + f : ''); }
  function path() { return S.sell ? [W, U] : [U, W]; }
  function syms() { return S.sell ? ['WSIGIL3', 'USDC'] : ['USDC', 'WSIGIL3']; }
  function dps() { return S.sell ? [18, 6] : [6, 18]; }
  function say(s, bad) { $('exMessage').textContent = s; $('exMessage').classList.toggle('ex-error', !!bad); }
  function row(k, id) { return '<div class="ex-row"><span>' + k + '</span><strong id="' + id + '">—</strong></div>'; }
  var css = document.createElement('style'); css.textContent = `
    #exchange-modal,.xc-ov#xchain-modal{padding:32px!important;background:rgba(3,6,12,.86)!important;backdrop-filter:blur(18px)!important;z-index:100010!important}#ask-modal{z-index:100060!important}#mmToast{z-index:100070!important}
    #swap-modal{z-index:100010!important}#swap-modal>div{width:min(900px,100%)!important;max-width:900px!important;max-height:calc(100dvh - 36px)!important}#swap-modal #swapForm{padding:26px!important;gap:20px!important}#swap-modal #swapAmt,#swap-modal #swapOut{font-size:28px!important}
    .ex-dialog,#xchain-modal .xc-box{box-sizing:border-box!important;width:min(1100px,100%)!important;max-width:1100px!important;max-height:calc(100dvh - 64px)!important;border:1px solid #30354c!important;border-radius:26px!important;background:#0d111c!important;color:#e8ecf8;font-family:Inter,'Segoe UI',sans-serif!important;box-shadow:0 40px 130px #0009!important;overflow:hidden!important}
    .ex-dialog{display:grid;grid-template-columns:280px minmax(0,1fr)}
    .ex-aside{padding:32px 26px;background:radial-gradient(ellipse at 0 0,#7562ba25,transparent 70%),#111522;border-right:1px solid #ffffff0c;display:flex;flex-direction:column;gap:24px;min-width:0}
    .ex-brand{font-size:12px;letter-spacing:2px;font-weight:750;color:#b9adff}.ex-aside h2{font-size:32px;letter-spacing:-1.2px;line-height:1.14;margin:0;font-weight:600}.ex-aside p{font-size:13px;line-height:1.7;color:#a0aac1;margin:0}
    .ex-aside nav{display:grid;gap:8px}.ex-nav{border:1px solid transparent;background:transparent;color:#a0aac1;text-align:left;border-radius:12px;padding:14px;font:600 14px Inter,'Segoe UI',sans-serif;cursor:pointer}.ex-nav[aria-current=page]{background:#a995ff16;border-color:#a995ff40;color:#ddd5ff}.ex-nav small{display:block;color:#8895ad;font-weight:400;font-size:11px;margin-top:5px}
    .ex-network{margin-top:auto;padding:16px;border:1px solid #ffffff12;border-radius:12px;font-size:12px;color:#a0aac1;line-height:1.7}.ex-network b{display:block;color:#ddd5ff;font-weight:500}.ex-network a{color:#b9adff}
    .ex-main{overflow-y:auto;padding:30px 34px;min-width:0}.ex-top{display:flex;justify-content:space-between;align-items:center;margin-bottom:24px}.ex-top h1{font-size:23px;letter-spacing:-.5px;margin:0;font-weight:600}.ex-close{border:1px solid #ffffff18;background:#ffffff05;color:#b6bfd3;border-radius:50%;width:36px;height:36px;font-size:20px;cursor:pointer}.ex-label{font-size:11px;letter-spacing:1px;text-transform:uppercase;color:#a3aec5}
    .ex-segment{display:flex;gap:6px;padding:5px;background:#080c15;border:1px solid #ffffff0c;border-radius:12px;margin-bottom:20px}.ex-segment button{flex:1;padding:11px;border:0;border-radius:8px;background:transparent;color:#919db7;cursor:pointer;font-size:13px}.ex-segment button[aria-pressed=true]{background:#23233b;color:#e6ddff}
    .ex-token{background:#141b29;border:1px solid #ffffff12;border-radius:17px;padding:20px}.ex-token-head{display:flex;justify-content:space-between;color:#9eabc3;font-size:12px;gap:8px}.ex-token-body{display:flex;align-items:center;gap:12px;margin-top:15px}.ex-token-body input,.ex-token-body output{min-width:0;width:100%;background:none;border:0;color:#eef1fc;font-size:34px;letter-spacing:-1px;outline:none;font-family:inherit}.ex-symbol{flex:none;background:#26213c;border:1px solid #b9adff28;color:#e1d9ff;padding:10px 12px;border-radius:24px;font-size:13px;white-space:nowrap}.ex-percents{display:flex;gap:6px;margin-top:14px}.ex-percents button{padding:6px 10px;border-radius:6px;background:#ffffff06;border:1px solid #ffffff12;color:#b5c0d7;cursor:pointer;font-size:11px}
    .ex-flip{display:block;position:relative;margin:-8px auto;z-index:1;width:34px;height:34px;border:4px solid #0d111c;border-radius:11px;background:#302944;color:#c4b5fd;cursor:pointer;font-size:18px}.ex-settings{display:flex;justify-content:space-between;align-items:center;font-size:12px;color:#a3aec5;margin:20px 0 8px}.ex-settings select{background:#151a2a;color:#ddd5ff;border:1px solid #ffffff20;padding:7px;border-radius:8px}.ex-rows{margin-top:8px}.ex-row{display:flex;justify-content:space-between;gap:20px;font-size:12px;line-height:1.6;padding:7px 0;color:#939fba}.ex-row strong{font-weight:500;color:#d7dff1;text-align:right;overflow-wrap:anywhere}.ex-risk{font-size:12px;line-height:1.65;color:#dec18e;border:1px solid #8b682b50;background:#bd86300c;border-radius:10px;padding:12px;margin-top:12px}.ex-action{width:100%;padding:16px;border:1px solid #c4b5fd40;border-radius:12px;background:#bba7f1;color:#181226;font-size:14px;font-weight:700;margin-top:18px;cursor:pointer}.ex-action:disabled{opacity:.4;cursor:not-allowed}.ex-message{font-size:12px;color:#b5c5dd;line-height:1.7;overflow-wrap:anywhere;min-height:22px;margin-top:12px}.ex-error{color:#ffa5b2}.ex-foot{font-size:11px;color:#7e8ba6;line-height:1.7;margin-top:18px}.ex-link{border:0;background:none;color:#bfaeff;text-decoration:underline;cursor:pointer;padding:0;font:inherit}.ex-receipt{padding:18px;background:#111e22;border:1px solid #70d8b545;border-radius:14px;margin-top:16px;font-size:13px;line-height:1.8;overflow-wrap:anywhere}.ex-receipt a{color:#9ee4cd}.ex-progress{display:flex;gap:8px;flex-wrap:wrap;margin-top:14px;color:#8693ae;font-size:11px}.ex-progress span{padding:7px 10px;border:1px solid #ffffff10;border-radius:6px}.ex-progress .on{color:#d9ccff;border-color:#b9adff60;background:#b9adff10}
    #exchange-modal :focus-visible,#xchain-modal :focus-visible{outline:2px solid #b9adff!important;outline-offset:4px}#exchange-modal button:disabled{cursor:not-allowed}
    #xchain-modal .xc-box{display:grid!important;grid-template-columns:280px minmax(0,1fr);grid-template-rows:auto minmax(0,1fr)}#xchain-modal .ex-aside{grid-row:1/3;grid-column:1}#xchain-modal .xc-hd{grid-column:2;padding:25px 32px!important;border-bottom:1px solid #ffffff10;background:none}#xchain-modal .xc-ttl{font:600 23px Inter,'Segoe UI',sans-serif;letter-spacing:-.5px;color:#e8ecf8}#xchain-modal .xc-sub{font:12px Inter,'Segoe UI',sans-serif;color:#9aa6bf;margin-top:6px}#xchain-modal #xcForm,#xchain-modal #xcReceipt{grid-column:2;padding:24px 32px!important;min-height:0;gap:18px!important}#xchain-modal .xc-rail{flex-shrink:0}#xchain-modal .xc-half{padding:14px 8px}#xchain-modal .xc-half .n{font:600 12px Inter,'Segoe UI',sans-serif}#xchain-modal .xc-half .s{font:11px Inter,'Segoe UI',sans-serif;margin-top:6px}#xchain-modal .xc-knob{background:#a995ff18;border-color:#a995ff50}#xchain-modal .xc-lbl{font:12px Inter,'Segoe UI',sans-serif;letter-spacing:.3px;color:#b8c0d7}#xchain-modal .xc-amt{font:32px Inter,'Segoe UI',sans-serif;min-height:65px;background:#141b29}#xchain-modal .xc-in{font:13px Inter,'Segoe UI',sans-serif;padding:16px 12px;background:#141b29}#xchain-modal .xc-warn{font:12px/1.75 Inter,'Segoe UI',sans-serif;color:#d6bb8c;border-color:#8b682b50;background:#bd86300c}#xchain-modal .xc-go{font:700 14px Inter,'Segoe UI',sans-serif;background:#bba7f1;color:#181226;border-color:#c4b5fd40;padding:16px}#xchain-modal .xc-mm{padding:14px;background:#141b29}#xchain-modal .xc-pct{padding:9px}#xchain-modal .xc-msg{font:12px/1.7 Inter,'Segoe UI',sans-serif}#swap-modal #wmWrap{display:none!important}
    @media(max-width:760px){#exchange-modal,.xc-ov#xchain-modal{padding:10px!important}.ex-dialog,#xchain-modal .xc-box{max-height:calc(100dvh - 20px)!important;border-radius:18px!important;grid-template-columns:minmax(0,1fr)!important}.ex-aside{padding:16px 20px;border-right:0;border-bottom:1px solid #ffffff12;gap:12px}.ex-aside h2,.ex-aside>p,.ex-network{display:none}.ex-aside nav{display:flex;gap:8px}.ex-nav{flex:1;padding:10px}.ex-nav small{font-size:10px}.ex-main{padding:22px 20px}.ex-token{padding:16px}.ex-token-body input,.ex-token-body output{font-size:27px}.ex-symbol{font-size:11px;padding:9px}#xchain-modal .ex-aside{grid-column:1;grid-row:1}#xchain-modal .xc-hd{grid-column:1;grid-row:2;padding:18px 20px!important}#xchain-modal #xcForm,#xchain-modal #xcReceipt{grid-column:1;grid-row:3;padding:20px!important}#xchain-modal .xc-box{grid-template-rows:auto auto minmax(0,1fr)}}
    @media(prefers-reduced-motion:reduce){#exchange-modal *,#xchain-modal *{animation:none!important;transition:none!important}}
  `; document.head.appendChild(css);
  function aside(active) {
    var a = el('aside', 'ex-aside'); a.innerHTML = '<div class="ex-brand">◈ SIGIL / EXCHANGE</div><h2>Your assets.<br>Across networks.</h2><p>Move native SIGIL to Polygon, or trade wrapped SIGIL against USDC.</p><nav><button class="ex-nav" data-ex-route="swap" '+(active==='swap'?'aria-current="page"':'')+'>⇄ &nbsp; Swap<small>WSIGIL3 / USDC · Polygon</small></button><button class="ex-nav" data-ex-route="bridge" '+(active==='bridge'?'aria-current="page"':'')+'>↗ &nbsp; Bridge<small>Native SIGIL ↔ wrapped SIGIL</small></button></nav><div class="ex-network"><b>Polygon mainnet · 137</b>Gas is paid in POL.<br><a href="https://polygonscan.com/token/'+W+'" target="_blank" rel="noopener noreferrer">Inspect WSIGIL3 ↗</a><br>Bridge and market contracts can differ. Check the destination token.</div>';
    a.querySelector('[data-ex-route="swap"]').onclick = function () { if (S.busy || bridgeBusy) return; window.closeXChain(); open(); };
    a.querySelector('[data-ex-route="bridge"]').onclick = function () { if (S.busy || bridgeBusy) return; close(); window.openXChain('to_polygon'); };
    return a;
  }
  var m = el('div'); m.id = 'exchange-modal'; m.style.cssText = 'display:none;position:fixed;inset:0;z-index:10008;align-items:center;justify-content:center';
  m.innerHTML = '<div class="ex-dialog" role="dialog" aria-modal="true" aria-labelledby="exTitle"><main class="ex-main"><div class="ex-top"><h1 id="exTitle">Swap on Polygon</h1><button class="ex-close" id="exClose" aria-label="Close swap">×</button></div><div class="ex-segment"><button id="exBuy" aria-pressed="true">Buy WSIGIL3</button><button id="exSell" aria-pressed="false">Sell WSIGIL3</button></div><div class="ex-token"><div class="ex-token-head"><label for="exAmount">You pay</label><span id="exBalance">Connect to see balance</span></div><div class="ex-token-body"><input id="exAmount" inputmode="decimal" autocomplete="off" placeholder="0.00" aria-describedby="exMessage"><span class="ex-symbol" id="exIn">USDC</span></div><div class="ex-percents"><button data-pct="25">25%</button><button data-pct="50">50%</button><button data-pct="100">Max</button></div></div><button class="ex-flip" id="exFlip" aria-label="Reverse swap direction">↓</button><div class="ex-token"><div class="ex-token-head"><span>You receive · estimate</span><span>Polygon</span></div><div class="ex-token-body"><output id="exOutput">—</output><span class="ex-symbol" id="exOut">WSIGIL3</span></div></div><div class="ex-settings"><span>Slippage tolerance</span><select id="exSlippage" aria-label="Slippage tolerance"><option value="50">0.5%</option><option value="100" selected>1.0%</option><option value="200">2.0%</option></select></div><div class="ex-rows">'+row('Minimum received','exMin')+row('Price impact','exImpact')+row('Pool liquidity · estimated USD','exLiquidity')+row('Liquidity provider fee','exFee')+'</div><div class="ex-risk" id="exRisk">Reading the current WSIGIL3 pool on Polygon…</div><button class="ex-action" id="exGo">Connect wallet</button><div class="ex-progress" aria-label="Transaction steps"><span id="exStep1">1 · Review</span><span id="exStep2">2 · Approve</span><span id="exStep3">3 · Swap</span><span id="exStep4">4 · Confirm</span></div><div class="ex-message" id="exMessage" role="status" aria-live="polite"></div><div id="exReceipt" class="ex-receipt" hidden></div><div class="ex-foot">Uniswap V2 · Polygon mainnet · POL required for gas.<br>Native SIGIL lives on its own network. <button class="ex-link" id="exBridgeLink">Open bridge ↗</button> &nbsp;·&nbsp; <button class="ex-link" id="exNative">Native pools ↗</button></div></main></div>';
  m.firstChild.prepend(aside('swap')); document.body.appendChild(m);
  var legacyOpen = window.openSwap;
  var nativeHeading=$('swapClose')&&$('swapClose').previousElementSibling;
  if(nativeHeading&&nativeHeading.children.length===2){nativeHeading.firstElementChild.textContent='Native SIGIL swap';nativeHeading.lastElementChild.textContent='Native network · available pools load from the node';}
  function invalidate() { S.version++; S.quote = null; $('exReceipt').hidden = true; $('exOutput').textContent = '—'; $('exMin').textContent = '—'; $('exImpact').textContent = '—'; $('exFee').textContent = '0.30%'; $('exGo').textContent = MM.getState().connected ? 'Enter an amount' : 'Connect wallet'; $('exGo').disabled = MM.getState().connected; }
  function step(n) { for (var i=1;i<=4;i++) $('exStep'+i).classList.toggle('on', i===n); }
  function setSide(sell) {
    if (S.busy) return; S.sell = sell; $('exAmount').value = ''; $('exIn').textContent = syms()[0]; $('exOut').textContent = syms()[1];
    $('exBuy').setAttribute('aria-pressed', String(!sell)); $('exSell').setAttribute('aria-pressed', String(sell)); invalidate(); say(''); balance();
  }
  async function balance() {
    var state = MM.getState(), acct = state.account, sell = S.sell;
    S.balances = null; $('exBalance').textContent = acct ? 'Reading balance…' : 'Connect to see balance';
    if (!acct) return;
    try {
      var value = BigInt(await MM.rawCall(sell ? W : U, '0x70a08231'+acct.slice(2).padStart(64,'0')));
      if (MM.getState().account !== acct || S.sell !== sell) return;
      S.balances = value; $('exBalance').textContent = 'Balance '+fmt(value, dps()[0], 8);
    } catch (_) { $('exBalance').textContent = 'Balance unavailable'; }
  }
  async function quote() {
    invalidate(); var version = S.version, dp = dps(), pp = path(), sym = syms(), amt;
    try { amt = units($('exAmount').value, dp[0]); if (amt<=0n) throw new Error('Enter an amount greater than zero.'); }
    catch (e) { if ($('exAmount').value) say(e.message, true); else say(''); return; }
    say('Fetching a fresh Polygon quote…');
    try {
      var all = await Promise.all([MM.uniQuote(amt, pp), MM.rawCall(PAIR,'0x0902f1ac'), MM.rawCall(PAIR,'0x0dfe1681')]);
      if (version !== S.version || S.busy) return;
      var out = all[0][1], b = all[1].slice(2), r0=BigInt('0x'+b.slice(0,64)), r1=BigInt('0x'+b.slice(64,128));
      var token0='0x'+all[2].slice(-40).toLowerCase(), rU=token0===U?r0:r1, rW=token0===U?r1:r0;
      if(token0!==U&&token0!==W)throw new Error('Unexpected pool token. Do not sign a swap.');
      if (out<=0n || rU<=0n || rW<=0n) throw new Error('The pool cannot fill this amount.');
      var rIn=S.sell?rW:rU, rOut=S.sell?rU:rW;
      var impact=Math.max(0, 100*(1-Number(out*rIn)/Number(amt*rOut)));
      var slip=Number($('exSlippage').value), min=out*BigInt(10000-slip)/10000n;
      if(min<=0n) throw new Error('Amount is too small to protect with a minimum output.');
      S.quote={amt:amt,out:out,min:min,slip:slip,path:pp,dp:dp,sym:sym,impact:impact,time:Date.now(),version:version};
      $('exOutput').textContent=fmt(out,dp[1],12); $('exMin').textContent=fmt(min,dp[1],12)+' '+sym[1]; $('exImpact').textContent=impact.toFixed(2)+'%';
      $('exLiquidity').textContent='$'+(Number(rU)/1e6*2).toFixed(6);
      $('exRisk').textContent=impact>=15?'High price impact. This amount consumes a substantial part of the pool. Reduce the amount before swapping.':'Small pool: your trade changes the exchange rate. The estimate includes the 0.30% pool fee; Polygon gas is additional.';
      $('exGo').disabled=impact>=15; $('exGo').textContent=impact>=15?'Reduce amount · impact above 15%':MM.getState().connected?'Review swap':'Connect wallet';
      say('Quote ready · expires after 30 seconds.');
    } catch(e) { if(version===S.version) { say('Quote unavailable: '+e.message,true); $('exGo').textContent='Refresh quote'; $('exGo').disabled=false; } }
  }
  function lock(on) { S.busy=on; m.querySelectorAll('input,select,button').forEach(function(e){e.disabled=on;}); $('exGo').disabled=on; }
  async function go() {
    if(S.busy||S.executing) return; S.executing=true;
    try {
      if(!MM.getState().connected) { await MM.connect(); await balance(); await quote(); return; }
      if(!MM.getState().onPolygon) { await MM.ensurePolygon(); await balance(); await quote(); return; }
      var q=S.quote;
      if(!q || Date.now()-q.time>30000) { await quote(); say('Quote refreshed. Review the current minimum, then continue.'); return; }
      if(q.impact>=15) return;
      if(S.balances===null) { await balance(); if(S.balances===null) throw new Error('Could not verify your token balance. Retry when Polygon is reachable.'); }
      if(q.amt>S.balances) throw new Error('Amount exceeds your '+q.sym[0]+' balance.');
      lock(true); step(1); var acct=MM.getState().account;
      var ok=await window.sigilConfirm('Swap on Polygon mainnet\n\nPay: '+fmt(q.amt,q.dp[0])+' '+q.sym[0]+'\nReceive at least: '+fmt(q.min,q.dp[1])+' '+q.sym[1]+'\nPrice impact: '+q.impact.toFixed(2)+'%\nSlippage: '+q.slip/100+'%\n\nApprove the exact token amount if needed, then confirm the swap. POL gas is additional.');
      if(!ok) { say('Review cancelled. No swap requested.'); return; }
      if(q.version!==S.version || acct!==MM.getState().account) throw new Error('Wallet or quote changed. Review a fresh quote.');
      $('exGo').textContent='Waiting for wallet…';
      var r=await MM.uniSwap(q.amt,q.path,q.slip,function(s,tx){
        if(s==='approve') { step(2); say('Confirm the exact token approval in your wallet.'); }
        if(s==='approve-sent') say('Approval submitted. Waiting for Polygon confirmation.');
        if(s==='swap') {step(3);say('Approval ready. Confirm the swap in your wallet.');}
        if(s==='swap-sent') {step(4);say('Swap submitted. Waiting for Polygon confirmation.'); $('exReceipt').hidden=false; $('exReceipt').innerHTML='Submitted · confirmation pending<br><a href="https://polygonscan.com/tx/'+esc(tx)+'" target="_blank" rel="noopener noreferrer">Track transaction ↗</a>';}
      },{minOut:q.min,account:acct,quotedAt:q.time});
      await MM.waitMined(r.swapTx);
      $('exReceipt').hidden=false; $('exReceipt').innerHTML='✓ Swap confirmed on Polygon.<br><a href="https://polygonscan.com/tx/'+esc(r.swapTx)+'" target="_blank" rel="noopener noreferrer">View transaction and actual token amounts ↗</a>';
      say('Confirmed. Token balances are refreshing.'); $('exAmount').value=''; S.quote=null; await balance();
      if(window.SigilMarket) window.SigilMarket.refresh();
    } catch(e) {say(e.code===4001?'Rejected in your wallet. Check any already submitted approval or swap in the explorer.':e.message,true);}
    finally {S.executing=false;lock(false);$('exGo').textContent=S.quote?(S.quote.impact>=15?'Reduce amount · impact above 15%':'Review swap'):MM.getState().connected?'Refresh quote':'Connect wallet';$('exGo').disabled=!!(S.quote&&S.quote.impact>=15);}
  }
  function open() { if(bridgeBusy) return; if($('xchain-modal')) window.closeXChain(); S.returnFocus=document.activeElement; m.style.display='flex'; document.body.style.overflow='hidden'; invalidate(); balance(); if($('exAmount').value) quote(); $('exAmount').focus(); }
  function close() { if(S.busy) return; m.style.display='none'; document.body.style.overflow=''; if(S.returnFocus&&S.returnFocus.focus) S.returnFocus.focus(); }
  window.openSwap=open;
  $('exClose').onclick=close; m.onclick=function(e){if(e.target===m) close();};
  $('exBuy').onclick=function(){setSide(false);};$('exSell').onclick=function(){setSide(true);};$('exFlip').onclick=function(){setSide(!S.sell);};
  var timer; $('exAmount').oninput=function(){invalidate();clearTimeout(timer);timer=setTimeout(quote,300);};$('exSlippage').onchange=quote;$('exGo').onclick=go;
  m.querySelectorAll('[data-pct]').forEach(function(b){b.onclick=function(){if(S.balances===null){say('Connect a wallet and wait for its balance.',true);return;}$('exAmount').value=fmt(S.balances*BigInt(b.dataset.pct)/100n,dps()[0]);quote();};});
  $('exBridgeLink').onclick=function(){close();window.openXChain('to_polygon');};$('exNative').onclick=function(){close();legacyOpen();};
  setInterval(function(){if(S.quote&&!S.busy&&Date.now()-S.quote.time>30000){invalidate();$('exGo').textContent='Refresh quote';$('exGo').disabled=false;say('Quote expired. Refresh before reviewing.');}},1000);
  MM.on('change',function(){invalidate();balance();if(m.style.display==='flex'&&!S.busy&&$('exAmount').value)quote();});
  // Preserve the existing shielded-lock/burn implementation; enlarge its real form.
  var bridgeBusy=false, originalBridge=window.openXChain, originalClose=window.closeXChain, originalGo=window.xcGo, originalDir=window.xcSetDir;
  var bridge=$('xchain-modal'), box=bridge&&bridge.querySelector('.xc-box');
  if(box){
    box.prepend(aside('bridge'));box.setAttribute('role','dialog');box.setAttribute('aria-modal','true');box.setAttribute('aria-label','Bridge SIGIL and Polygon');
    // Legacy Send also has an element named uniGo. Scope this lookup to bridge.
    var direct=bridge.querySelector('[id="uniGo"]'); if(direct){var oldPanel=direct.parentElement;oldPanel.hidden=true;oldPanel.style.display='none';}
    var note=el('div','ex-foot','Bridge wraps native SIGIL at 1:1. It does not exchange SIGIL for USDC. A submitted lock still requires settlement and a relayer mint.');$('xcForm').prepend(note);
    var route=el('div','ex-risk');route.id='exBridgeSummary';$('xcForm').insertBefore(route,$('xcGo'));
    $('xcAmt').setAttribute('aria-label','Bridge amount');$('xcDest').setAttribute('aria-label','Destination wallet address');$('xcMsg').setAttribute('role','status');$('xcMsg').setAttribute('aria-live','polite');
    bridge.querySelector('.xc-x').setAttribute('aria-label','Close bridge');
    bridge.querySelectorAll('.xc-half').forEach(function(e){e.setAttribute('role','button');e.tabIndex=0;e.onkeydown=function(k){if(k.key==='Enter'||k.key===' '){k.preventDefault();e.click();}};});
    function summary(){var back=bridge.dataset.dir==='to_sigil', t=MM.TOKEN.address;route.textContent=(back?'Polygon → native SIGIL':'Native SIGIL → Polygon')+' · 1:1 wrapping · bridge token '+t+'. The market trades WSIGIL3 ('+W+'); these are different contracts.';}
    window.xcSetDir=function(d){if(bridgeBusy)return;originalDir(d);summary();};
    window.openXChain=function(d){if(S.busy||bridgeBusy)return;close();S.returnFocus=document.activeElement;originalBridge(d);document.body.style.overflow='hidden';summary();$('xcAmt').focus();};
    window.closeXChain=function(){if(bridgeBusy)return;originalClose();document.body.style.overflow='';if(S.returnFocus&&S.returnFocus.focus)S.returnFocus.focus();};
    window.openBridge=function(){window.openXChain('to_polygon');};window.openBridgeFrom=window.openBridge;
    window.xcGo=async function(){
      if(bridgeBusy||window.__exchangeBridgeRunning)return;window.__exchangeBridgeRunning=true;var back=bridge.dataset.dir==='to_sigil',dest=$('xcDest').value.trim(),amount;
      try{
        amount=MM.parseAmount8($('xcAmt').value);if(amount<=0n)throw new Error('Enter an amount greater than zero.');
        if(!back&&amount*100n>BigInt(Number.MAX_SAFE_INTEGER))throw new Error('Amount exceeds the bridge API safe precision limit. Use a smaller transfer.');
        if(!(back?/^[0-9a-f]{64}$/i:/^0x[0-9a-f]{40}$/i).test(dest)||/^0x0{40}$/i.test(dest)||/^0{64}$/.test(dest))throw new Error('Enter a valid non-zero destination address for '+(back?'SIGIL.':'Polygon.'));
        if(back&&(!MM.burnEnabled||!MM.burnEnabled()))throw new Error('Return bridge is disabled. Do not burn wrapped tokens.');
        if(!back){var raw=window.SIGIL_RAW;if(raw==null)throw new Error('Native balance is unavailable. Refresh your wallet.');if(amount>BigInt(raw)/100n)throw new Error('Amount exceeds your available native balance.');}
        if(!back&&!(await window.sigilConfirm('Bridge '+fmt(amount,8)+' native SIGIL to Polygon?\n\nRecipient: '+dest+'\nBridge token: '+MM.TOKEN.address+'\n\nThis locks SIGIL before settlement and relayer minting. It does not buy WSIGIL3.')))return;
        bridgeBusy=true;var controls=Array.from(bridge.querySelectorAll('input,button,select'));controls.forEach(function(e){e.dataset.exDisabled=e.disabled?'1':'0';e.disabled=true;});
        await originalGo();
      }catch(e){$('xcMsg').textContent=e.message;$('xcMsg').style.color='#ffa5b2';}
      finally{bridgeBusy=false;window.__exchangeBridgeRunning=false;bridge.querySelectorAll('[data-ex-disabled]').forEach(function(e){e.disabled=e.dataset.exDisabled==='1';delete e.dataset.exDisabled;});if(back&&!MM.burnEnabled())$('xcGo').disabled=true;}
    };
    window.uniTrade=open; // All visible Polygon swaps use the same reviewed flow.
  }
  document.addEventListener('keydown',function(e){
    var active=m.style.display==='flex'?m:bridge&&bridge.style.display==='flex'?bridge:null;if(!active)return;
    if($('ask-modal')&&$('ask-modal').style.display!=='none')return;
    if(e.key==='Escape'){e.stopImmediatePropagation();if(active===m)close();else window.closeXChain();}
    if(e.key==='Tab'){var nodes=Array.from(active.querySelectorAll('button,input,select,a[href],[tabindex="0"]')).filter(function(n){return !n.disabled&&n.getClientRects().length;});if(!nodes.length)return;var first=nodes[0],last=nodes[nodes.length-1];if(e.shiftKey&&document.activeElement===first){e.preventDefault();last.focus();}else if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first.focus();}}
  },true);
})();
