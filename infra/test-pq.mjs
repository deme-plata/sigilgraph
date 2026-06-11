// LANE-AD e2e test client: act like the browser wallet — dial the bridge over
// ws://127.0.0.1:9610, run the /sigil/g0/pq-hybrid/1.0.0 handshake, verify the
// CONFIRM tag. Proves the bridge side without a browser.
// Run: /root/.quillon/node/bin/node test-pq.mjs

import { createLibp2p } from 'libp2p'
import { webSockets } from '@libp2p/websockets'
import { noise } from '@chainsafe/libp2p-noise'
import { yamux } from '@chainsafe/libp2p-yamux'
import { multiaddr } from '@multiformats/multiaddr'
import { readFileSync } from 'node:fs'
import { randomBytes } from 'node:crypto'
import { ml_kem1024 } from '@noble/post-quantum/ml-kem.js'
import { ml_dsa87 } from '@noble/post-quantum/ml-dsa.js'
import { hkdf } from '@noble/hashes/hkdf.js'
import { sha256 } from '@noble/hashes/sha2.js'
import { chacha20poly1305 } from '@noble/ciphers/chacha'

const PQ_PROTOCOL = '/sigil/g0/pq-hybrid/1.0.0'
const PQ_INFO = 'sigil/g0/pq-hybrid/v1'
const te = new TextEncoder()
const hex = (b) => Buffer.from(b).toString('hex')
const unhex = (s) => new Uint8Array(Buffer.from(s, 'hex'))

// the published announce (what the browser fetches over TLS)
const announce = JSON.parse(readFileSync('/home/orobit/q-narwhalknight/dist-final/bridge-addr.json', 'utf8'))
const pqAnnounce = JSON.parse(readFileSync('/home/orobit/q-narwhalknight/dist-final/bridge-pq.json', 'utf8'))
console.log('bridge peer:', announce.peer, '| published kem_fp:', pqAnnounce.kem_fp)

const node = await createLibp2p({
  transports: [webSockets()],
  connectionEncryption: [noise()],
  streamMuxers: [yamux()],
})
await node.start()

const target = multiaddr(`/ip4/127.0.0.1/tcp/9610/ws/p2p/${announce.peer}`)
console.log('dialing', target.toString())
const stream = await node.dialProtocol(target, PQ_PROTOCOL)
console.log('✓ noise channel up, PQ protocol stream open')

// frame helpers (same wire format as pq.mjs)
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
        if (buf.length >= 4 + len) {
          const body = buf.slice(4, 4 + len)
          buf = buf.slice(4 + len)
          return JSON.parse(new TextDecoder().decode(body))
        }
      }
      const { value, done } = await iter.next()
      if (done) throw new Error('stream closed')
      const chunk = value.subarray ? value.subarray() : new Uint8Array(value)
      const next = new Uint8Array(buf.length + chunk.length)
      next.set(buf, 0); next.set(chunk, buf.length)
      buf = next
    }
  }
}
const q = []; let notify = null; let closed = false
const gen = (async function* () {
  for (;;) { while (q.length) yield q.shift(); if (closed) return; await new Promise(r => { notify = r }) }
})()
const sinkDone = stream.sink(gen)
const write = (o) => { q.push(frame(o)); if (notify) { const n = notify; notify = null; n() } }
const read = makeReader(stream.source)

// wallet ML-DSA-87 identity (throwaway for the test)
const mldsaSeed = new Uint8Array(randomBytes(32))
const mldsa = ml_dsa87.keygen(mldsaSeed)

const t0 = Date.now()
const transcript = []
// 1. HELLO_REQ
const nonceW = hex(randomBytes(32))
const helloReq = { t: 'HELLO_REQ', v: 1, nonce_w: nonceW, mldsa_pk: hex(mldsa.publicKey) }
transcript.push(JSON.stringify(helloReq))
write(helloReq)

// 2. HELLO ← bridge
const hello = await read()
if (hello.t !== 'HELLO') throw new Error('expected HELLO, got ' + hello.t)
transcript.push(JSON.stringify(hello))
// TRUST ANCHOR CHECK: in-band KEM pk must equal the TLS-published one
const inbandFp = hex(sha256(unhex(hello.kem_pk))).slice(0, 16)
if (inbandFp !== pqAnnounce.kem_fp) throw new Error(`KEM pk MISMATCH: in-band ${inbandFp} vs published ${pqAnnounce.kem_fp}`)
console.log('✓ HELLO: kem_fp matches the TLS-published anchor:', inbandFp)

// 3. ENCAP →
const { cipherText, sharedSecret } = ml_kem1024.encapsulate(unhex(hello.kem_pk))
const sigMsg = sha256(te.encode(transcript.join('|')))
const sig = ml_dsa87.sign(sigMsg, mldsa.secretKey)
write({ t: 'ENCAP', ct: hex(cipherText), sig: hex(sig) })

// derive the session key exactly like the bridge
const walletPeer = node.peerId.toString()
const salt = sha256(Buffer.concat([
  Buffer.from(unhex(nonceW)), Buffer.from(unhex(hello.nonce_b)),
  Buffer.from(te.encode(walletPeer)), Buffer.from(te.encode(announce.peer)),
]))
const key = hkdf(sha256, sharedSecret, salt, te.encode(PQ_INFO), 32)

// 4. CONFIRM ← bridge: open the AEAD tag
const confirm = await read()
if (confirm.t !== 'CONFIRM') throw new Error('expected CONFIRM, got ' + confirm.t)
transcript.push(JSON.stringify({ t: 'ENCAP', ct_fp: hex(sha256(cipherText)).slice(0, 16) }))
const b2wKey = hkdf(sha256, key, undefined, te.encode(PQ_INFO + '/b2w'), 32)
const sealed = unhex(confirm.tag)
const opened = chacha20poly1305(b2wKey, sealed.slice(0, 12), te.encode('confirm')).decrypt(sealed.slice(12))
const expect = sha256(te.encode(transcript.join('|')))
if (Buffer.from(opened).equals(Buffer.from(expect))) {
  console.log(`\n✅ PQ-HYBRID HANDSHAKE COMPLETE in ${Date.now() - t0}ms`)
  console.log('   channel : classical noise (X25519/ChaCha20-Poly1305) — js-libp2p standard')
  console.log('   + layer : ML-KEM-1024 encapsulation → HKDF-SHA256 session key (channel-bound)')
  console.log('   wallet  : ML-DSA-87 transcript signature VERIFIED by bridge')
  console.log('   bridge  : CONFIRM tag opened under derived key = same secret on both ends')
} else {
  throw new Error('CONFIRM tag opened but transcript hash mismatch')
}
closed = true; if (notify) notify()
await sinkDone.catch(() => {})
await node.stop()
process.exit(0)
