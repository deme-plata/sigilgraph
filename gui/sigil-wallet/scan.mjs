// Browser-side shielded-note scanner — must byte-match flux_swarm_secret's sealed box.
import { blake3 } from '@noble/hashes/blake3';
import { x25519 } from '@noble/curves/ed25519.js';
import { chacha20poly1305 } from '@noble/ciphers/chacha.js';

const KDF_CTX   = 'flux-swarm-secret v1 sealed-box x25519 aead-key';
const NONCE_CTX = 'flux-swarm-secret v1 sealed-box x25519 aead-nonce';
const SUITE     = 'x25519-chacha20poly1305-blake3';
const NOTE_MAGIC = new TextEncoder().encode('SIGILNT1');
const ENC_DOMAIN = new TextEncoder().encode('sigil-shielded-enc-key-v1');

const hex2b = h => Uint8Array.from(h.match(/../g).map(x => parseInt(x, 16)));
const b2hex = b => [...b].map(x => x.toString(16).padStart(2, '0')).join('');

/** The note-delivery identity: a THIRD key from the one seed, domain-separated
 *  from the spend key so a viewing capability never implies spend authority. */
export function encIdentityFromSeed(seed32) {
  const h = blake3.create({});
  h.update(ENC_DOMAIN); h.update(seed32);
  const sk = h.digest();
  return { sk, pk: x25519.getPublicKey(sk) };
}

function deriveKeyNonce(ss, ephPk, recipientPk) {
  const km = new Uint8Array(96);
  km.set(ss, 0); km.set(ephPk, 32); km.set(recipientPk, 64);
  const key = blake3(km, { context: KDF_CTX, dkLen: 32 });
  const nonce = blake3(km, { context: NONCE_CTX, dkLen: 32 }).slice(0, 12);
  return { key, nonce };
}

/** Open one envelope. Returns null for anything not addressed to us — the common
 *  case, and it must stay cheap: a wallet runs this over every ciphertext. */
export function openEnvelope(envelopeJson, id) {
  let env; try { env = JSON.parse(envelopeJson); } catch { return null; }
  if (env.suite !== SUITE) return null;
  try {
    const ephPk = hex2b(env.eph_pk);
    const ss = x25519.getSharedSecret(id.sk, ephPk);
    const { key, nonce } = deriveKeyNonce(ss, ephPk, id.pk);
    return chacha20poly1305(key, nonce, new TextEncoder().encode(SUITE)).decrypt(hex2b(env.ct));
  } catch { return null; }   // wrong key -> AEAD tag fails; that IS the "not ours" signal
}

/** 24 bytes: MAGIC(8) || value LE u64 || blinding LE u64 */
export function decodeNote(pt) {
  if (!pt || pt.length !== 24) return null;
  for (let i = 0; i < 8; i++) if (pt[i] !== NOTE_MAGIC[i]) return null;
  const dv = new DataView(pt.buffer, pt.byteOffset, pt.byteLength);
  return { value: dv.getBigUint64(8, true), blinding: dv.getBigUint64(16, true) };
}

/** Every note in the pool this seed can open, with spends netted out.
 *
 * `spent` is the set from /v1/shielded/nullifiers (lowercase hex). A spent note is
 * NEVER removed from the pool — the commitment stays forever, which is what keeps the
 * anonymity set from shrinking — so without netting, a balance can only ever go UP.
 * That is wrong in the one direction that matters, so when the caller cannot supply
 * what is needed to net, this says so via `netted:false` instead of quietly returning
 * a gross number dressed up as a balance.
 *
 * `mimc` is the wallet's OWN shield implementation ({ compress2, deriveField, toWireHex,
 * PK_DOMAIN }) — injected rather than reimplemented here, so the nullifiers computed for
 * the balance are byte-identical to the ones the wallet publishes when it spends. Two
 * copies of a hash function is two chances to disagree.
 */
export function scanBalance(ciphertexts, seed32, opts = {}) {
  const { spent = null, mimc = null } = opts;
  const id = encIdentityFromSeed(seed32);
  const mine = [];
  let gross = 0n, net = 0n, spentValue = 0n;

  // nullifier(position) = compress2(spend_key, position)
  const spendKey = mimc ? mimc.deriveField(seed32, 'spend-key', 0) : null;
  const canNet = !!(spent && mimc && spendKey);

  for (let i = 0; i < ciphertexts.length; i++) {
    const ct = ciphertexts[i];
    if (!ct) continue;                       // no envelope -> not discoverable
    const note = decodeNote(openEnvelope(ct, id));
    if (!note) continue;                     // not ours; the AEAD tag decided
    gross += note.value;
    let isSpent = false;
    if (canNet) {
      const nf = mimc.toWireHex(mimc.compress2(spendKey, BigInt(i))).toLowerCase();
      isSpent = spent.has(nf);
    }
    if (isSpent) spentValue += note.value; else net += note.value;
    mine.push({ index: i, value: note.value, blinding: note.blinding, spent: isSpent });
  }
  return {
    balance: canNet ? net : gross,
    gross,
    spentValue: canNet ? spentValue : null,
    netted: canNet,
    notes: mine,
  };
}

/** Fetch the pool and return a netted balance. `base` is the node origin ('' = same-origin). */
export async function fetchShieldedBalance(seed32, mimc, base = '') {
  const [lv, nf] = await Promise.all([
    fetch(`${base}/v1/shielded/leaves`).then(r => r.json()),
    fetch(`${base}/v1/shielded/nullifiers`).then(r => r.json()).catch(() => null),
  ]);
  const spent = nf && nf.ok ? new Set(nf.nullifiers.map(x => x.toLowerCase())) : null;
  return scanBalance(lv.ciphertexts || [], seed32, { spent, mimc });
}
export { hex2b, b2hex };
