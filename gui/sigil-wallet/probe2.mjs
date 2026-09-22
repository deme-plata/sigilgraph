import pkg from '/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/node_modules/playwright-core/index.js';
const { chromium } = pkg;
const b = await chromium.launch({ args:['--no-sandbox'] });
const ctx = await b.newContext({ viewport:{width:1280,height:900} });
const p = await ctx.newPage();
const errs=[]; p.on('pageerror',e=>errs.push('pageerror: '+e.message));
p.on('console',m=>{if(m.type()==='error')errs.push(m.text());});
const reqs=[]; p.on('response',r=>{if(r.url().includes('court'))reqs.push(r.status()+' '+r.url());});
// seed a wallet so the page does not bounce to the login gate
await p.goto('https://sigilgraph.org/', { waitUntil:'domcontentloaded', timeout:60000 });
await p.evaluate(()=>{
  localStorage.setItem('sigil-wallet-mnemonic','abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about');
  localStorage.setItem('sigil-wallet-address','2f2c1d0f3b6ea9d63e6e1a0b4c8a7d5e9f0a1b2c3d4e5f60718293a4b5c6d7e8');
});
await p.goto('https://sigilgraph.org/sigil-wallet-tron-embedded.html', { waitUntil:'networkidle', timeout:60000 });
await p.waitForTimeout(3000);
const s1 = await p.evaluate(()=>({
  url: location.href, title: document.title,
  loaded: !!window.__sigilCourtLoaded, signRaw: typeof window.sigilSignRaw,
  deriveAuto: typeof window.sigilDeriveAuto, fab: !!document.getElementById('sc-fab'),
}));
console.log('AFTER LOAD', JSON.stringify(s1));
if (s1.fab) {
  await p.click('#sc-fab'); await p.waitForTimeout(3500);
  await p.screenshot({ path:'/home/storage/sigil-scratch/sigil-court/ui/court-modal.png' });
  const s2 = await p.evaluate(()=>({ open: document.getElementById('sc-ov')?.classList.contains('on'), body: (document.getElementById('sc-body')?.innerText||'').slice(0,420) }));
  console.log('MODAL', JSON.stringify(s2, null, 1));
}
console.log('REQS', JSON.stringify(reqs), 'ERRS', JSON.stringify(errs.slice(0,5)));
await b.close();
