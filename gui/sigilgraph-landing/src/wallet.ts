// wallet.ts — the SAME derivation scheme the live SIGIL wallet already uses
// (gui/sigil-wallet-tron-embedded.html: window.sigilDeriveFrom / sigilSign),
// re-implemented against the real @noble packages instead of a vendored
// minified bundle. A wallet created here derives IDENTICALLY to one created
// in the existing wallet — same recovery phrase in, same address out.

import { sha3_256 } from '@noble/hashes/sha3';
import { sha512 } from '@noble/hashes/sha512';
import * as ed from '@noble/ed25519';

// @noble/ed25519 v2's sync API (getPublicKey/sign/verify) needs a sync
// SHA-512 wired in — the browser's native crypto.subtle is async-only.
ed.etc.sha512Sync = (...m: Uint8Array[]) => sha512(ed.etc.concatBytes(...m));

export interface DerivedWallet {
  priv: Uint8Array;
  address: string; // 64-hex, canonical lowercase — the raw ed25519 pubkey bytes
}

const toHex = (b: Uint8Array) => Array.from(b).map((x) => x.toString(16).padStart(2, '0')).join('');

/** Derive a wallet from ANY string — same scheme as the live wallet. */
export function deriveFromMnemonic(mnemonic: string): DerivedWallet {
  const priv = sha3_256(new TextEncoder().encode(mnemonic.trim()));
  const pub = ed.getPublicKey(priv);
  return { priv, address: toHex(pub) };
}

/** Sign the SAME `sigil-rpc/v1|<action>|<fields>|nonce=<n>` RPC message the
 * live wallet + sigil-api's DexBridge/UsdsBridge/etc. all verify against. */
export function sigilSign(priv: Uint8Array, action: string, fields: string[], reqNonce: number): string {
  const msg = `sigil-rpc/v1|${action}|${fields.join('|')}|nonce=${reqNonce}`;
  const sig = ed.sign(new TextEncoder().encode(msg), priv);
  return toHex(sig);
}

// A small, plain wordlist — NOT BIP39, deliberately: the derivation scheme
// above hashes whatever string it's given, so any memorable phrase works.
// Kept short and simple on purpose for a landing-page "try it" flow; the
// full wallet app can offer BIP39 later without changing this derivation.
const WORDS = [
  'ash','birch','cedar','delta','ember','fern','glow','harbor','iris','jade',
  'keel','lotus','maple','nova','oak','pine','quartz','river','sigil','tide',
  'umber','vale','willow','xenon','yarrow','zephyr','amber','basalt','coral','dawn',
  'echo','flint','granite','haven','ion','juniper','kelp','lumen','moss','north',
  'onyx','pearl','quill','ridge','slate','thorn','urn','violet','wren','yew',
];

/** A fresh, random 6-word recovery phrase. Client-side only — never sent
 * anywhere; only its SHA3-256 hash (the derived private key) ever signs
 * anything, and even that never leaves the browser. */
export function generatePhrase(): string {
  const bytes = crypto.getRandomValues(new Uint32Array(6));
  return Array.from(bytes, (n) => WORDS[n % WORDS.length]).join(' ');
}
