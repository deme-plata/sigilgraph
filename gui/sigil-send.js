/* Send studio: retains the wallet's signing, proof, receipt and local-node flows. */
(function () {
  'use strict';
  var modal = document.getElementById('send-modal');
  if (!modal || modal.dataset.studio) return;
  modal.dataset.studio = '1';
  var $ = function (id) { return modal.querySelector('[id="' + id + '"]'); };
  var dialog = modal.firstElementChild, form = $('sendForm'), unified = $('sendUnified');
  var addr = $('uniAddr'), amount = $('uniAmt'), go = $('uniGo');
  var unit = window.SIGIL_UNITS || 10000000000n, dp = window.SIGIL_DP || 10;
  // Mirrors sigil_state::shielded::SHIELDED_FEE, also used by doUnifiedSend.
  var fee = 100000n, busy = false, checking = false, review = null, opener = null;
  var icon = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M12 20V4m-6 6 6-6 6 6M5 20h14"/></svg>';
  var style = document.createElement('style');
  style.textContent = `
  #send-modal{z-index:100020!important;padding:24px!important;background:rgba(3,6,13,.86)!important;backdrop-filter:blur(16px)!important}
  #send-modal .ss-dialog{width:100%!important;max-width:1080px!important;max-height:90dvh!important;background:radial-gradient(ellipse at 100% 0,rgba(123,81,208,.12),transparent 55%),#0a101b!important;border:1px solid #2e3447!important;border-radius:24px!important;box-shadow:0 28px 100px #0009!important;color:#e9edf7;font:15px/1.5 'Rajdhani',system-ui,sans-serif!important}
  #send-modal .ss-header{padding:24px 28px!important;background:transparent!important;border-bottom:1px solid #252d3d!important;gap:14px!important}
  #send-modal .ss-mark{width:44px;height:44px;border:1px solid #6b54a2;border-radius:14px;display:grid;place-items:center;color:#c6b0ff;background:#a78bfa12;flex:none}
  #send-modal svg{width:24px;height:24px;flex:none}
  #send-modal .ss-title{margin:0;font:600 24px/1.3 system-ui,sans-serif;letter-spacing:-.6px;color:#f1f0ff}
  #send-modal .ss-sub{font:12px/1.5 system-ui,sans-serif;color:#9aa8bf;margin-top:4px}
  #send-modal #sendClose{width:38px!important;height:38px!important;border:1px solid #354059!important;border-radius:12px!important;background:#ffffff05!important;color:#bfc8dc!important}
  #send-modal #sendForm{padding:0!important;overflow:auto!important;gap:0!important;flex-direction:row!important;align-items:stretch;min-height:0}
  #send-modal .ss-main{padding:26px 28px;flex:1;min-width:0}
  #send-modal .ss-side{width:310px;flex:none;background:#ffffff02;border-left:1px solid #252d3d;padding:26px 24px;box-sizing:border-box}
  #send-modal .ss-eyebrow{font:600 10px/1.5 system-ui,sans-serif;color:#a89acb;letter-spacing:1.6px;text-transform:uppercase;margin-bottom:10px}
  #send-modal .ss-network{display:inline-flex;align-items:center;gap:8px;border:1px solid #344659;color:#b8e9dc;border-radius:30px;padding:6px 11px;font:11px system-ui,sans-serif;margin-bottom:22px}
  #send-modal .ss-network::before{content:'';width:6px;height:6px;background:#66d8be;border-radius:50%}
  #send-modal #sendUnified{gap:20px!important}
  #send-modal label{font:600 12px/1.5 system-ui,sans-serif!important;letter-spacing:0!important;text-transform:none!important;color:#cbd4e7!important}
  #send-modal input{box-sizing:border-box;max-width:100%;background:#070c16!important;border:1px solid #354059!important;border-radius:12px!important;color:#edf1fb!important;min-height:50px;outline:none}
  #send-modal input:focus{border-color:#ae91f8!important;box-shadow:0 0 0 3px #a78bfa1c!important}
  #send-modal #uniAddr{font:12px/1.6 'JetBrains Mono',monospace!important;padding:14px!important}
  #send-modal #uniAmt{font:500 27px/1.4 system-ui,sans-serif!important;padding:12px 14px!important;min-width:0}
  #send-modal #uniAvail{display:none}
  #send-modal #uniAddrMsg{line-height:1.6!important;min-height:17px!important}
  #send-modal .ss-tools{display:flex;gap:8px;flex-wrap:wrap;margin-top:8px}
  #send-modal .ss-button{border:1px solid #354059;border-radius:10px;background:#ffffff05;color:#c5cde0;padding:8px 12px;font:12px/1.5 system-ui,sans-serif;cursor:pointer}
  #send-modal button:focus-visible,#send-modal [tabindex]:focus-visible{outline:2px solid #bda0ff;outline-offset:3px}
  #send-modal button:disabled{cursor:wait;opacity:.55}
  #send-modal #uniGo{background:linear-gradient(100deg,#b79af5,#8faff6)!important;color:#101326!important;border:0!important;font:600 14px/1.5 system-ui,sans-serif!important;letter-spacing:0!important;padding:15px!important}
  #send-modal .ss-row{display:flex;justify-content:space-between;gap:14px;color:#95a3bb;font:12px/1.7 system-ui,sans-serif;padding:11px 0;border-bottom:1px solid #252d3d}
  #send-modal .ss-row b{font-weight:500;color:#e2e8f6;text-align:right;overflow-wrap:anywhere}
  #send-modal .ss-note{font:12px/1.8 system-ui,sans-serif;color:#92a0b8;margin:16px 0}
  #send-modal .ss-note strong{color:#c5cedf;font-weight:500}
  #send-modal #ssBalance{font:500 25px/1.3 system-ui,sans-serif;color:#e6e1fc;overflow-wrap:anywhere;margin:0 0 4px}
  #send-modal #ssDoor{padding:12px;border:1px solid #354059;border-radius:12px;font:12px/1.7 system-ui,sans-serif;color:#b6c5db;margin:18px 0}
  #send-modal .ss-review{border:1px solid #68548f;border-radius:16px;padding:18px;background:#a78bfa08}
  #send-modal .ss-review h3{font:500 18px system-ui,sans-serif;margin:0 0 12px;color:#e7dbff}
  #send-modal #ssReviewAddress{font:11px/1.8 monospace;color:#d8e1f4;overflow-wrap:anywhere;padding:12px 0}
  #send-modal #ssContacts{margin-top:10px;padding:12px;border:1px solid #354059;border-radius:12px;background:#080e18}
  #send-modal #ssSearch{width:100%;font:12px system-ui,sans-serif;padding:10px;min-height:38px}
  #send-modal #ssContactList{max-height:165px;overflow:auto;display:grid;gap:5px;margin-top:8px}
  #send-modal .ss-contact{text-align:left;width:100%;display:block}
  #send-modal .ss-contact small{display:block;color:#93a0b8;font:10px/1.7 monospace;overflow-wrap:anywhere}
  #send-modal #sendAdvanced{gap:16px!important}
  #send-modal #sendReceipt{padding:30px!important;gap:18px!important;min-height:260px}
  #send-modal #rcRows{overflow-wrap:anywhere}
  #send-modal #uniMsg{line-height:1.7!important;overflow-wrap:anywhere}
  #send-modal #uniSecret{line-height:1.8!important;font-size:10px!important}
  @media(max-width:720px){#send-modal{padding:10px!important}#send-modal .ss-dialog{border-radius:18px!important;max-height:96dvh!important}#send-modal .ss-header{padding:18px!important}#send-modal #sendForm{flex-direction:column!important}#send-modal .ss-main{padding:20px 18px}#send-modal .ss-side{width:100%;border-left:0;border-top:1px solid #252d3d;padding:20px 18px}#send-modal .ss-title{font-size:21px}#send-modal #uniAmt{font-size:23px!important}}
  `;
  document.head.appendChild(style);
  dialog.classList.add('ss-dialog');
  dialog.setAttribute('role','dialog'); dialog.setAttribute('aria-modal','true'); dialog.setAttribute('aria-labelledby','ssTitle');
  var header=dialog.firstElementChild; header.classList.add('ss-header');
  header.firstElementChild.className='ss-mark';header.firstElementChild.innerHTML=icon;
  header.children[1].innerHTML='<h2 id="ssTitle" class="ss-title">Send SIGIL</h2><div class="ss-sub">A private payment. One continuous flow.</div><span id="sendAvail" hidden></span>';
  $('sendClose').setAttribute('aria-label','Close Send');
  var main=document.createElement('div');main.className='ss-main';
  while(form.firstChild)main.appendChild(form.firstChild);form.appendChild(main);
  var badge=document.createElement('div');badge.className='ss-network';badge.textContent='SIGIL native network';main.prepend(badge);
  var side=document.createElement('aside');side.className='ss-side';
  side.innerHTML='<div class="ss-eyebrow">Your wallet</div><div id="ssBalance">—</div><div class="ss-sub">SIGIL · private balance</div><div class="ss-row"><span>Transparent balance</span><b id="ssTransparent">—</b></div><div class="ss-row"><span>Private payment fee</span><b id="ssFee"></b></div><div id="ssDoor" role="status">Checking private pool availability…</div><p class="ss-note"><strong>Private by default.</strong><br>The wallet prepares a private balance when needed, then pays your recipient. Follow each step here.</p><p class="ss-note">Send native SIGIL to a SIGIL address or payment code. Polygon WSIGIL belongs in Bridge &amp; Swap.</p><button type="button" class="ss-button" id="ssAdvanced">Advanced controls</button>';
  form.appendChild(side);
  addr.id='uniAddr';addr.setAttribute('aria-label','Recipient address or payment code');addr.setAttribute('aria-describedby','uniAddrMsg');
  amount.setAttribute('aria-label','Amount in SIGIL');amount.setAttribute('aria-describedby','ssAmountHelp');
  $('uniMsg').setAttribute('role','status');$('uniMsg').setAttribute('aria-live','polite');$('uniAddrMsg').setAttribute('aria-live','polite');
  var tools=document.createElement('div');tools.className='ss-tools';tools.innerHTML='<button type="button" class="ss-button" id="ssChoose">Choose contact</button><button type="button" class="ss-button" id="ssPaste">Paste</button>';
  addr.after(tools);
  var contacts=document.createElement('div');contacts.id='ssContacts';contacts.hidden=true;contacts.innerHTML='<input id="ssSearch" aria-label="Search saved contacts" placeholder="Search names or addresses"><div id="ssContactList"></div>';
  tools.after(contacts);
  var help=document.createElement('div');help.id='ssAmountHelp';help.className='ss-sub';help.textContent='SIGIL · up to '+dp+' decimals. MAX reserves the private payment fee.';amount.parentElement.after(help);
  var panel=document.createElement('div');panel.className='ss-review';panel.hidden=true;
  panel.innerHTML='<h3>Review your payment</h3><div class="ss-sub">Recipient · SIGIL native network</div><div id="ssReviewAddress"></div><div class="ss-row"><span>Recipient receives</span><b id="ssReviewAmount"></b></div><div class="ss-row"><span>Private payment fee</span><b id="ssReviewFee"></b></div><div class="ss-row"><span>Amount + payment fee</span><b id="ssReviewTotal"></b></div><p class="ss-note">Check the full recipient address. Confirming starts the existing wallet signing and private payment flow.</p><button type="button" class="ss-button" id="ssEdit">Edit payment</button>';
  go.before(panel);
  var advLink=$('uniAdvLink');if(advLink)advLink.parentElement.style.display='none';
  function fmt(raw){var f=(raw%unit).toString().padStart(dp,'0').replace(/0+$/,'');return (raw/unit).toString()+(f?'.'+f:'');}
  function rawAmount(s){if(!new RegExp('^\\d+(\\.\\d{1,'+dp+'})?$').test(s))return null;var p=s.split('.');return BigInt(p[0])*unit+BigInt((p[1]||'').padEnd(dp,'0'));}
  function balance(name){try{return BigInt(window[name]||0);}catch(_){return 0n;}}
  function message(s){$('uniMsg').textContent=s;}
  function invalidate(){review=null;panel.hidden=true;go.textContent='Review payment';}
  function updateBalances(){
    $('ssBalance').textContent=fmt(balance('__sigilShieldRaw'));$('ssTransparent').textContent=fmt(balance('SIGIL_RAW'))+' SIGIL';$('ssFee').textContent=fmt(fee)+' SIGIL';
  }
  async function pool(){
    // Fetch independently: the legacy helper treats a network failure as an open door.
    var controller=new AbortController(),timer=setTimeout(function(){controller.abort();},8000);
    try{var r=await fetch('/v1/shielded/door',{cache:'no-store',signal:controller.signal});if(!r.ok)throw Error();var j=await r.json();if(!j.door||typeof j.door.deposits_paused!=='boolean')throw Error();return j.door.deposits_paused;}finally{clearTimeout(timer);}
  }
  async function updatePool(){try{var paused=await pool();$('ssDoor').textContent=paused?'Private deposits are paused. Existing private funds can still be sent. MAX uses your private balance.':'Private deposits are open. If needed, the wallet prepares your private balance before payment.';}catch(_){$('ssDoor').textContent='Private pool availability could not be checked. The wallet will check again when preparing payment.';}}
  function renderContacts(){
    var list=$('ssContactList');list.replaceChildren();var entries;
    try{entries=JSON.parse(localStorage.getItem('sigil-addrbook')||'[]');if(!Array.isArray(entries))throw Error();}catch(_){list.textContent='Saved contacts could not be read. Open Address Book to manage them.';return;}
    var q=$('ssSearch').value.toLowerCase();entries.filter(function(c){return c&&typeof c.addr==='string'&&/^(?:0x)?[a-f0-9]{64}$/i.test(c.addr)&&!/^(?:0x)?0{64}$/i.test(c.addr)&&(String(c.label||'')+' '+c.addr).toLowerCase().includes(q);}).sort(function(a,b){return Number(!!b.favorite)-Number(!!a.favorite);}).forEach(function(c){
      var b=document.createElement('button');b.type='button';b.className='ss-button ss-contact';b.textContent=(c.favorite?'★ ':'')+String(c.label||'Saved wallet');var sub=document.createElement('small');sub.textContent=c.addr.slice(0,12)+'…'+c.addr.slice(-10);b.appendChild(sub);
      b.onclick=function(){addr.value=c.addr.replace(/^0x/i,'').toLowerCase();addr.dispatchEvent(new Event('input',{bubbles:true}));contacts.hidden=true;amount.focus();};list.appendChild(b);
    });if(!list.childNodes.length)list.textContent=q?'No matching contacts.':'No saved contacts yet. Add one in Address Book.';
  }
  $('ssChoose').onclick=function(){contacts.hidden=!contacts.hidden;if(!contacts.hidden){renderContacts();$('ssSearch').focus();}};
  $('ssSearch').oninput=renderContacts;
  window.addEventListener('storage',function(e){if(e.key==='sigil-addrbook'&&!contacts.hidden)renderContacts();});
  $('ssPaste').onclick=async function(){try{addr.value=(await navigator.clipboard.readText()).trim();addr.dispatchEvent(new Event('input',{bubbles:true}));addr.focus();}catch(_){message('Clipboard unavailable. Paste into the recipient field with your keyboard.');addr.focus();}};
  addr.addEventListener('input',invalidate);amount.addEventListener('input',invalidate);
  // MAX updates .value directly, so explicitly invalidate any previous review.
  var max=amount.parentElement.querySelector('button');if(max){max.removeAttribute('onclick');max.onclick=async function(){invalidate();await window.uniMax();amount.dispatchEvent(new Event('input',{bubbles:true}));};}
  $('ssAdvanced').onclick=function(){if(busy||checking)return;invalidate();window.uniToggleAdvanced();$('ssAdvanced').textContent=$('sendAdvanced').style.display==='none'?'Advanced controls':'Back to simple send';};
  $('ssEdit').onclick=function(){invalidate();addr.focus();};
  go.removeAttribute('onclick');
  go.onclick=async function(){
    if(busy||checking)return;
    var a=addr.value.trim(),s=amount.value.trim(),n=rawAmount(s),payee=window.sigilParsePayee&&window.sigilParsePayee(a);
    if(!payee){message('Enter a valid native SIGIL address or payment code. Polygon addresses are not supported here.');addr.focus();return;}
    if(n===null||n<=0n){message('Enter an amount greater than zero, with up to '+dp+' decimals.');amount.focus();return;}
    if(!review||review.address!==a||review.amount!==s){
      checking=true;go.disabled=true;go.textContent='Checking payment…';
      try{
        updateBalances();var p=balance('__sigilShieldRaw'),t=balance('SIGIL_RAW');
        if(n+fee>p){var paused;try{paused=await pool();}catch(_){throw Error('Cannot check private pool availability. Try reviewing again when connected.');}if(paused)throw Error('Private deposits are paused. This payment needs more private funds. Use MAX to send from your existing private balance.');if(n+fee>p+t)throw Error('Your displayed balance does not cover this amount and the private payment fee. Refresh your balance or use MAX.');}
        if(addr.value.trim()!==a||amount.value.trim()!==s)return;
        review={address:a,amount:s};$('ssReviewAddress').textContent=a;$('ssReviewAmount').textContent=fmt(n)+' SIGIL';$('ssReviewFee').textContent=fmt(fee)+' SIGIL';$('ssReviewTotal').textContent=fmt(n+fee)+' SIGIL';panel.hidden=false;message('Review the full recipient and amount, then confirm.');go.textContent='Confirm & send privately';
      }catch(e){invalidate();message(e.message);}finally{checking=false;go.disabled=false;if(!review)go.textContent='Review payment';}
      return;
    }
    // Resolve the handler at confirmation time: TUI injects its local-node wrapper after this module.
    busy=true;go.disabled=true;go.textContent='Payment in progress…';
    var fields=Array.from(main.querySelectorAll('input,button'));var prior=fields.map(function(e){return e.disabled;});fields.forEach(function(e){e.disabled=true;});
    try{await window.doUnifiedSend();}catch(e){message('Payment status could not be retrieved. Check your transaction history before trying again.');}
    finally{busy=false;fields.forEach(function(e,i){e.disabled=prior[i];});go.disabled=false;invalidate();}
  };
  var originalOpen=window.openSend,originalClose=window.closeSend;
  window.openSend=function(){
    opener=document.activeElement;
    // Reopening an in-flight operation must not clear its progress, recipient or receipt.
    if(busy||$('sendReceipt').style.display==='flex'){modal.style.display='flex';return;}
    originalOpen.apply(this,arguments);invalidate();contacts.hidden=true;$('ssAdvanced').textContent='Advanced controls';updateBalances();updatePool();
  };
  window.closeSend=function(){originalClose.apply(this,arguments);if(opener&&opener.isConnected&&typeof opener.focus==='function')opener.focus();};
  modal.addEventListener('keydown',function(e){
    if(e.key==='Escape'){e.preventDefault();window.closeSend();return;}
    if(e.key!=='Tab')return;
    var focusable=Array.from(dialog.querySelectorAll('button,input,a[href],[tabindex="0"]')).filter(function(el){return !el.disabled&&el.getClientRects().length;});
    if(!focusable.length)return;var first=focusable[0],last=focusable[focusable.length-1];
    if(e.shiftKey&&(document.activeElement===first||!dialog.contains(document.activeElement))){e.preventDefault();last.focus();}else if(!e.shiftKey&&(document.activeElement===last||!dialog.contains(document.activeElement))){e.preventDefault();first.focus();}
  });
  invalidate();updateBalances();
})();
