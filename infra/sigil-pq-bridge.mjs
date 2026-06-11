// LANE-AD: POST-QUANTUM HYBRID session layer for the SIGIL browser↔mesh channel.
//
// ── WHAT THIS IS (documented hybrid, per the lane spec) ─────────────────────
// js-libp2p has no ML-KEM/hybrid noise handshake yet (PQ-noise is emerging,
// not standard). So the libp2p channel KEEPS classical noise (X25519 +
// ChaCha20-Poly1305) for connection encryption, and THIS module layers an
// ML-KEM-1024 (Kyber) key-encapsulation ON TOP of the established channel:
//
//   browser wallet ──(noise-encrypted libp2p stream)──> bridge
//        │  /sigil/g0/pq-hybrid/1.0.0 handshake:
//        │    W→B HELLO_REQ { nonce_w, wallet ML-DSA-87 pk }
//        │    B→W HELLO     { ML-KEM-1024 pk, nonce_b }
//        │    W→B ENCAP     { kem ciphertext, ML-DSA-87 sig over transcript }
//        │    B→W CONFIRM   { AEAD tag over transcript under the session key }
//        ▼
//   session_key = HKDF-SHA256(
//       ikm  = ML-KEM-1024 shared secret,
//       salt = SHA-256(nonce_w ‖ nonce_b ‖ walletPeerId ‖ bridgePeerId),  ← channel binding
//       info = "sigil/g0/pq-hybrid/v1")
//
// SECURITY CLAIM (the honest version): traffic protected by the session key
// is confidential against an attacker who breaks X25519 (quantum) but not
// ML-KEM-1024, AND against one who breaks ML-KEM but not noise — they must
// break BOTH layers. The handshake transcript is signed with the wallet's
// ML-DSA-87 (Dilithium5) identity, so the wallet end is PQ-authenticated.
// The bridge end is authenticated by (a) the noise channel to the pinned
// bridge peer-id from bridge-addr.json, and (b) the TLS origin serving
// bridge-pq.json with the same KEM pk — two independent anchors — plus the
// CONFIRM tag, which only the true KEM secret-key holder can produce.
//
// WHAT IT IS NOT: it does not retrofit the noise handshake itself; gossipsub
// broadcast topics (blocks/balance fan-out) stay noise-only — they are public
// chain data. The PQ session protects the direct wallet↔bridge lane (and
// gives AC/AE a `seal/open` primitive for anything confidential).
//
// Keys: the ML-KEM keypair is persisted via its 64-byte seed at
// bridge-pq-kem.json so the published pk is stable across restarts,
// mirroring the LANE-AB identity persistence. The pk + sha256 fingerprint is
// published to the web roots as bridge-pq.json, next to bridge-addr.json.

import { readFileSync, writeFileSync } from 'node:fs'
import { randomBytes } from 'node:crypto'
import { ml_kem1024 } from '@noble/post-quantum/ml-kem.js'
import { ml_dsa87 } from '@noble/post-quantum/ml-dsa.js'
import { hkdf } from '@noble/hashes/hkdf.js'
import { sha256 } from '@noble/hashes/sha2.js'
import { chacha20poly1305 } from '@noble/ciphers/chacha'

export const PQ_PROTOCOL = '/sigil/g0/pq-hybrid/1.0.0'
const PQ_INFO = 'sigil/g0/pq-hybrid/v1'
const KEM_SEED_FILE = process.env.SIGIL_BRIDGE_PQ_KEY || '/home/orobit/sigil-bridge/bridge-pq-kem.json'
const WEB_ROOTS = [
  '/home/orobit/q-narwhalknight/dist-final',
  '/home/orobit/q-narwhalknight/dist-fluxapp',
  '/home/orobit/q-narwhalknight/.fluxapp-upper',
]

const te = new TextEncoder()
const hex = (b) => Buffer.from(b).toString('hex')
const unhex = (s) => new Uint8Array(Buffer.from(s, 'hex'))

// ── persistent ML-KEM-1024 keypair (64-byte seed → deterministic keygen) ────
function loadOrMintKem() {
  try {
    const seed = unhex(JSON.parse(readFileSync(KEM_SEED_FILE, 'utf8')).seed)
    const kp = ml_kem1024.keygen(seed)
    console.log('🔐 PQ: ML-KEM-1024 keypair LOADED (stable) from', KEM_SEED_FILE)
    return kp
  } catch {
    const seed = new Uint8Array(randomBytes(64))
    const kp = ml_kem1024.keygen(seed)
    writeFileSync(KEM_SEED_FILE, JSON.stringify({ v: 1, alg: 'ML-KEM-1024', seed: hex(seed) }))
    console.log('🔐 PQ: ML-KEM-1024 keypair MINTED + saved →', KEM_SEED_FILE)
    return kp
  }
}

// ── length-prefixed JSON frames over a libp2p stream (4-byte BE length) ─────
function frame(obj) {
  const body = te.encode(JSON.stringify(obj))
  const out = new Uint8Array(4 + body.length)
  new DataView(out.buffer).setUint32(0, body.length, false)
  out.set(body, 4)
  return out
}

// Incremental frame reader over the stream's async source (handles partial
// chunks AND multiple frames per chunk).
function makeReader(source) {
  let buf = new Uint8Array(0)
  const iter = source[Symbol.asyncIterator]()
  return async function read() {
    for (;;) {
      if (buf.length >= 4) {
        const len = new DataView(buf.buffer, buf.byteOffset).getUint32(0, false)
        if (len > 1 << 20) throw new Error('pq frame too large')
        if (buf.length >= 4 + len) {
          const body = buf.slice(4, 4 + len)
          buf = buf.slice(4 + len)
          return JSON.parse(new TextDecoder().decode(body))
        }
      }
      const { value, done } = await iter.next()
      if (done) throw new Error('pq stream closed mid-handshake')
      // js-libp2p yields Uint8ArrayList or Uint8Array
      const chunk = value.subarray ? value.subarray() : new Uint8Array(value)
      const next = new Uint8Array(buf.length + chunk.length)
      next.set(buf, 0); next.set(chunk, buf.length)
      buf = next
    }
  }
}

// Async frame queue → stream.sink (sink consumes exactly ONE source, so all
// outbound frames flow through this generator).
function makeWriter(stream) {
  const q = []
  let notify = null
  let closed = false
  const gen = (async function* () {
    for (;;) {
      while (q.length) yield q.shift()
      if (closed) return
      await new Promise((res) => { notify = res })
    }
  })()
  const done = stream.sink(gen)
  return {
    write(obj) { q.push(frame(obj)); if (notify) { const n = notify; notify = null; n() } },
    async close() { closed = true; if (notify) { const n = notify; notify = null; n() } await done.catch(() => {}) },
  }
}

function deriveSessionKey(sharedSecret, nonceW, nonceB, walletPeer, bridgePeer) {
  const salt = sha256(Buffer.concat([
    Buffer.from(nonceW), Buffer.from(nonceB),
    Buffer.from(te.encode(walletPeer)), Buffer.from(te.encode(bridgePeer)),
  ]))
  return hkdf(sha256, sharedSecret, salt, te.encode(PQ_INFO), 32)
}

// Per-direction AEAD with a monotonically increasing 96-bit counter nonce.
// Direction separation (b2w vs w2b) via HKDF info suffix → no nonce reuse.
function makeAead(sessionKey, dirInfo) {
  const key = hkdf(sha256, sessionKey, undefined, te.encode(PQ_INFO + '/' + dirInfo), 32)
  let counter = 0n
  return {
    seal(plain, aad) {
      const nonce = new Uint8Array(12)
      new DataView(nonce.buffer).setBigUint64(4, counter++, false)
      const ct = chacha20poly1305(key, nonce, aad).encrypt(plain)
      const out = new Uint8Array(12 + ct.length)
      out.set(nonce, 0); out.set(ct, 12)
      return out
    },
    open(sealed, aad) {
      const nonce = sealed.slice(0, 12)
      return chacha20poly1305(key, nonce, aad).decrypt(sealed.slice(12))
    },
  }
}

// walletPeerId(str) → { key, sealToWallet, openFromWallet, walletMldsaFp, at }
export const pqSessions = new Map()

/** Wire the PQ-hybrid responder onto the bridge's libp2p node. */
export async function initPq(node) {
  const kem = loadOrMintKem()
  const bridgePeer = node.peerId.toString()
  const kemFp = hex(sha256(kem.publicKey)).slice(0, 16)

  // Publish the KEM pubkey next to bridge-addr.json — the wallet's second
  // trust anchor (TLS origin) alongside the noise channel to the pinned peer.
  const pqAnnounce = JSON.stringify({
    v: 1, alg: 'ML-KEM-1024', protocol: PQ_PROTOCOL,
    kem_pk: hex(kem.publicKey), kem_fp: kemFp,
    bridge_peer: bridgePeer,
    hybrid: 'noise(X25519/ChaCha20-Poly1305) + ML-KEM-1024 → HKDF-SHA256 session key',
    ts: Date.now(),
  })
  for (const root of WEB_ROOTS) {
    try { writeFileSync(`${root}/bridge-pq.json`, pqAnnounce) }
    catch (e) { console.log('⚠ PQ announce write', root, '→', e?.message || e) }
  }
  console.log(`🔐 PQ: published bridge-pq.json (kem_fp ${kemFp}) → web roots`)

  await node.handle(PQ_PROTOCOL, async ({ stream, connection }) => {
    const walletPeer = connection.remotePeer.toString()
    const t0 = Date.now()
    const writer = makeWriter(stream)
    try {
      const read = makeReader(stream.source)
      const transcript = []

      // 1. HELLO_REQ from the wallet (its ML-DSA-87 identity + nonce)
      const helloReq = await read()
      if (helloReq.t !== 'HELLO_REQ' || !helloReq.nonce_w || !helloReq.mldsa_pk) {
        throw new Error('bad HELLO_REQ')
      }
      transcript.push(JSON.stringify(helloReq))

      // 2. HELLO with our KEM pk
      const nonceB = hex(randomBytes(32))
      const hello = { t: 'HELLO', v: 1, kem_pk: hex(kem.publicKey), nonce_b: nonceB, peer: bridgePeer }
      transcript.push(JSON.stringify(hello))
      writer.write(hello)

      // 3. ENCAP: kem ciphertext + ML-DSA-87 signature over the transcript-so-far
      const encap = await read()
      if (encap.t !== 'ENCAP' || !encap.ct || !encap.sig) throw new Error('bad ENCAP')
      const sigMsg = sha256(te.encode(transcript.join('|')))
      const ok = ml_dsa87.verify(unhex(encap.sig), sigMsg, unhex(helloReq.mldsa_pk))
      if (!ok) throw new Error('ML-DSA-87 transcript signature INVALID')

      const ss = ml_kem1024.decapsulate(unhex(encap.ct), kem.secretKey)
      const key = deriveSessionKey(ss, unhex(helloReq.nonce_w), unhex(nonceB), walletPeer, bridgePeer)

      // 4. CONFIRM: AEAD over the FULL transcript hash proves key agreement
      // (and authenticates the bridge inside the PQ layer — only the true
      // KEM secret-key holder derives this key).
      transcript.push(JSON.stringify({ t: 'ENCAP', ct_fp: hex(sha256(unhex(encap.ct))).slice(0, 16) }))
      const b2w = makeAead(key, 'b2w')
      const tag = b2w.seal(sha256(te.encode(transcript.join('|'))), te.encode('confirm'))
      writer.write({ t: 'CONFIRM', tag: hex(tag) })
      await writer.close()

      pqSessions.set(walletPeer, {
        key,
        sealToWallet: b2w,                 // continues the b2w nonce counter after CONFIRM
        openFromWallet: makeAead(key, 'w2b'),
        walletMldsaFp: hex(sha256(unhex(helloReq.mldsa_pk))).slice(0, 16),
        at: Date.now(),
      })
      console.log(`🔐 PQ-HYBRID session ESTABLISHED ↔ ${walletPeer.slice(0, 16)}… ` +
        `(ML-KEM-1024 + ML-DSA-87 wallet-id ${pqSessions.get(walletPeer).walletMldsaFp}, ${Date.now() - t0}ms)`)
    } catch (err) {
      console.log(`⚠ PQ handshake failed (${walletPeer.slice(0, 12)}…):`, err?.message || err)
      try { await writer.close() } catch {}
      try { stream.abort?.(err instanceof Error ? err : new Error(String(err))) } catch {}
    }
  })

  console.log(`🔐 PQ: ${PQ_PROTOCOL} responder live (hybrid: classical noise channel + ML-KEM-1024 session layer)`)
  return { kemFingerprint: kemFp, protocol: PQ_PROTOCOL }
}
