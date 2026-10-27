// sigil-wallet-phrase.mjs — the browser's door into crates/sigil-wallet-phrase (2026-10-27).
//
// Recovery words (+ an optional password) → the 32-byte wallet secret, computed by the SAME
// Rust code sigil-top and the phone run, compiled to wasm with a plain C ABI (no generated
// glue). Built by scripts/build-wallet-phrase-wasm.sh; served as /wasm/sigil_wallet_phrase.wasm.
//
//   await ready;                         // once
//   candidates(words, password)          // → [{ scheme: 'v3'|'v2'|'v1'|'raw', secret: Uint8Array(32) }, …]
//   check(words)                         // → { ok } or { ok:false, code, position, message }
//   generateWords()                      // → 12 fresh BIP39 words (v3)
//   deriveV2(phrase)                     // → Uint8Array(32), the v2 secret (~18× faster than JS Argon2id)
//
// Two candidates come back only for checksum-valid BIP39 words typed WITHOUT a password: the
// words could be a new (v3) wallet or, for ~2.9 % of old phrases, an old (v2) one. Pick by the
// address you expect, else by which address has history on chain. Every call is synchronous
// once `ready` has resolved, like the page's existing signing path.
const SCHEMES = ['raw', 'v1', 'v2', 'v3'];
const MESSAGES = {
  1: 'The recovery words are empty.',
  2: 'A recovery phrase has 12 words (or 15, 18, 21, 24).',
  3: 'is not a recovery word — check its spelling.',
  4: 'The words do not fit together — one word is probably mistyped, or two are swapped.',
  5: 'That is not a valid 64-character hex key.',
  6: 'Key stretching failed.',
  9: 'The text could not be read.',
};
const te = new TextEncoder(), td = new TextDecoder();
let X = null;

export async function init(source) {
  if (X) return true;
  const bytes = source instanceof Uint8Array || source instanceof ArrayBuffer ? source
    : await (await fetch(source || new URL('./wasm/sigil_wallet_phrase.wasm?v=1', import.meta.url))).arrayBuffer();
  X = (await WebAssembly.instantiate(bytes, {})).instance.exports;
  return true;
}
export const ready = typeof window === 'undefined' ? Promise.resolve(false) : init().catch(e => { console.warn('[wallet-phrase] wasm failed to load', e); return false; });
export const loaded = () => !!X;

function need() {
  if (!X) { const e = new Error('Wallet key code is still loading — try again in a moment.'); e.code = 'PHRASE_LOADING'; throw e; }
  return X;
}
function put(x, bytes) { const p = x.swp_alloc(bytes.length); new Uint8Array(x.memory.buffer, p, bytes.length).set(bytes); return p; }
function wipe(x, p, n) { x.swp_free(p, n); }   // the Rust side zeroes before freeing

function fail(x, rc, words) {
  let c = -rc, viaPassword = false;
  if (c >= 10) { viaPassword = true; c -= 10; }
  const pos = c === 3 ? x.swp_err_pos() : 0;
  let msg = MESSAGES[c] || ('Error ' + c);
  if (c === 3) { const w = String(words).trim().split(/\s+/)[pos - 1] || ''; msg = `Word ${pos} ("${w}") ${msg}`; }
  // With a password the words must be new-style (v3) words. The likeliest cause is a typo in them; the
  // other is an older wallet (no password) — say the first plainly, then the second.
  if (viaPassword) msg += ' Older SIGIL wallets (made before 27 October 2026) have no password — if this is one, leave the password empty.';
  const e = new Error(msg); e.code = c; e.position = pos; e.viaPassword = viaPassword; return e;
}

export function check(words) {
  const x = need(), b = te.encode(String(words)), p = put(x, b);
  try { const rc = x.swp_check(p, b.length); if (rc === 0) return { ok: true }; const e = fail(x, rc, words); return { ok: false, code: e.code, position: e.position, message: e.message }; }
  finally { wipe(x, p, b.length); }
}

export function candidates(words, password = '') {
  const x = need(), w = te.encode(String(words)), pw = te.encode(String(password));
  const pp = put(x, w), pq = put(x, pw), out = x.swp_alloc(66);
  try {
    const n = x.swp_candidates(pp, w.length, pq, pw.length, out);
    if (n < 0) throw fail(x, n, words);
    const o = new Uint8Array(x.memory.buffer, out, 66), r = [];
    for (let i = 0; i < n; i++) r.push({ scheme: SCHEMES[o[i * 33]], secret: o.slice(i * 33 + 1, i * 33 + 33) });
    return r;
  } finally { wipe(x, pp, w.length); wipe(x, pq, pw.length); wipe(x, out, 66); }
}

export function deriveV2(phrase) {
  const x = need(), b = te.encode(String(phrase)), p = put(x, b), out = x.swp_alloc(32);
  try { const rc = x.swp_derive_v2(p, b.length, out); if (rc) throw fail(x, rc, phrase); return new Uint8Array(x.memory.buffer, out, 32).slice(); }
  finally { wipe(x, p, b.length); wipe(x, out, 32); }
}

export function generateWords() {
  const x = need(), ent = crypto.getRandomValues(new Uint8Array(16)), p = put(x, ent), cap = 256, out = x.swp_alloc(cap);
  try { const n = x.swp_words(p, 16, out, cap); if (n < 0) throw fail(x, n, ''); return td.decode(new Uint8Array(x.memory.buffer, out, n)); }
  finally { ent.fill(0); wipe(x, p, 16); wipe(x, out, cap); }
}

export const passwordHasEdgeWhitespace = pw => String(pw) !== String(pw).trim();
