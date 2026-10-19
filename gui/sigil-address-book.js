/* SIGIL address book. Keeps the existing {label,addr} storage and Send integration. */
(function () {
  'use strict';
  var old = document.getElementById('book-modal');
  if (!old || document.getElementById('contactsTitle')) return;
  var KEY = 'sigil-addrbook', S = { selected: null, mode: 'view', filter: 'all', base: null, focus: null, overflow: '' };
  var paths = {
    book: '<path d="M4 4h13a3 3 0 0 1 3 3v13H7a3 3 0 0 1-3-3V4Z"/><path d="M4 16h16M8 4v12"/><circle cx="14" cy="9" r="2"/><path d="M11 14c.3-2 5.7-2 6 0"/>',
    plus: '<path d="M12 5v14M5 12h14"/>', search: '<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 4 4"/>',
    close: '<path d="m6 6 12 12M6 18 18 6"/>', copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V4H4v12h4"/>',
    send: '<path d="m21 3-7 18-4-7-7-4L21 3ZM10 14 21 3"/>',
    edit: '<path d="m15 5 4 4M4 20l4-1L20 7a2.8 2.8 0 0 0-4-4L4 15v5Z"/>',
    trash: '<path d="M3 6h18M9 6V3h6v3M6 6l1 15h10l1-15M10 10v7M14 10v7"/>',
    star: '<path d="m12 3 2.8 5.7 6.2.9-4.5 4.4 1.1 6.2-5.6-3-5.6 3 1.1-6.2L3 9.6l6.2-.9L12 3Z"/>',
    shield: '<path d="m12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6l8-3Z"/><path d="m8 12 3 3 5-6"/>'
  };
  function icon(name) { return '<svg class="cb-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'+paths[name]+'</svg>'; }
  function element(tag, cls, text) { var e=document.createElement(tag);e.className=cls||'';if(text!=null)e.textContent=text;return e; }
  function $(id) { return document.getElementById(id); }
  function normalize(a) { return String(a||'').trim().toLowerCase().replace(/^0x/,''); }
  function valid(a) { return /^[0-9a-f]{64}$/.test(a)&&!/^0{64}$/.test(a); }
  function name(c) { return c&&typeof c.label==='string'&&c.label.trim()?c.label.trim():'Unnamed contact'; }
  function address(c) { return c&&typeof c.addr==='string'?normalize(c.addr):''; }
  function short(a) { return a.length>22?a.slice(0,12)+'…'+a.slice(-10):a||'Address needs attention'; }
  function read() {
    var raw=localStorage.getItem(KEY), entries=JSON.parse(raw||'[]');
    if(!Array.isArray(entries))throw new Error('Saved contacts have an unsupported format. Your data has been kept.');
    return {raw:raw,entries:entries};
  }
  function write(snapshot, entries) {
    if(localStorage.getItem(KEY)!==snapshot.raw)throw new Error('Contacts changed in another tab. Review the latest version and try again.');
    localStorage.setItem(KEY,JSON.stringify(entries));
    if(typeof window.abRender==='function')window.abRender();
    window.dispatchEvent(new CustomEvent('sigil-addressbook-change'));
  }
  function say(text, error) { $('contactsStatus').textContent=text||'';$('contactsStatus').classList.toggle('cb-error',!!error); }
  function button(label, symbol, cls, fn) { var b=element('button',cls||'cb-button');b.type='button';b.innerHTML=icon(symbol);b.appendChild(document.createTextNode(label));b.onclick=fn;return b; }
  // book-user is absent from older Lucide bundles. Use a local SVG, with no CDN dependency.
  document.querySelectorAll('[data-lucide="book-user"]').forEach(function(node){
    var holder=document.createElement('span');holder.innerHTML=icon('book');var svg=holder.firstChild;svg.classList.add('lic');node.replaceWith(svg);
    var action=svg.closest('.act,a');if(action){action.setAttribute('aria-label','Open address book');action.setAttribute('title','Address book');if(action.tagName!=='BUTTON'&&!action.hasAttribute('href')){action.setAttribute('role','button');action.tabIndex=0;action.addEventListener('keydown',function(e){if(e.key==='Enter'||e.key===' '){e.preventDefault();action.click();}});}}
  });
  var style=element('style');style.textContent=`
    .cb-icon{width:20px;height:20px;flex:none;vertical-align:middle}.act .cb-icon.lic{width:24px;height:24px;display:block}
    #book-modal.cb-overlay{display:none;position:fixed;inset:0;z-index:100010;padding:28px;align-items:center;justify-content:center;background:#03060cdb;backdrop-filter:blur(18px);color:#e5eaf7;font-family:Inter,'Segoe UI',sans-serif}
    #book-modal .cb-dialog{box-sizing:border-box;width:min(1060px,100%);height:min(720px,calc(100dvh - 56px));display:flex;flex-direction:column;overflow:hidden;background:#0d111c;border:1px solid #30354c;border-radius:24px;box-shadow:0 40px 130px #0009}
    #book-modal *{box-sizing:border-box}#book-modal button,#book-modal input,#book-modal textarea{font-family:inherit}#book-modal button{cursor:pointer}#book-modal button:disabled{opacity:.4;cursor:not-allowed}#book-modal :focus-visible{outline:2px solid #bba7f1;outline-offset:3px}
    .cb-header{display:flex;align-items:center;gap:14px;padding:24px 28px;border-bottom:1px solid #ffffff10;flex:none}.cb-emblem{display:grid;place-items:center;width:46px;height:46px;border-radius:14px;background:#a995ff18;color:#c5b6fa;border:1px solid #a995ff35}.cb-emblem .cb-icon{width:25px;height:25px}.cb-heading{flex:1;min-width:0}.cb-heading h1{font-size:23px;letter-spacing:-.6px;margin:0;font-weight:600}.cb-heading p{font-size:12px;color:#94a2bd;margin:6px 0 0}.cb-button{border:1px solid #ffffff1d;border-radius:10px;background:#ffffff05;color:#bdc7dc;display:inline-flex;align-items:center;justify-content:center;gap:8px;padding:10px 13px;font-size:12px;line-height:1.3}.cb-button.primary{background:#bba7f1;color:#1a142a;border-color:#c4b5fd55;font-weight:650}.cb-button.danger{color:#ee9daa;border-color:#ee9daa26}.cb-button.starred{color:#e6c889}.cb-button.starred .cb-icon{fill:#e6c88930}.cb-close{width:36px;height:36px;padding:8px;border-radius:50%}
    .cb-body{display:grid;grid-template-columns:340px minmax(0,1fr);flex:1;min-height:0}.cb-sidebar{background:#111522;border-right:1px solid #ffffff0d;display:flex;flex-direction:column;min-height:0}.cb-tools{padding:22px 20px 14px;flex:none}.cb-search{position:relative}.cb-search>.cb-icon{position:absolute;left:12px;top:12px;color:#8593ae;width:17px;height:17px}.cb-search input{width:100%;background:#090e18;border:1px solid #ffffff18;border-radius:10px;padding:12px 10px 12px 37px;color:#dce5f7;font-size:12px}.cb-filters{display:flex;gap:7px;margin-top:12px}.cb-filters button{padding:7px 11px;background:transparent;color:#8f9db7;border:1px solid transparent;border-radius:7px;font-size:11px}.cb-filters button[aria-pressed=true]{background:#a995ff16;border-color:#a995ff30;color:#d4c7ff}.cb-count{font-size:10px;text-transform:uppercase;letter-spacing:1px;color:#7e8ba5;margin-top:18px}.cb-list{overflow-y:auto;padding:0 12px 18px;min-height:0;flex:1}.cb-contact{display:flex;align-items:center;gap:11px;width:100%;text-align:left;border:1px solid transparent;background:transparent;border-radius:12px;padding:12px 10px;margin-top:5px;color:#dce5f7}.cb-contact:hover{background:#ffffff04}.cb-contact[aria-current=true]{border-color:#a995ff35;background:#a995ff10}.cb-avatar{display:grid;place-items:center;width:40px;height:40px;border-radius:12px;flex:none;background:#bba7f112;border:1px solid #bba7f123;color:#c7b8ff;font-size:13px;font-weight:650}.cb-contact-text{min-width:0;flex:1}.cb-contact-name{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:13px;font-weight:550}.cb-contact-address{display:block;margin-top:5px;color:#8492ad;font:10px ui-monospace,monospace}.cb-contact>.cb-icon{width:14px;height:14px;color:#d4b67c}.cb-contact>.cb-icon.starred{fill:#d4b67c40}.cb-detail{overflow-y:auto;padding:32px;min-width:0}.cb-detail-top{display:flex;justify-content:space-between;gap:14px;align-items:flex-start}.cb-detail .cb-avatar{width:68px;height:68px;border-radius:20px;font-size:23px;background:radial-gradient(circle at 20% 10%,#bba7f12a,#bba7f10d)}.cb-detail h2{font-size:27px;letter-spacing:-.7px;margin:19px 0 10px;overflow-wrap:anywhere;font-weight:600}.cb-network{display:inline-flex;gap:6px;align-items:center;color:#93d0bd;background:#74bca910;border:1px solid #74bca925;border-radius:20px;padding:6px 9px;font-size:10px}.cb-network:before{content:'';width:5px;height:5px;border-radius:50%;background:#82c9b5}.cb-caption{font-size:10px;text-transform:uppercase;letter-spacing:1px;color:#929fb9;margin:28px 0 10px}.cb-address{width:100%;resize:none;display:block;background:#131a28;color:#becae2;border:1px solid #ffffff14;border-radius:12px;padding:16px;font:12px/1.9 ui-monospace,monospace!important;overflow-wrap:anywhere;word-break:break-all}.cb-address-note{font-size:11px;line-height:1.7;color:#8391ab;margin-top:10px}.cb-actions{display:flex;flex-wrap:wrap;gap:9px;margin-top:24px}.cb-actions .primary{flex:1}.cb-divider{border:0;border-top:1px solid #ffffff10;margin:30px 0 16px}.cb-local{display:flex;align-items:flex-start;gap:9px;font-size:11px;line-height:1.8;color:#8391ab}.cb-local>.cb-icon{width:17px;height:17px;margin-top:2px;color:#8b9abc}.cb-empty{text-align:center;padding:45px 14px;color:#8d9ab4;line-height:1.7;font-size:12px}.cb-empty>.cb-icon{width:40px;height:40px;color:#9e8bd4;margin-bottom:10px}.cb-empty h2,.cb-empty h3{color:#dce5f7;font-size:18px;margin:8px 0;font-weight:550}.cb-empty .cb-button{margin-top:16px}.cb-list .cb-empty{padding:30px 12px}.cb-form h2{margin-top:0;font-size:23px}.cb-form label{display:block;color:#b6c1d7;font-size:12px;margin:24px 0 9px}.cb-form input{width:100%;background:#131a28;border:1px solid #ffffff1c;border-radius:10px;padding:14px;color:#e0e7f7;font-size:14px}.cb-form textarea{font:12px/1.8 ui-monospace,monospace!important}.cb-form .cb-helper{font-size:11px;color:#8391ab;line-height:1.7;margin-top:8px}.cb-form .cb-inline{display:flex;justify-content:space-between;align-items:center;gap:10px;margin-top:22px}.cb-form .cb-inline label{margin:0}.cb-form .cb-inline .cb-button{padding:6px 10px;font-size:10px}.cb-status{margin:0 28px;padding:12px 0 16px;color:#9fd2bf;font-size:12px;line-height:1.5;min-height:40px;flex:none}.cb-status.cb-error{color:#f3a3b1}.cb-footer{border-top:1px solid #ffffff0d;padding:12px 28px;color:#7886a0;font-size:10px;flex:none;display:flex;justify-content:space-between;gap:12px}#ask-modal{z-index:100060!important}
    @media(max-width:760px){#book-modal.cb-overlay{padding:10px}#book-modal .cb-dialog{height:calc(100dvh - 20px);border-radius:18px}.cb-header{padding:18px 16px;gap:10px}.cb-heading h1{font-size:20px}.cb-heading p{font-size:10px}.cb-header>.primary{padding:10px;font-size:0;gap:0}.cb-header>.primary .cb-icon{width:20px;height:20px}.cb-emblem{width:38px;height:38px;border-radius:11px}.cb-body{display:block;overflow-y:auto}.cb-sidebar{max-height:285px;border-right:0;border-bottom:1px solid #ffffff10}.cb-tools{padding:16px 16px 8px}.cb-count{margin-top:10px}.cb-list{max-height:175px;flex:auto}.cb-detail{overflow:visible;padding:22px 20px}.cb-detail h2{font-size:24px}.cb-detail .cb-avatar{width:52px;height:52px;font-size:19px}.cb-status{margin:0 18px;padding:10px 0;font-size:11px;min-height:32px}.cb-footer{padding:11px 18px;font-size:9px}.cb-caption{margin-top:20px}.cb-address{font-size:11px!important}.cb-empty{padding:25px 12px}.cb-form label{margin-top:17px}}
    @media(prefers-reduced-motion:reduce){#book-modal *{scroll-behavior:auto!important;transition:none!important;animation:none!important}}
  `;document.head.appendChild(style);
  var modal=element('div','cb-overlay');modal.id='book-modal';
  modal.innerHTML='<section class="cb-dialog" role="dialog" aria-modal="true" aria-labelledby="contactsTitle"><header class="cb-header"><div class="cb-emblem">'+icon('book')+'</div><div class="cb-heading"><h1 id="contactsTitle">Address book</h1><p>Your people. Your wallets. One place.</p></div><button class="cb-button primary" id="contactsNew" aria-label="Add contact">'+icon('plus')+'New contact</button><button class="cb-button cb-close" id="contactsClose" aria-label="Close address book">'+icon('close')+'</button></header><div class="cb-body"><aside class="cb-sidebar"><div class="cb-tools"><div class="cb-search">'+icon('search')+'<input id="contactsSearch" type="search" placeholder="Search name or address" aria-label="Search contacts" autocomplete="off"></div><div class="cb-filters"><button id="contactsAll" aria-pressed="true">All contacts</button><button id="contactsFavorites" aria-pressed="false">Favorites</button></div><div class="cb-count" id="contactsCount"></div></div><div class="cb-list" id="contactsList" aria-label="Saved contacts"></div></aside><main class="cb-detail" id="contactsDetail"></main></div><div class="cb-status" id="contactsStatus" role="status" aria-live="polite"></div><footer class="cb-footer"><span>Saved in this browser · no cloud sync</span><span>SIGIL native network</span></footer></section>';
  old.replaceWith(modal);
  function avatar(c) { var words=name(c).replace(/\([^)]*\)/g,'').match(/[\p{L}\p{N}]+/gu)||['?'];return element('span','cb-avatar',(words[0][0]+(words.length>1?words[words.length-1][0]:'')).toUpperCase()); }
  function empty(target,title,text,create) { target.replaceChildren();var e=element('div','cb-empty');e.innerHTML=icon('book');e.append(element('h3','',title),element('p','',text));if(create)e.append(button('Add your first contact','plus','cb-button primary',function(){editor(null);}));target.append(e); }
  function renderList(snapshot) {
    var entries=snapshot.entries,q=$('contactsSearch').value.trim().toLowerCase();
    var rows=entries.map(function(c,i){return {c:c,i:i};}).filter(function(r){return (S.filter!=='favorites'||(r.c&&r.c.favorite))&&(!q||name(r.c).toLowerCase().includes(q)||address(r.c).includes(q));});
    rows.sort(function(a,b){return name(a.c).localeCompare(name(b.c))||address(a.c).localeCompare(address(b.c));});
    $('contactsCount').textContent=rows.length+' '+(rows.length===1?'contact':'contacts')+(rows.length!==entries.length?' · '+entries.length+' total':'')+' · A–Z';
    $('contactsAll').setAttribute('aria-pressed',String(S.filter==='all'));$('contactsFavorites').setAttribute('aria-pressed',String(S.filter==='favorites'));
    var list=$('contactsList');list.replaceChildren();
    if(!rows.length){empty(list,!entries.length?'Your address book starts here':q?'No matching contacts':'No favorites yet',!entries.length?'Save a name once, find the right wallet next time.':q?'Try another name or a few characters from the address.':'Star a contact to keep it close.',false);if(S.mode==='view')S.selected=null;return;}
    if(S.mode==='view'&&!rows.some(function(r){return r.i===S.selected;}))S.selected=rows[0].i;
    rows.forEach(function(r){var b=element('button','cb-contact');b.type='button';b.setAttribute('aria-current',String(r.i===S.selected));b.append(avatar(r.c));var text=element('span','cb-contact-text');text.append(element('span','cb-contact-name',name(r.c)),element('span','cb-contact-address',short(address(r.c))));b.append(text);if(r.c&&r.c.favorite){var h=element('span');h.innerHTML=icon('star');h.firstChild.classList.add('starred');b.append(h.firstChild);}b.onclick=function(){if(S.mode!=='view'){say('Save or cancel your edit before choosing another contact.',true);return;}S.selected=r.i;say('');render();};list.append(b);});
  }
  function detail(snapshot) {
    if(S.mode!=='view')return;
    var host=$('contactsDetail');host.replaceChildren();var c=snapshot.entries[S.selected];
    if(S.selected==null){empty(host,snapshot.entries.length?'Find your next recipient':'A name is easier to remember','Contacts stay on this browser. Add native SIGIL wallets and open Send with the recipient already filled in.',!snapshot.entries.length);return;}
    var a=address(c),ok=valid(a),top=element('div','cb-detail-top');top.append(avatar(c));
    var favorite=button(c&&c.favorite?'Favorited':'Favorite','star','cb-button'+(c&&c.favorite?' starred':''),function(){try{var snap=read();var item=snap.entries[S.selected];if(!item||typeof item!=='object')throw new Error('Edit this contact to repair its saved address.');snap.entries[S.selected]=Object.assign({},item,{favorite:!item.favorite});write(snap,snap.entries);render();say(item.favorite?'Removed from favorites.':'Added to favorites.');}catch(e){say(e.message,true);}});favorite.setAttribute('aria-pressed',String(!!(c&&c.favorite)));top.append(favorite);host.append(top,element('h2','',name(c)),element('span','cb-network',ok?'SIGIL · native address':'Address needs attention'),element('div','cb-caption','Wallet address'));
    var full=element('textarea','cb-address');full.readOnly=true;full.rows=3;full.value=a;full.setAttribute('aria-label','Full contact address');host.append(full,element('p','cb-address-note',ok?'Native SIGIL address · 64 hexadecimal characters. Check the full address before sending.':'This saved entry is incomplete or invalid. Edit it before sending.'));
    var actions=element('div','cb-actions');var send=button('Send to contact','send','cb-button primary',function(){if(typeof window.openSend!=='function'){say('Send is unavailable. Reload the wallet and try again.',true);return;}close();window.openSend();var recipient=$('uniAddr');if(recipient){recipient.value=a;recipient.dispatchEvent(new Event('input',{bubbles:true}));recipient.focus();}});send.disabled=!ok;actions.append(send,button('Copy address','copy','cb-button',async function(){try{if(!navigator.clipboard)throw new Error();await navigator.clipboard.writeText(a);say('Address copied.');}catch(_){full.focus();full.select();say('Clipboard unavailable. The address is selected so you can copy it manually.',true);}}));host.append(actions);
    var tools=element('div','cb-actions');tools.append(button('Edit contact','edit','cb-button',function(){editor(S.selected);}),button('Remove','trash','cb-button danger',async function(){try{var captured=read();var index=S.selected;if(JSON.stringify(captured.entries[index])!==JSON.stringify(c))throw new Error('This contact changed. Review it before removing it.');if(!(await window.sigilConfirm('Remove “'+name(c)+'” from this browser’s address book?')))return;captured.entries.splice(index,1);write(captured,captured.entries);S.selected=null;render();say('Contact removed.');}catch(e){say(e.message,true);render();}}));host.append(element('hr','cb-divider'),tools);
    var local=element('p','cb-local');local.innerHTML=icon('shield');local.append(document.createTextNode('Your address book is stored in this browser. Contact names are not included when you open Send.'));host.append(local);
  }
  function render() {try{var snapshot=read();renderList(snapshot);detail(snapshot);}catch(e){$('contactsList').replaceChildren();empty($('contactsDetail'),'Contacts could not be loaded','Browser storage may be blocked or the saved data may need repair. Existing data has not been replaced.',false);say(e.message,true);}}
  function editor(index) {
    try{
      var snap=read();S.mode=index==null?'new':'edit';S.selected=index;S.base=snap;say('');
      var c=index==null?null:snap.entries[index],host=$('contactsDetail');host.replaceChildren();var form=element('form','cb-form');
      form.innerHTML='<h2>'+(index==null?'New contact':'Edit contact')+'</h2><p class="cb-address-note">Save a native SIGIL wallet with a name you recognize.</p><label for="contactName">Contact name</label><input id="contactName" placeholder="e.g. Cold wallet or Torbjørn" maxlength="80" autocomplete="off" required><div class="cb-inline"><label for="contactAddress">SIGIL wallet address</label><button class="cb-button" id="contactUseMine" type="button">Use my wallet</button></div><textarea id="contactAddress" class="cb-address" rows="3" placeholder="64 hexadecimal characters" autocomplete="off" spellcheck="false" required></textarea><p class="cb-helper">Native SIGIL only. Polygon and other 0x addresses use a different network.</p><div class="cb-actions"><button class="cb-button primary" type="submit">'+icon('plus')+'Save contact</button><button class="cb-button" id="contactCancel" type="button">Cancel</button></div>';
      host.append(form);$('contactName').value=c?name(c):'';$('contactAddress').value=c?address(c):'';
      $('contactCancel').onclick=function(){S.mode='view';S.base=null;say('');render();};
      $('contactUseMine').onclick=function(){var me=normalize(window.MINE_ADDR||window.ADDR);if(!valid(me)){say('This wallet does not have a loaded SIGIL address yet.',true);return;}$('contactAddress').value=me;say('Current wallet address filled in.');};
      form.onsubmit=function(e){e.preventDefault();save();};renderList(snap);$('contactName').focus();
      if(window.innerWidth<761)host.scrollIntoView({block:'start',behavior:'auto'});
    }catch(e){S.mode='view';say(e.message,true);}
  }
  function save() {
    if(!S.base||S.mode==='view')return;
    var label=$('contactName').value.trim(),addr=normalize($('contactAddress').value);
    if(!label){say('Give this contact a name.',true);$('contactName').focus();return;}
    if(!valid(addr)){say('Enter a non-zero native SIGIL address: exactly 64 hexadecimal characters.',true);$('contactAddress').focus();return;}
    try{
      var snapshot=S.base,idx=S.mode==='edit'?S.selected:null;
      if(snapshot.entries.some(function(c,i){return i!==idx&&address(c)===addr;}))throw new Error('That address is already saved. Choose its existing contact instead.');
      var entries=snapshot.entries.slice(),item=Object.assign({},idx==null?{}:entries[idx],{label:label,addr:addr});
      if(idx==null){entries.unshift(item);idx=0;}else entries[idx]=item;
      write(snapshot,entries);S.selected=idx;S.mode='view';S.base=null;$('contactsSearch').value='';S.filter='all';render();say('Contact saved.');
    }catch(e){say(e.message||'Could not save. Browser storage may be full or blocked.',true);}
  }
  function open() {if(modal.style.display==='flex')return;S.focus=document.activeElement;S.overflow=document.body.style.overflow;document.body.style.overflow='hidden';S.mode='view';S.base=null;S.filter='all';$('contactsSearch').value='';modal.style.display='flex';say('');render();$('contactsSearch').focus();}
  function close() {modal.style.display='none';S.mode='view';S.base=null;document.body.style.overflow=S.overflow;if(S.focus&&S.focus.isConnected)S.focus.focus();}
  window.openBook=open;window.closeBook=close;window.abkRender=render;window.abkAdd=save;
  $('contactsNew').onclick=function(){if(S.mode!=='view'){say('Save or cancel your current edit first.',true);return;}editor(null);};$('contactsClose').onclick=close;
  $('contactsSearch').oninput=function(){if(S.mode==='view')say('');render();};
  $('contactsAll').onclick=function(){S.filter='all';render();};$('contactsFavorites').onclick=function(){S.filter='favorites';render();};
  modal.onclick=function(e){if(e.target===modal)close();};
  document.addEventListener('keydown',function(e){
    if(modal.style.display!=='flex')return;var ask=$('ask-modal');if(ask&&getComputedStyle(ask).display!=='none')return;
    if(e.key==='Escape'){e.preventDefault();e.stopImmediatePropagation();if(S.mode!=='view'){S.mode='view';S.base=null;say('Edit cancelled.');render();}else close();}
    if(e.key==='Tab'){var nodes=Array.from(modal.querySelectorAll('button,input,textarea,a[href]')).filter(function(n){return !n.disabled&&n.getClientRects().length;});if(!nodes.length)return;var first=nodes[0],last=nodes[nodes.length-1];if(e.shiftKey&&document.activeElement===first){e.preventDefault();last.focus();}else if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first.focus();}}
  },true);
  window.addEventListener('storage',function(e){if(e.key!==KEY&&e.key!==null)return;if(modal.style.display==='flex'){render();if(S.mode!=='view')say('Contacts changed in another tab. Cancel this edit to load the latest version.',true);}});
})();
