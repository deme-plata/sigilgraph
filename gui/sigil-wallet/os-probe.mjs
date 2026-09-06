import { chromium } from 'playwright-core';
const url = process.argv[2];
const b = await chromium.launch({ args:['--no-sandbox','--disable-dev-shm-usage'] });
const p = await b.newPage();
const errs = [], msgs = [], failed = [];
p.on('pageerror', e => errs.push(String(e).slice(0,300)));
p.on('console', m => { if (m.type()==='error'||m.type()==='warning') msgs.push(m.type()+': '+m.text().slice(0,200)); });
p.on('requestfailed', r => failed.push(r.url().slice(0,110)+' :: '+(r.failure()?.errorText||'')));
try { await p.goto(url, { waitUntil:'load', timeout:30000 }); } catch(e){ console.log('GOTO FAIL:', String(e).slice(0,200)); }
await p.waitForTimeout(9000);
const state = await p.evaluate(() => ({
  bootStillPresent: !!document.getElementById('boot'),
  deskHidden: document.getElementById('desk')?.hidden,
  barHidden : document.getElementById('bar')?.hidden,
  bootLines : document.querySelectorAll('#bootlog .l').length,
  lastLine  : document.querySelector('#bootlog .l:last-child')?.innerText?.slice(0,90) || null,
  title     : document.title,
}));
console.log('STATE', JSON.stringify(state,null,1));
console.log('PAGEERRORS', errs.length ? errs : 'none');
console.log('CONSOLE', msgs.slice(0,8));
console.log('REQ_FAILED', failed.slice(0,8));
await b.close();
