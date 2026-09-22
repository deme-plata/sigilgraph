import { chromium } from 'playwright-core';
const b = await chromium.launch({ args:['--no-sandbox'] });
const p = await b.newPage();
const errs = [];
p.on('console', m => { if (m.type()==='error') errs.push(m.text()); });
p.on('pageerror', e => errs.push('pageerror: '+e.message));
const reqs = [];
p.on('response', r => { if (r.url().includes('court.js')) reqs.push(r.status()+' '+r.url()); });
await p.goto('https://sigilgraph.org/sigil-wallet-tron-embedded.html', { waitUntil:'networkidle', timeout:60000 });
await p.waitForTimeout(2500);
const out = await p.evaluate(() => ({
  loaded: !!window.__sigilCourtLoaded,
  signRaw: typeof window.sigilSignRaw,
  deriveAuto: typeof window.sigilDeriveAuto,
  fab: !!document.getElementById('sc-fab'),
  fabBox: document.getElementById('sc-fab')?.getBoundingClientRect(),
  scripts: [...document.querySelectorAll('script[src]')].map(s=>s.src).filter(s=>s.includes('court')),
  bodyKids: document.body.children.length,
}));
console.log(JSON.stringify({out, reqs, errs: errs.slice(0,6)}, null, 1));
await b.close();
