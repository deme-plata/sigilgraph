// LANE-AD: SIGIL wallet POST-QUANTUM hybrid session (browser side).
//
// Pairs with sigil-bridge/pq.mjs. The js-libp2p channel keeps CLASSICAL noise
// (X25519/ChaCha20-Poly1305 — js-libp2p has no ML-KEM noise handshake yet);
// this module layers ML-KEM-1024 (Kyber) on top of the established channel:
//
//   session_key = HKDF-SHA256(
//     ikm  = ML-KEM-1024 shared secret (encapsulated against the bridge's pk),
//     salt = SHA-256(nonce_w ‖ nonce_b ‖ walletPeerId ‖ bridgePeerId),   ← channel binding
//     info = "sigil/g0/pq-hybrid/v1")
//
// An attacker must break BOTH X25519 (the noise channel) AND ML-KEM-1024 to
// read anything sealed under the session key. The wallet authenticates with
// its ML-DSA-87 (Dilithium5) identity — generated once, seed persisted in
// localStorage — the wallet's PQ identity per @noble/post-quantum.
// Bridge authenticity anchors: (1) the noise channel to the pinned peer-id
// from /bridge-addr.json, (2) the TLS-served /bridge-pq.json KEM fingerprint
// must equal the in-band HELLO pk, (3) the CONFIRM AEAD tag only the true
// KEM secret-key holder can produce.
//
// Exposes window.SigilPQ = { status, seal, open } + 'sigil-pq-status' events,
// and shows a small 🔐PQ badge next to the P2P pill when the session is live.

import { ml_kem1024 } from 'https://esm.sh/@noble/post-quantum@0.6.1/ml-kem.js'
import { ml_dsa87 } from 'https://esm.sh/@noble/post-quantum@0.6.1/ml-dsa.js'
import { hkdf } from 'https://esm.sh/@noble/hashes@1.8.0/hkdf.js'
import { sha256 } from 'https://esm.sh/@noble/hashes@1.8.0/sha2.js'
import { chacha20poly1305 } from 'https://esm.sh/@noble/ciphers@0.6.0/chacha'

const PQ_PROTOCOL = '/sigil/g0/pq-hybrid/1.0.0'
const PQ_INFO = 'sigil/g0/pq-hybrid/v1'
const te = new TextEncoder()
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
const unhex = (s) => new Uint8Array((s.match(/../g) || []).map((h) => parseInt(h, 16)))
const concatBytes = (...arrs) => {
  const out = new Uint8Array(arrs.reduce((n, a) => n + a.length, 0))
  let o = 0
  for (const a of arrs) { out.set(a, o); o += a.length }
  return out
}

// ── wallet ML-DSA-87 identity (seed persisted; rederived each load) ─────────
function walletMldsa() {
  let seedHex = null
  try { seedHex = localStorage.getItem('sigil.pq.mldsa.seed') } catch {}
  if (!seedHex || seedHex.length !== 64) {
    const seed = crypto.getRandomValues(new Uint8Array(32))
    seedHex = hex(seed)
    try { localStorage.setItem('sigil.pq.mldsa.seed', seedHex) } catch {}
  }
  return ml_dsa87.keygen(unhex(seedHex))
}

// ── frames (identical wire format to the bridge) ────────────────────────────
function frame(obj) {
  const body = te.encode(JSON.stringify(obj))
  const out = new Uint8Array(4 + body.length)
  new DataView(out.buffer).setUint32(0, body.length, false)
  out.set(body, 4)
  return out
}
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
      if (done) throw new Error('pq stream closed')
      const chunk = value.subarray ? value.subarray() : new Uint8Array(value)
      buf = concatBytes(buf, chunk)
    }
  }
}
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

function makeAead(sessionKey, dirInfo) {
  const key = hkdf(sha256, sessionKey, undefined, te.encode(PQ_INFO + '/' + dirInfo), 32)
  let counter = 0n
  return {
    seal(plain, aad) {
      const nonce = new Uint8Array(12)
      new DataView(nonce.buffer).setBigUint64(4, counter++, false)
      const ct = chacha20poly1305(key, nonce, aad).encrypt(plain)
      return concatBytes(nonce, ct)
    },
    open(sealed, aad) {
      return chacha20poly1305(key, sealed.slice(0, 12), aad).decrypt(sealed.slice(12))
    },
  }
}

// ── state ───────────────────────────────────────────────────────────────────
const S = {
  established: false,
  kemFp: null,
  mldsaFp: null,
  bridgePeer: null,
  ms: null,
  error: null,
  sealToBridge: null,   // w2b AEAD
  openFromBridge: null, // b2w AEAD (CONFIRM consumed nonce 0)
}
function emit() {
  try {
    window.dispatchEvent(new CustomEvent('sigil-pq-status', { detail: { ...S, sealToBridge: undefined, openFromBridge: undefined } }))
  } catch {}
  badge()
}

// small self-contained badge next to the P2P pill
function badge() {
  try {
    let el = document.getElementById('pq-badge')
    if (!el) {
      el = document.createElement('div')
      el.id = 'pq-badge'
      el.style.cssText = 'position:fixed;left:10px;bottom:44px;z-index:9999;font-family:JetBrains Mono,monospace;font-size:10px;padding:4px 8px;border-radius:8px;backdrop-filter:blur(6px);user-select:none'
      document.body.appendChild(el)
    }
    if (S.established) {
      el.style.color = '#7CFFB2'
      el.style.background = 'rgba(4,12,8,.85)'
      el.style.border = '1px solid rgba(124,255,178,.4)'
      el.title = `post-quantum hybrid session: classical noise + ML-KEM-1024 → HKDF\nkem ${S.kemFp} · wallet ML-DSA-87 ${S.mldsaFp} · ${S.ms}ms`
      el.textContent = '🔐 PQ ML-KEM-1024 · hybrid live'
    } else {
      el.style.color = '#5a93a8'
      el.style.background = 'rgba(4,8,12,.8)'
      el.style.border = '1px solid rgba(90,147,168,.3)'
      el.title = S.error ? `PQ hybrid not established: ${S.error}` : 'PQ hybrid: waiting for bridge connection…'
      el.textContent = S.error ? '🔓 PQ off — classical noise only' : '🔐 PQ negotiating…'
    }
  } catch {}
}

// ── the handshake ───────────────────────────────────────────────────────────
async function handshake(node, bridgePeer, publishedKemFp) {
  const conn = node.getConnections().find((c) => c.remotePeer.toString() === bridgePeer)
  if (!conn) throw new Error('no connection to bridge yet')
  const stream = await conn.newStream(PQ_PROTOCOL)
  const writer = makeWriter(stream)
  try {
    const read = makeReader(stream.source)
    const mldsa = walletMldsa()
    const t0 = Date.now()
    const transcript = []

    const nonceW = hex(crypto.getRandomValues(new Uint8Array(32)))
    const helloReq = { t: 'HELLO_REQ', v: 1, nonce_w: nonceW, mldsa_pk: hex(mldsa.publicKey) }
    transcript.push(JSON.stringify(helloReq))
    writer.write(helloReq)

    const hello = await read()
    if (hello.t !== 'HELLO') throw new Error('expected HELLO')
    transcript.push(JSON.stringify(hello))
    // TLS-anchor cross-check: in-band KEM pk must match /bridge-pq.json
    const inbandFp = hex(sha256(unhex(hello.kem_pk))).slice(0, 16)
    if (publishedKemFp && inbandFp !== publishedKemFp) {
      throw new Error(`KEM pk mismatch: in-band ${inbandFp} ≠ published ${publishedKemFp}`)
    }

    const { cipherText, sharedSecret } = ml_kem1024.encapsulate(unhex(hello.kem_pk))
    const sig = ml_dsa87.sign(sha256(te.encode(transcript.join('|'))), mldsa.secretKey)
    writer.write({ t: 'ENCAP', ct: hex(cipherText), sig: hex(sig) })

    const walletPeer = node.peerId.toString()
    const salt = sha256(concatBytes(unhex(nonceW), unhex(hello.nonce_b), te.encode(walletPeer), te.encode(bridgePeer)))
    const key = hkdf(sha256, sharedSecret, salt, te.encode(PQ_INFO), 32)

    const confirm = await read()
    if (confirm.t !== 'CONFIRM') throw new Error('expected CONFIRM')
    transcript.push(JSON.stringify({ t: 'ENCAP', ct_fp: hex(sha256(cipherText)).slice(0, 16) }))
    const b2w = makeAead(key, 'b2w')
    const opened = b2w.open(unhex(confirm.tag), te.encode('confirm'))
    const expect = sha256(te.encode(transcript.join('|')))
    if (hex(opened) !== hex(expect)) throw new Error('CONFIRM transcript hash mismatch')

    await writer.close()
    S.established = true
    S.kemFp = inbandFp
    S.mldsaFp = hex(sha256(mldsa.publicKey)).slice(0, 16)
    S.bridgePeer = bridgePeer
    S.ms = Date.now() - t0
    S.error = null
    S.openFromBridge = b2w
    S.sealToBridge = makeAead(key, 'w2b')
    console.log(`🔐 [SIGIL PQ] HYBRID SESSION LIVE in ${S.ms}ms — classical noise + ML-KEM-1024, wallet ML-DSA-87 ${S.mldsaFp}`)
    emit()
  } catch (e) {
    try { await writer.close() } catch {}
    throw e
  }
}

// ── boot: wait for the tron P2P node + bridge connection, then negotiate ────
async function boot() {
  emit()
  let bridgePeer = null
  let publishedKemFp = null
  try {
    const [addr, pq] = await Promise.all([
      fetch('/bridge-addr.json?ts=' + Date.now()).then((r) => r.json()),
      fetch('/bridge-pq.json?ts=' + Date.now()).then((r) => r.json()).catch(() => null),
    ])
    bridgePeer = addr.peer
    publishedKemFp = pq?.kem_fp || null
    if (!pq) console.warn('[SIGIL PQ] /bridge-pq.json missing — bridge predates LANE-AD?')
  } catch (e) {
    S.error = 'bridge-addr.json unavailable: ' + (e?.message || e)
    emit()
    return
  }

  let attempts = 0
  const tryOnce = async () => {
    if (S.established) return true
    const api = window.SigilTronP2P
    const node = api && api.node && api.node()
    if (!node) return false
    try {
      await handshake(node, bridgePeer, publishedKemFp)
      return true
    } catch (e) {
      const msg = e?.message || String(e)
      if (!/no connection to bridge/.test(msg)) {
        S.error = msg
        console.warn('[SIGIL PQ] handshake attempt failed:', msg)
        emit()
      }
      return false
    }
  }

  const timer = setInterval(async () => {
    attempts++
    if (await tryOnce() || attempts > 60) clearInterval(timer)
  }, 3000)
  await tryOnce()
}

window.SigilPQ = {
  status: () => ({ ...S, sealToBridge: undefined, openFromBridge: undefined }),
  // PQ-sealed lane for confidential wallet→bridge / bridge→wallet payloads
  // (AC/AE building blocks). Throws if the session isn't established.
  seal: (bytes, aad) => {
    if (!S.sealToBridge) throw new Error('PQ session not established')
    return S.sealToBridge.seal(bytes, aad ? te.encode(aad) : undefined)
  },
  open: (bytes, aad) => {
    if (!S.openFromBridge) throw new Error('PQ session not established')
    return S.openFromBridge.open(bytes, aad ? te.encode(aad) : undefined)
  },
  protocol: PQ_PROTOCOL,
}

boot().catch((e) => { S.error = e?.message || String(e); emit() })
