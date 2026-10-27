// The sigilgraph.org front page wallet card, v3 (2026-10-27, Viktor: "bip39 argonid password hybrid in
// conjunction with mnemonic when creating new wallet on sigilgraph").
//
// A v3 wallet is 12 real BIP39 words plus an OPTIONAL password, stretched with Argon2id (64 MiB) by
// crates/sigil-wallet-phrase — the same code sigil-top and the phone run (/sigil-wallet-phrase.mjs).
// The words carry BIP39's check word, so a typo is caught instead of silently opening an empty wallet;
// the password makes found-on-paper words useless on their own.
//
// This module replaces /sigil-import-12.mjs ("I already have a wallet") and, when CREATE_V3 is on, the
// React "Generate" button. It is plain DOM mounted into the React card (the landing page's React
// source is in no repository; see sigil-scratch/sigil-wallet-phrase-1014/HANDOFF.md).
//
// Rollout switch. Off = stage 1, "readers first": every wallet (v3 or older) OPENS here, but new wallets
// are still made by the old Generate button (v2), so nobody can create a wallet that an older sigil-top
// or phone app would open as an empty one. On = stage 2: Create makes v3. Viktor decides when.
const CREATE_V3 = false;

import * as P from '/sigil-wallet-phrase.mjs?v=1';
import { sha512 } from '/sigil-sha512.mjs';
import * as ed from '/sigil-ed25519.mjs';
ed.etc.sha512Sync = (...m) => sha512(ed.etc.concatBytes(...m));

const ID = 'sigil-wallet-v3';
const hex = u8 => Array.from(u8, b => b.toString(16).padStart(2, '0')).join('');
const addressOf = secret => hex(ed.getPublicKey(secret));
const INPUT = 'width:100%;box-sizing:border-box;margin-top:8px;padding:10px 12px;border-radius:10px;background:rgba(0,0,0,.35);' +
  'border:1px solid rgba(255,255,255,.16);color:#e8f2ee;font:14px/1.5 ui-monospace,monospace';
const SMALL = 'margin:8px 0 0;font-size:12px;line-height:1.5;color:rgba(232,242,238,.6)';
let wrap = null;

function node(tag, props, kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (k === 'style') e.style.cssText = v; else if (k === 'text') e.textContent = v; else e.setAttribute(k, v);
  }
  for (const k of kids || []) if (k != null) e.appendChild(typeof k === 'string' ? document.createTextNode(k) : k);
  return e;
}
const loginLink = text => node('a', { href: '/enter-sigil.html', style: 'color:#7dd3c0;text-decoration:underline' }, [text || 'full login page']);
const btn = (text, primary, extra) => node('button', { type: 'button', class: primary ? 'btn btn-primary' : 'btn',
  style: 'width:100%;justify-content:center;margin-top:10px' + (primary ? '' : ';background:transparent;border:1px solid rgba(255,255,255,.2);color:#e8f2ee') + (extra || '') }, [text]);
function status() {
  const el = node('div', { role: 'status', 'aria-live': 'polite', style: 'min-height:1.2em;margin-top:8px;font-size:13px;line-height:1.45;color:rgba(232,242,238,.8)' });
  el.say = (...parts) => { el.textContent = ''; for (const p of parts) el.appendChild(typeof p === 'string' ? document.createTextNode(p) : p); };
  return el;
}
function passwordField(label, autocomplete) {
  const inp = node('input', { type: 'password', autocomplete, autocapitalize: 'off', autocorrect: 'off', spellcheck: 'false', 'aria-label': label, placeholder: label, style: INPUT });
  return inp;
}
const later = () => new Promise(r => setTimeout(r, 30));   // let the "working…" line paint before Argon2id runs

// The chain's opinion of an address: does it have a balance or published private-payment keys?
async function probe(address) {
  let balance = null, registered = false, reachable = false;
  try {
    const j = await (await fetch('/v1/balance?wallet=' + address, { cache: 'no-store' })).json();
    if (j && j.ok && j.data) { balance = BigInt(j.data.balance || '0'); reachable = true; }
  } catch (_) {}
  try {
    const j = await (await fetch('/v1/shielded/address?wallet=' + address, { cache: 'no-store' })).json();
    if (j && typeof j.ok === 'boolean') reachable = true;
    registered = !!(j && j.ok && j.pk_shield);
  } catch (_) {}
  return { balance, registered, reachable, known: registered || (balance !== null && balance > 0n) };
}

// What the login page's enterWith() writes, plus which kind of wallet it is. A v3 wallet keeps only its
// derived 64-hex key in this browser — never the words, never the password.
function enter(secretOrPhrase, address, scheme) {
  localStorage.setItem('sigil-wallet-seed', secretOrPhrase);
  localStorage.setItem('sigil-wallet-mnemonic', secretOrPhrase);
  localStorage.setItem('sigil-wallet-address', address);
  if (scheme === 'v3') localStorage.setItem('sigil-wallet-scheme', 'v3'); else localStorage.removeItem('sigil-wallet-scheme');
  try { sessionStorage.removeItem('sigil-adopted'); } catch (_) {}
  location.href = '/sigil-wallet-tron-embedded.html';
}

// ── Open an existing wallet ─────────────────────────────────────────────────────────────────────────
function buildOpen() {
  const ta = node('textarea', { rows: '3', autocomplete: 'off', autocapitalize: 'off', autocorrect: 'off', spellcheck: 'false',
    'aria-label': 'Your 12 words', placeholder: 'your twelve words, separated by spaces', style: INPUT + ';resize:vertical' });
  const pw = passwordField('password — only if your wallet has one', 'current-password');
  const msg = status();
  const go = btn('Open my wallet', true, ';margin-top:8px');
  const foot = node('p', { style: SMALL }, ['Your words and password stay in this browser. An older 6-word phrase or a 64-character key? Use the ', loginLink(), '.']);
  const form = node('div', { style: 'display:none' }, [ta, pw, go, msg, foot]);
  const toggle = btn('I already have a wallet', false);
  toggle.setAttribute('aria-expanded', 'false');
  toggle.addEventListener('click', () => {
    const show = form.style.display === 'none';
    form.style.display = show ? 'block' : 'none';
    toggle.setAttribute('aria-expanded', String(show));
    if (show) ta.focus();
  });
  go.addEventListener('click', () => openWallet(ta, pw, msg, go));
  for (const el of [ta, pw]) el.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); go.click(); } });
  return node('div', {}, [toggle, form]);
}

async function openWallet(ta, pwEl, msg, go) {
  const say = msg.say, words = ta.value.trim().split(/\s+/).filter(Boolean), password = pwEl.value;
  if (!words.length) { say('Type or paste your 12 words.'); return; }
  if (words.length < 12 || /^(0x)?[0-9a-fA-F]{64}$/.test(ta.value.trim())) {
    say(`That is ${words.length} word${words.length === 1 ? '' : 's'}. This box opens 12-word wallets. For an older 6-word phrase or a 64-character key, use the `, loginLink(), '.');
    return;
  }
  go.disabled = true;
  try {
    await P.ready;
    if (!P.loaded()) { say('Could not load the wallet code. Reload the page and try again.'); return; }
    say(password ? 'Checking your words and password…' : 'Checking your words…'); await later();
    let cs;
    try { cs = P.candidates(words.join(' '), password); }
    catch (e) { say(e.message); return; }
    const found = cs.map(c => ({ scheme: c.scheme, secret: c.secret, address: addressOf(c.secret) }));

    if (found.length === 2) {
      // Valid BIP39 words, no password: a new (v3) wallet, or one of the ~3 % of older wallets whose words
      // happen to be valid BIP39 too. The chain says which one has been used.
      say('Asking the chain which wallet these words belong to…');
      const [v3, v2] = found, [p3, p2] = await Promise.all([probe(v3.address), probe(v2.address)]);
      if (p2.known && !p3.known) { say(`Opening your wallet ${v2.address.slice(0, 10)}… (made before October 2026)`); enter(words.join(' '), v2.address, 'v2'); return; }
      if (!p3.known && !p2.known) {
        const old = node('a', { href: '#', style: 'color:#7dd3c0;text-decoration:underline' }, ['open the old-style wallet instead']);
        old.addEventListener('click', e => { e.preventDefault(); enter(words.join(' '), v2.address, 'v2'); });
        say(`Opening ${v3.address.slice(0, 10)}… — it has no coins yet. Made this wallet before 27 October 2026? Then `, old, '.');
        await new Promise(r => setTimeout(r, 2500));
      }
      enter(hex(v3.secret), v3.address, 'v3');
      return;
    }

    const [c] = found;
    if (c.scheme === 'v3') { say(`Opening wallet ${c.address.slice(0, 10)}…${c.address.slice(-6)}`); enter(hex(c.secret), c.address, 'v3'); return; }

    // An older (v2) wallet: hashed exactly as typed, so check it the way the old box did — plus one new
    // guard: words that are ALL BIP39 words but fail the check word are much more likely a mistyped new
    // wallet than an old one.
    if (words.length !== 12) {
      say(`That is ${words.length} words. Older SIGIL wallets have exactly 12; for anything else use the `, loginLink(), '.');
      return;
    }
    const mod = await import('/sigil-create-12.mjs?v=2'), list = new Set(mod.WL);
    const chk = P.check(words.join(' '));
    const bad = words.findIndex(w => !list.has(w));
    if (bad >= 0) {
      const w = words[bad], low = w.toLowerCase();
      if (!chk.ok && chk.code === 4) { say(chk.message); return; }   // all BIP39 words: a mistyped new wallet
      say(list.has(low) ? `Word ${bad + 1} “${w}” has a capital letter. The words are exact, and in your phrase it is “${low}”.`
        : `Word ${bad + 1} “${w}” is not a recovery word. A phone keyboard may have changed it.`);
      return;
    }
    if (!chk.ok && chk.code === 4) {
      const p = await probe(c.address);
      if (!p.known) {
        const anyway = node('a', { href: '#', style: 'color:#7dd3c0;text-decoration:underline' }, ['open it as an older wallet']);
        anyway.addEventListener('click', e => { e.preventDefault(); enter(words.join(' '), c.address, 'v2'); });
        say('These words open an older-style wallet that has no history yet, and they are not valid new-style words. ' +
          'Made this wallet before 27 October 2026? Then ', anyway, '. A new wallet? Then one word is probably mistyped, or two are swapped.');
        return;
      }
    }
    say(`Opening wallet ${c.address.slice(0, 10)}…${c.address.slice(-6)}`);
    enter(words.join(' '), c.address, c.scheme);
  } catch (e) {
    say('This browser could not open the wallet here (storage blocked?). Use the ', loginLink(), '.');
  } finally { go.disabled = false; }
}

// ── Create a new (v3) wallet ────────────────────────────────────────────────────────────────────────
function buildCreate() {
  const start = btn('✦ Create a new wallet', true);
  const body = node('div', { style: 'display:none' });
  start.addEventListener('click', async () => {
    await P.ready;
    if (!P.loaded()) { start.textContent = 'Could not load the wallet code — reload the page'; return; }
    start.style.display = 'none';
    body.style.display = 'block';
    showWords(body, P.generateWords());
  });
  return node('div', {}, [start, body]);
}

function showWords(body, words) {
  body.textContent = '';
  const grid = node('div', { class: 'phrase-box', style: 'display:grid;grid-template-columns:repeat(3,1fr);gap:6px 14px;text-align:left' },
    words.split(' ').map((w, i) => node('span', {}, [node('i', { style: 'opacity:.45;font-style:normal;font-size:.8em;margin-right:6px' }, [String(i + 1)]), w])));
  const pw1 = passwordField('password (optional)', 'new-password'), pw2 = passwordField('the same password again', 'new-password');
  const show = node('label', { style: SMALL + ';display:flex;gap:8px;align-items:center;cursor:pointer' }, [node('input', { type: 'checkbox' }), 'show password']);
  show.firstChild.addEventListener('change', e => { pw1.type = pw2.type = e.target.checked ? 'text' : 'password'; });
  const wrote = node('label', { style: 'display:flex;gap:8px;align-items:flex-start;margin-top:12px;font-size:13px;line-height:1.45;color:#e8f2ee;cursor:pointer' },
    [node('input', { type: 'checkbox', style: 'margin-top:3px' }), 'I have written down the 12 words in this order — and my password, if I chose one.']);
  const msg = status();
  const make = btn('Create my wallet', true);
  make.disabled = true;
  wrote.firstChild.addEventListener('change', e => { make.disabled = !e.target.checked; });

  body.append(
    node('div', { class: 'field-label' }, ['Your recovery words — write them down, never share them']), grid,
    node('div', { class: 'warn-line' }, ['These 12 words are a standard BIP39 phrase. The last word is a check word, so a mistyped word is caught when you open the wallet.']),
    node('div', { class: 'field-label', style: 'margin-top:14px' }, ['A password — optional, an extra lock']),
    node('p', { style: SMALL + ';margin-top:4px' }, ['With a password, someone who finds your words still cannot open the wallet. But the words alone will not open it for you either: ',
      node('b', {}, ['forget the password and the wallet is gone']), ' — nobody can reset it. Leave both boxes empty for no password.']),
    pw1, pw2, show, wrote, make, msg);

  make.addEventListener('click', async () => {
    const a = pw1.value, b = pw2.value;
    if (a !== b) { msg.say('The two passwords are not the same.'); return; }
    if (P.passwordHasEdgeWhitespace(a)) { msg.say('The password starts or ends with a space. Remove it — a hidden space is the easiest way to lock yourself out.'); return; }
    make.disabled = true;
    msg.say(a ? 'Making your wallet — stretching words and password (about half a second)…' : 'Making your wallet…'); await later();
    let c;
    try { c = P.candidates(words, a)[0]; } catch (e) { msg.say(e.message); make.disabled = false; return; }
    const address = addressOf(c.secret), secretHex = hex(c.secret);
    pw1.value = pw2.value = '';
    for (const el of [pw1, pw2, show, wrote, make]) el.style.display = 'none';
    msg.say('');
    const balance = node('span', { class: 'v' }, ['checking…']), reg = node('div', { class: 'status-line' }, ['Publishing your private receiving keys…']);
    const open = btn('Open my wallet ↗', true);
    open.addEventListener('click', () => enter(secretHex, address, 'v3'));
    body.append(node('div', { class: 'wallet-result' }, [
      node('div', { class: 'kv' }, [node('span', { class: 'k' }, ['Address']), node('span', { class: 'v' }, [address])]),
      node('div', { class: 'kv' }, [node('span', { class: 'k' }, ['Balance']), balance]),
      node('div', { class: 'kv' }, [node('span', { class: 'k' }, ['Kind']), node('span', { class: 'v' }, [a ? 'v3 · 12 words + password' : 'v3 · 12 words'])]),
    ]), reg, open);
    probe(address).then(p => { balance.textContent = p.balance === null ? 'node not reachable' : (Number(p.balance) / 1e10).toLocaleString(undefined, { maximumFractionDigits: 6 }) + ' SIGIL (testnet)'; });
    // Until the receiving keys are on chain nobody can pay this wallet privately (2026-10-15).
    import('/sigil-create-12.mjs?v=2').then(m => m.ensureRegistered({ priv: c.secret, address }, s => {
      reg.textContent = { registered: '✓ Ready to receive private payments.', submitted: 'Keys sent — waiting for the chain…',
        foreign: 'This address already has other keys published.', failed: 'Could not publish keys now — the wallet page will retry.',
        unknown: 'Could not confirm the keys yet — the wallet page will retry.' }[s] || s;
      reg.className = 'status-line' + (s === 'registered' ? ' ok' : '');
    })).catch(() => { reg.textContent = 'Could not publish keys now — the wallet page will retry.'; });
  });
}

function mount() {
  const card = document.querySelector('.wallet-card');
  if (!card) return;
  if (!wrap) {
    wrap = node('div', { id: ID }, [CREATE_V3 ? buildCreate() : null, buildOpen()]);
    if (CREATE_V3) {   // the React card's own generator (v2) steps aside
      document.head.appendChild(node('style', {}, [`.wallet-card > :not(h3):not(.sub):not(#${ID}){display:none!important}`]));
    }
  }
  if (!card.contains(wrap)) card.appendChild(wrap);   // the same element: typed words survive a re-render
}

mount();
new MutationObserver(mount).observe(document.getElementById('root') || document.body, { childList: true, subtree: true });
