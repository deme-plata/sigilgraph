/* sigil-ble.js — Chrome ↔ SIGIL phone over Web Bluetooth. v1 (2026-09-11)
 *
 * The browser port of `flux-bluetooth::sigil` (Rust reference) and the Android wallet's
 * `wallet/ble/SigilBle.kt`. All three are pinned to the same bytes by
 * `tests/vectors/sigil-ble-v1.json`; `window.SigilBle.selfTest(vectors)` runs them here.
 *
 * What this can do today, honestly:
 *   • connect to a phone that has More → Nearby → "Be visible" on (Chrome is a BLE
 *     central only — it cannot advertise, so the phone is always the server);
 *   • show the same six-digit code the phone shows (X25519 → HKDF → AES-256-GCM link;
 *     the code is bound to both ephemeral keys, so a man-in-the-middle shows two codes);
 *   • RECEIVE a bearer coin from the phone into a purse in this browser, and CLAIM it
 *     (sweep the coin's one note into this wallet with a real zk-STARK, same WASM the
 *     Send button uses);
 *   • send the phone a payment request carrying this wallet's shielded keys.
 * What it cannot do yet: mint a coin in the browser to GIVE to a phone. Say so in UI.
 *
 * Money rule, said the same way as on the phone: a coin is cash — first claim wins.
 */
(function () {
  'use strict';
  const SERVICE = '53494749-4c42-4c45-a000-000000000001';
  const INFO    = '53494749-4c42-4c45-a000-000000000002';
  const RX      = '53494749-4c42-4c45-a000-000000000003';
  const TX      = '53494749-4c42-4c45-a000-000000000004';
  const VERSION = 1, NETWORK = 'sigil-g2';
  const CAP_COIN = 'coin', CAP_REQUEST = 'request', CAP_RELAY = 'relay';
  const ENV_PLAIN = 0x00, ENV_SEALED = 0x01;
  const MAX_MESSAGE_LEN = 1 << 20;
  const FEE = 100000n; // sigil_state::shielded::SHIELDED_FEE on g2 — mirrored in the wallet page
  const te = new TextEncoder(), td = new TextDecoder();

  const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
  const unhex = (s) => new Uint8Array(s.match(/../g).map((h) => parseInt(h, 16)));
  const cat = (...arrs) => { const n = arrs.reduce((a, b) => a + b.length, 0); const o = new Uint8Array(n); let i = 0; for (const a of arrs) { o.set(a, i); i += a.length; } return o; };

  // ── frames ────────────────────────────────────────────────────────────────
  const Frame = {
    MAGIC: 0x5b, HEADER_LEN: 8, ATT_OVERHEAD: 3, MIN_MTU: 23, MAX_MTU: 517,
    chunkPayload(mtu) { mtu = Math.min(Math.max(mtu | 0, 23), 517); return mtu - 3 - 8; },
    encode(msgId, payload, mtu) {
      if (!payload.length) throw new Error('empty message');
      const per = Frame.chunkPayload(mtu), total = Math.ceil(payload.length / per);
      if (total > 0xffff) throw new Error('message too large: ' + payload.length);
      const out = [];
      for (let seq = 0; seq < total; seq++) {
        const part = payload.subarray(seq * per, Math.min((seq + 1) * per, payload.length));
        const c = new Uint8Array(8 + part.length); const dv = new DataView(c.buffer);
        c[0] = 0x5b; c[1] = VERSION; dv.setUint16(2, msgId, true); dv.setUint16(4, seq, true); dv.setUint16(6, total, true);
        c.set(part, 8); out.push(c);
      }
      return out;
    },
    parseHeader(c) {
      if (c.length < 8 || c[0] !== 0x5b || c[1] !== VERSION) return null;
      const dv = new DataView(c.buffer, c.byteOffset, c.byteLength);
      const h = { msgId: dv.getUint16(2, true), seq: dv.getUint16(4, true), total: dv.getUint16(6, true) };
      if (h.total === 0 || h.seq >= h.total) return null;
      return h;
    },
  };
  class Reassembler {
    constructor(maxLen = MAX_MESSAGE_LEN) { this.partial = new Map(); this.maxLen = maxLen; }
    push(chunk) {
      const h = Frame.parseHeader(chunk); if (!h) return null;
      let p = this.partial.get(h.msgId);
      if (!p || p.total !== h.total || (p.next !== h.seq && h.seq === 0)) { p = { total: h.total, next: 0, parts: [], len: 0 }; this.partial.set(h.msgId, p); }
      if (p.next !== h.seq) { this.partial.delete(h.msgId); return null; }
      const body = chunk.subarray(8);
      if (p.len + body.length > this.maxLen) { this.partial.delete(h.msgId); return null; }
      p.parts.push(body); p.len += body.length; p.next++;
      if (p.next === p.total) { this.partial.delete(h.msgId); return cat(...p.parts); }
      return null;
    }
    progress(msgId) { const p = this.partial.get(msgId); return p ? [p.next, p.total] : null; }
    clear() { this.partial.clear(); }
  }

  // ── link (WebCrypto) ──────────────────────────────────────────────────────
  const SALT = te.encode('sigil-ble-v1'), AAD = te.encode('sigil-ble-v1');
  async function hkdf(shared, info, len) {
    const k = await crypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveBits']);
    return new Uint8Array(await crypto.subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt: SALT, info }, k, len * 8));
  }
  class Link {
    /** role: 'central'|'peripheral'; shared: 32 bytes; epkC/epkP: the two ephemeral publics. */
    static async derive(role, shared, epkC, epkP) {
      const l = new Link(); l.role = role;
      const kc2p = await hkdf(shared, cat(te.encode('c2p'), epkC, epkP), 32);
      const kp2c = await hkdf(shared, cat(te.encode('p2c'), epkC, epkP), 32);
      const sasB = await hkdf(shared, cat(te.encode('sas'), epkC, epkP), 4);
      l.sas = String(new DataView(sasB.buffer).getUint32(0, false) % 1000000).padStart(6, '0');
      const imp = (k) => crypto.subtle.importKey('raw', k, 'AES-GCM', false, ['encrypt', 'decrypt']);
      if (role === 'central') { l.sendKey = await imp(kc2p); l.recvKey = await imp(kp2c); l.sendDir = 0; l.recvDir = 1; }
      else { l.sendKey = await imp(kp2c); l.recvKey = await imp(kc2p); l.sendDir = 1; l.recvDir = 0; }
      l.sendCtr = 0n; l.recvCtr = 0n; return l;
    }
    static nonce(dir, ctr) { const n = new Uint8Array(12); n[0] = dir; new DataView(n.buffer).setBigUint64(4, ctr, false); return n; }
    async seal(pt) {
      const n = Link.nonce(this.sendDir, this.sendCtr++);
      const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv: n, additionalData: AAD, tagLength: 128 }, this.sendKey, pt));
      return cat(n, ct);
    }
    async open(sealed) {
      if (sealed.length < 28) throw new Error('sealed message too short');
      if (sealed[0] !== this.recvDir) throw new Error('wrong direction');
      const ctr = new DataView(sealed.buffer, sealed.byteOffset).getBigUint64(4, false);
      if (ctr !== this.recvCtr) throw new Error('nonce ' + ctr + ' is not the next expected (' + this.recvCtr + ')');
      let pt; try { pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: sealed.subarray(0, 12), additionalData: AAD, tagLength: 128 }, this.recvKey, sealed.subarray(12)); }
      catch (_) { throw new Error('authentication failed'); }
      this.recvCtr++; return new Uint8Array(pt);
    }
  }

  // ── coin URI (mirrors Android CoinTag) ────────────────────────────────────
  function parseCoin(s) {
    s = (s || '').trim(); let body;
    if (/^sigil:coin\?/i.test(s)) body = s.slice(11); else if (/^sigil:\/\/coin\?/i.test(s)) body = s.slice(13); else return null;
    let key = null, amount = null, network = NETWORK, version = 1;
    for (const pair of body.split('&')) { const i = pair.indexOf('='); if (i <= 0) continue; const k = pair.slice(0, i).toLowerCase(), v = pair.slice(i + 1).trim();
      if (k === 'v') { version = parseInt(v, 10); if (!Number.isFinite(version)) return null; }
      else if (k === 'k') key = v.toLowerCase(); else if (k === 'a') { if (!/^\d+$/.test(v)) return null; amount = BigInt(v); } else if (k === 'n') network = v; }
    if (version !== 1 || !key || !/^[0-9a-f]{64}$/.test(key)) return null;
    return { key, amount, network, uri: 'sigil:coin?v=1&k=' + key + (amount !== null ? '&a=' + amount : '') + '&n=' + network };
  }

  // ── session ───────────────────────────────────────────────────────────────
  class Session {
    constructor(role, me, mtu) { this.role = role; this.me = me; this.mtu = Math.min(Math.max(mtu | 0, 23), 517); this.link = null; this.peer = null; this.rx = new Reassembler(); this.nextMsgId = 1 + Math.floor(Math.random() * 0xfffe); this.dead = null; }
    async init() {
      this.kp = await crypto.subtle.generateKey({ name: 'X25519' }, false, ['deriveBits']);
      this.myEpk = new Uint8Array(await crypto.subtle.exportKey('raw', this.kp.publicKey));
      return this;
    }
    hello() { const h = { t: 'hello', v: VERSION, net: NETWORK, addr: this.me.addr }; if (this.me.pkShield) h.pk_shield = this.me.pkShield; if (this.me.pkEnc) h.pk_enc = this.me.pkEnc; h.epk = hex(this.myEpk); if (this.me.name) h.name = this.me.name; h.caps = this.me.caps || [CAP_COIN, CAP_REQUEST, CAP_RELAY]; h.mtu = this.mtu; return h; }
    infoBytes() { return te.encode(JSON.stringify(this.hello())); }
    async onInfo(info) { const h = JSON.parse(td.decode(info)); if (h.t !== 'hello') throw new Error('INFO was not a hello'); if (typeof h.mtu === 'number') this.mtu = Math.min(Math.max(h.mtu, 23), 517); return this.establish(h); }
    async start() { return this.frames(cat(new Uint8Array([ENV_PLAIN]), this.infoBytes())); }
    async establish(h) {
      if (h.v !== VERSION) throw this.die('peer protocol v' + h.v + ' ≠ v' + VERSION);
      if (h.net !== NETWORK) throw this.die('peer network ' + h.net + ' ≠ ' + NETWORK);
      if (!this.kp) throw this.die('hello already exchanged');
      const peerPk = await crypto.subtle.importKey('raw', unhex(h.epk), { name: 'X25519' }, false, []);
      const shared = new Uint8Array(await crypto.subtle.deriveBits({ name: 'X25519', public: peerPk }, this.kp.privateKey, 256));
      const [epkC, epkP] = this.role === 'central' ? [this.myEpk, unhex(h.epk)] : [unhex(h.epk), this.myEpk];
      this.link = await Link.derive(this.role, shared, epkC, epkP); this.kp = null; this.peer = h;
      return { type: 'peer_hello', hello: h, sas: this.link.sas };
    }
    die(why) { this.dead = why; return new Error(why); }
    frames(payload) { const id = this.nextMsgId; this.nextMsgId = this.nextMsgId >= 0xffff ? 1 : this.nextMsgId + 1; return Frame.encode(id, payload, this.mtu); }
    async sealed(m) { if (this.dead) throw new Error('session is dead: ' + this.dead); if (!this.link) throw new Error('link not established yet'); return this.frames(cat(new Uint8Array([ENV_SEALED]), await this.link.seal(te.encode(JSON.stringify(m))))); }
    peerCan(cap) { if (!this.peer) throw new Error('link not established yet'); if (!(this.peer.caps || []).includes(cap)) throw new Error('peer cannot take a ' + cap); }
    async sendCoin(uri, memo) { this.peerCan(CAP_COIN); const c = parseCoin(uri); if (!c || c.network !== NETWORK) throw new Error('not a ' + NETWORK + ' coin'); const id = hex(crypto.getRandomValues(new Uint8Array(4))); const m = { t: 'coin', id, uri: c.uri }; if (memo) m.memo = memo; return [id, await this.sealed(m)]; }
    async ackCoin(id, ok, err) { const m = { t: 'coin_ack', id, ok }; if (err) m.err = err; return this.sealed(m); }
    async sendRequest(uri) { this.peerCan(CAP_REQUEST); return this.sealed({ t: 'request', uri }); }
    async sendRelay(body) { this.peerCan(CAP_RELAY); const id = hex(crypto.getRandomValues(new Uint8Array(4))); return [id, await this.sealed({ t: 'relay', id, body })]; }
    async ackRelay(id, ok, txid, err) { const m = { t: 'relay_ack', id, ok }; if (txid) m.txid = txid; if (err) m.err = err; return this.sealed(m); }
    async bye() { return this.sealed({ t: 'bye' }); }
    /** → { events: [...], outgoing: [chunks] } */
    async onChunk(chunk) {
      const step = { events: [], outgoing: [] };
      if (this.dead) { step.events.push({ type: 'fatal', why: this.dead }); return step; }
      const payload = this.rx.push(chunk); if (!payload) return step;
      const fatal = (why) => { this.dead = why; step.events.push({ type: 'fatal', why }); };
      const env = payload[0], body = payload.subarray(1);
      if (env === ENV_PLAIN && !this.link) {
        let h = null; try { h = JSON.parse(td.decode(body)); } catch (_) {}
        if (!h || h.t !== 'hello') { fatal('first message was not a hello'); return step; }
        try { step.events.push(await this.establish(h)); } catch (e) { fatal(e.message); }
      } else if (env === ENV_PLAIN) fatal('plaintext after link');
      else if (env === ENV_SEALED && !this.link) fatal('sealed message before hello');
      else if (env === ENV_SEALED) {
        let m; try { m = JSON.parse(td.decode(await this.link.open(body))); } catch (e) { fatal('open failed: ' + e.message); return step; }
        await this.onSealed(m, step);
      } else fatal('unknown envelope ' + env);
      return step;
    }
    async onSealed(m, step) {
      const refuse = async (id, reason) => { try { step.outgoing.push(...await this.ackCoin(id, false, reason)); } catch (_) {} step.events.push({ type: 'coin_refused', id, reason }); };
      switch (m.t) {
        case 'hello': this.dead = 'second hello'; step.events.push({ type: 'fatal', why: this.dead }); break;
        case 'coin': { const c = parseCoin(m.uri); if (!c) await refuse(m.id, 'not a coin'); else if (c.network !== NETWORK) await refuse(m.id, 'coin is for ' + c.network + ', not ' + NETWORK); else step.events.push({ type: 'coin', id: m.id, coin: c, memo: m.memo || null }); break; }
        case 'coin_ack': step.events.push({ type: 'coin_ack', id: m.id, ok: !!m.ok, err: m.err || null }); break;
        case 'request': step.events.push({ type: 'request', uri: m.uri }); break;
        case 'relay': step.events.push({ type: 'relay', id: m.id, body: m.body }); break;
        case 'relay_ack': step.events.push({ type: 'relay_ack', id: m.id, ok: !!m.ok, txid: m.txid || null, err: m.err || null }); break;
        case 'bye': step.events.push({ type: 'bye' }); break;
        default: this.dead = 'unknown message ' + m.t; step.events.push({ type: 'fatal', why: this.dead });
      }
    }
  }

  // ── self-test against the shared vectors ──────────────────────────────────
  async function selfTest(v) {
    const eq = (a, b, what) => { if (a !== b) throw new Error(what + ': ' + a + ' ≠ ' + b); };
    eq(v.service_uuid, SERVICE, 'service'); eq(v.network, NETWORK, 'network');
    for (const f of v.frames) {
      const got = Frame.encode(f.msg_id, unhex(f.payload_hex), f.mtu).map(hex);
      eq(JSON.stringify(got), JSON.stringify(f.chunks_hex), 'frame ' + f.msg_id);
      const r = new Reassembler(); let out = null; for (const c of f.chunks_hex) out = r.push(unhex(c));
      eq(hex(out), f.payload_hex, 'reassemble ' + f.msg_id);
    }
    const l = v.link, shared = unhex(l.shared_hex), epkC = unhex(l.epk_central_hex), epkP = unhex(l.epk_peripheral_hex);
    // X25519 through WebCrypto against the Rust shared secret
    const skC = await crypto.subtle.importKey('pkcs8', cat(unhex('302e020100300506032b656e04220420'), unhex(l.sk_central_hex)), { name: 'X25519' }, false, ['deriveBits']);
    const pkP = await crypto.subtle.importKey('raw', epkP, { name: 'X25519' }, false, []);
    eq(hex(new Uint8Array(await crypto.subtle.deriveBits({ name: 'X25519', public: pkP }, skC, 256))), l.shared_hex, 'x25519');
    const c = await Link.derive('central', shared, epkC, epkP), p = await Link.derive('peripheral', shared, epkC, epkP);
    eq(c.sas, l.sas, 'sas'); eq(p.sas, l.sas, 'sas');
    for (const s of l.sealed) { const [snd, rcv] = s.dir === 'c2p' ? [c, p] : [p, c]; eq(hex(await snd.seal(te.encode(s.plaintext_utf8))), s.sealed_hex, 'seal ' + s.dir + s.ctr); eq(td.decode(await rcv.open(unhex(s.sealed_hex))), s.plaintext_utf8, 'open ' + s.dir + s.ctr); }
    return 'sigil-ble vectors OK: ' + v.frames.length + ' frames, ' + l.sealed.length + ' sealed, sas ' + l.sas;
  }

  // ── Web Bluetooth transport ───────────────────────────────────────────────
  async function connect(me, onEvent, onLog) {
    if (!navigator.bluetooth) throw new Error('This browser has no Web Bluetooth. Use Chrome or Edge on a laptop (not Firefox/Safari).');
    const log = onLog || (() => {});
    const device = await navigator.bluetooth.requestDevice({ filters: [{ services: [SERVICE] }], optionalServices: [SERVICE] });
    log('Connecting to ' + (device.name || device.id) + '…');
    const server = await device.gatt.connect();
    const svc = await server.getPrimaryService(SERVICE);
    const [info, rx, tx] = await Promise.all([svc.getCharacteristic(INFO), svc.getCharacteristic(RX), svc.getCharacteristic(TX)]);
    const sess = await new Session('central', me, 23).init();
    const writeAll = async (chunks) => { for (const c of chunks) { try { await rx.writeValueWithoutResponse(c); } catch (_) { await rx.writeValue(c); } } };
    const infoVal = await info.readValue();
    const ev = await sess.onInfo(new Uint8Array(infoVal.buffer, infoVal.byteOffset, infoVal.byteLength));
    const h = { device, sess, writeAll, disconnect: async () => { try { await writeAll(await sess.bye()); } catch (_) {} try { device.gatt.disconnect(); } catch (_) {} } };
    tx.addEventListener('characteristicvaluechanged', async (e) => {
      const v = e.target.value; const step = await sess.onChunk(new Uint8Array(v.buffer, v.byteOffset, v.byteLength));
      if (step.outgoing.length) await writeAll(step.outgoing);
      for (const ev2 of step.events) onEvent(ev2, h);
    });
    device.addEventListener('gattserverdisconnected', () => onEvent({ type: 'disconnected' }, h));
    await tx.startNotifications();
    await writeAll(await sess.start());
    onEvent(ev, h);
    return h;
  }

  // ── purse (this browser) ──────────────────────────────────────────────────
  const PURSE_KEY = 'sigil-ble-purse';
  const purse = { list() { try { return JSON.parse(localStorage.getItem(PURSE_KEY) || '[]'); } catch (_) { return []; } }, save(l) { try { localStorage.setItem(PURSE_KEY, JSON.stringify(l)); } catch (_) {} },
    add(c) { const l = purse.list(); if (l.some((x) => x.key === c.key)) return false; l.unshift(c); purse.save(l); return true; },
    set(key, patch) { const l = purse.list().map((x) => (x.key === key ? Object.assign(x, patch) : x)); purse.save(l); } };

  // ── claim: sweep the coin's one note into THIS wallet ─────────────────────
  async function claim(coin, log) {
    const W = window.sigilShieldWasm; if (!(W && W.openNoteCiphertext && W.buildPrivateSendReceived)) throw new Error('the shielded WASM is not loaded yet — wait a moment and retry');
    const from = (window.MINE_ADDR || window.ADDR || '').toLowerCase(); if (!/^[0-9a-f]{64}$/.test(from)) throw new Error('no wallet address loaded');
    let mnemonic = window.__sigilMnemonic || (typeof sigilLoadSecret === 'function' ? sigilLoadSecret() : null);
    if (!mnemonic) { mnemonic = (prompt('Enter your recovery phrase to claim this coin into your wallet.') || '').trim(); if (!mnemonic) throw new Error('cancelled'); }
    const kp = window.sigilDeriveAuto(mnemonic, from); window.__sigilMnemonic = mnemonic;
    const mySeed = hex(kp.priv);
    const myPkShield = W.shieldPublicKey(mySeed), myPkEnc = W.shieldEncryptPublicKey(mySeed);
    const anchor = await fetch('/v1/shielded/anchor', { cache: 'no-store' }).then((r) => r.json());
    const live = anchor.epoch || 0;
    const nl = await fetch('/v1/shielded/nullifiers', { cache: 'no-store' }).then((r) => r.json()).catch(() => null);
    const spent = new Set(((nl && nl.nullifiers) || []).map((x) => String(x).toLowerCase()));
    const M = window.__sigilShield;
    const scoped = (epoch, raw) => { if (!epoch || !M) return raw; const dom = te.encode('sigil-shielded-nullifier-epoch-v1'); const ep = new Uint8Array(4); new DataView(ep.buffer).setUint32(0, epoch, true); return hex(M.blake3(cat(dom, ep, unhex(raw)))).toLowerCase(); };
    // Newest generation first — a coin is far likelier recent than ancient.
    let sawSpent = false;
    for (let ep = live; ep >= 0; ep--) {
      log('Scanning pool generation ' + ep + '…');
      const lj = await fetch('/v1/shielded/leaves' + (ep === live ? '' : '?epoch=' + ep), { cache: 'no-store' }).then((r) => r.json());
      const cts = lj.ciphertexts || [], leaves = lj.leaves || [], capacity = lj.capacity;
      for (let i = 0; i < cts.length; i++) {
        if (!cts[i]) continue;
        let oj; try { oj = JSON.parse(W.openNoteCiphertext(coin.key, typeof cts[i] === 'string' ? cts[i] : JSON.stringify(cts[i]))); } catch (_) { continue; }
        if (!oj || oj.value == null) continue;
        const raw = String(W.noteNullifier(coin.key, i)).toLowerCase();
        if (spent.has(scoped(ep, raw))) { sawSpent = true; continue; }
        const value = BigInt(oj.value); if (value <= FEE) throw new Error('coin holds ' + value + ' glyphs, below the ' + FEE + ' glyph fee');
        const net = value - FEE;
        log('Found ' + value + ' glyphs at leaf #' + i + ' — proving the sweep (real zk-STARK, ~1 s)…');
        const req = JSON.parse(W.buildPrivateSendReceived(coin.key, oj.blinding, String(value), i, JSON.stringify(leaves), capacity, myPkShield, myPkEnc, net.toString()));
        const sr = await fetch('/v1/shielded_send', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ anchor: req.anchor, nullifier: req.nullifier, extra_nullifiers: req.extra_nullifiers || [], cm_outs: req.cm_outs, fee: req.fee, proof: req.proof, note_ciphertexts: req.note_ciphertexts }) });
        let sj = null; try { sj = await sr.json(); } catch (_) {}
        if (!sr.ok || !sj || !sj.ok) throw new Error((sj && (sj.error || sj.message)) || ('HTTP ' + sr.status));
        return { txid: sj.txid, net, nullifier: req.nullifier };
      }
    }
    throw new Error(sawSpent ? 'already cashed — somebody claimed this coin first' : 'no money found behind this coin (never funded, or its mint never settled)');
  }

  // ── UI: floating launcher + modal, self-contained ─────────────────────────
  function fmt(glyphs) { if (glyphs == null) return '?'; const s = BigInt(glyphs).toString().padStart(11, '0'); return (s.slice(0, -10) + '.' + s.slice(-10)).replace(/\.?0+$/, '') || '0'; }
  function ui() {
    if (document.getElementById('sigil-ble-fab')) return;
    const css = document.createElement('style'); css.textContent = `
      #sigil-ble-fab{position:fixed;right:18px;bottom:84px;z-index:9998;width:48px;height:48px;border-radius:50%;border:1px solid #2bd4ff66;background:#0b1620;color:#6df3ff;font-size:22px;cursor:pointer;box-shadow:0 0 18px #2bd4ff33}
      #sigil-ble-modal{position:fixed;inset:0;z-index:9999;display:none;align-items:center;justify-content:center;background:#000a}
      #sigil-ble-modal .card{width:min(440px,92vw);max-height:88vh;overflow:auto;background:#0b1620;border:1px solid #2bd4ff55;border-radius:14px;padding:18px;color:#cfe9f3;font:14px/1.45 system-ui,sans-serif}
      #sigil-ble-modal h3{margin:0 0 6px;color:#6df3ff;font-size:16px}
      #sigil-ble-modal .sas{font:700 34px/1 ui-monospace,monospace;letter-spacing:6px;text-align:center;margin:12px 0 6px;color:#fff}
      #sigil-ble-modal .muted{color:#8fb3c2;font-size:12px}
      #sigil-ble-modal button{background:#0f2a3a;color:#6df3ff;border:1px solid #2bd4ff66;border-radius:8px;padding:8px 12px;margin:4px 4px 4px 0;cursor:pointer;font-size:13px}
      #sigil-ble-modal button.primary{background:#2bd4ff;color:#03131b;font-weight:700}
      #sigil-ble-modal .row{display:flex;justify-content:space-between;align-items:center;gap:8px;padding:8px 0;border-top:1px solid #1e3a4a}
      #sigil-ble-modal .log{font:11px/1.4 ui-monospace,monospace;color:#8fb3c2;max-height:110px;overflow:auto;margin-top:8px;white-space:pre-wrap}
      #sigil-ble-modal .err{color:#ff8a8a}#sigil-ble-modal .ok{color:#00e0c6}`;
    document.head.appendChild(css);
    const fab = document.createElement('button'); fab.id = 'sigil-ble-fab'; fab.title = 'Nearby phone (Bluetooth)'; fab.textContent = '📶';
    const modal = document.createElement('div'); modal.id = 'sigil-ble-modal';
    modal.innerHTML = '<div class="card"><h3>📶 Nearby phone — coins over Bluetooth</h3><div id="sble-body"></div><div class="log" id="sble-log"></div><div style="margin-top:10px;text-align:right"><button id="sble-close">Close</button></div></div>';
    document.body.appendChild(fab); document.body.appendChild(modal);
    const body = modal.querySelector('#sble-body'), logEl = modal.querySelector('#sble-log');
    const log = (m, cls) => { const d = document.createElement('div'); if (cls) d.className = cls; d.textContent = m; logEl.appendChild(d); logEl.scrollTop = 1e9; };
    let link = null, state = { peer: null, sas: null, request: null };
    const render = () => {
      const p = purse.list().filter((c) => c.status === 'held');
      let h = '';
      if (!navigator.bluetooth) h += '<p class="err">This browser has no Web Bluetooth. Chrome or Edge on a laptop can do this; Firefox and Safari cannot.</p>';
      if (!state.peer) {
        h += '<p class="muted">On the phone: More → Nearby → <b>Be visible</b>. Then connect from here. Chrome can only connect, never be found — so the phone is always the visible one.</p>';
        h += '<button class="primary" id="sble-connect"' + (navigator.bluetooth ? '' : ' disabled') + '>Connect to phone</button>';
      } else {
        h += '<p>Linked with <b>' + state.peer.replace(/</g, '&lt;') + '</b></p><div class="sas">' + state.sas.replace(/(\d{3})(\d{3})/, '$1 $2') + '</div>';
        h += '<p class="muted" style="text-align:center">Same six digits on the phone? Then nobody is in between. Different? Stop.</p>';
        h += '<p class="muted">The phone can now <b>give you a coin</b> (it lands in the purse below). You can send the phone a payment request carrying this wallet\'s keys; giving a coin FROM the browser is not built yet.</p>';
        h += '<button id="sble-request">Send a payment request</button> <button id="sble-disconnect">Disconnect</button>';
        if (state.request) h += '<p class="ok">The phone sent a payment request: <code>' + state.request.replace(/</g, '&lt;').slice(0, 90) + '…</code></p>';
      }
      h += '<h3 style="margin-top:14px">Purse — coins received, not yet claimed</h3>';
      if (!p.length) h += '<p class="muted">Empty. A coin handed to you over Bluetooth waits here until you claim it (needs the node). Until then, whoever holds a copy can claim it first.</p>';
      for (const c of p) h += '<div class="row"><div>' + fmt(c.amount) + ' SIGIL coin<br><span class="muted">from ' + (c.from || 'someone').replace(/</g, '&lt;') + '</span></div><button data-claim="' + c.key + '">Claim</button></div>';
      body.innerHTML = h;
      body.querySelector('#sble-connect')?.addEventListener('click', doConnect);
      body.querySelector('#sble-disconnect')?.addEventListener('click', async () => { if (link) await link.disconnect(); link = null; state = { peer: null, sas: null, request: null }; render(); });
      body.querySelector('#sble-request')?.addEventListener('click', async () => {
        const amt = prompt('Amount in SIGIL to request (blank = any):') ; if (amt === null) return;
        const glyphs = amt.trim() ? BigInt(Math.round(parseFloat(amt) * 1e10)) : null;
        const W = window.sigilShieldWasm; const from = (window.MINE_ADDR || window.ADDR || '').toLowerCase();
        let uri = 'sigil:' + from + '?label=' + encodeURIComponent('Chrome') + (glyphs !== null ? '&amount=' + glyphs : '');
        try { const mn = window.__sigilMnemonic || sigilLoadSecret(); if (mn && W) { const kp = window.sigilDeriveAuto(mn, from); const sd = hex(kp.priv); uri += '&s=' + W.shieldPublicKey(sd) + '&e=' + W.shieldEncryptPublicKey(sd); } } catch (_) {}
        try { await link.writeAll(await link.sess.sendRequest(uri)); log('Sent payment request.', 'ok'); } catch (e) { log('✗ ' + e.message, 'err'); }
      });
      body.querySelectorAll('[data-claim]').forEach((b) => b.addEventListener('click', async () => {
        const key = b.getAttribute('data-claim'); const c = purse.list().find((x) => x.key === key); if (!c) return; b.disabled = true; b.textContent = 'Claiming…';
        try { const r = await claim(c, (m) => log(m)); purse.set(key, { status: 'claimed', txid: r.txid }); log('✓ claimed ' + fmt(r.net) + ' SIGIL into this wallet — txid ' + String(r.txid).slice(0, 12) + '…', 'ok'); }
        catch (e) { log('✗ ' + e.message, 'err'); if (/already cashed/.test(e.message)) purse.set(key, { status: 'spent_elsewhere' }); }
        render();
      }));
    };
    async function doConnect() {
      const from = (window.MINE_ADDR || window.ADDR || '').toLowerCase();
      if (!/^[0-9a-f]{64}$/.test(from)) { log('No wallet address loaded.', 'err'); return; }
      let pkShield = null, pkEnc = null;
      try { const W = window.sigilShieldWasm, mn = window.__sigilMnemonic || sigilLoadSecret(); if (mn && W) { const kp = window.sigilDeriveAuto(mn, from); const sd = hex(kp.priv); pkShield = W.shieldPublicKey(sd); pkEnc = W.shieldEncryptPublicKey(sd); } } catch (_) {}
      const me = { addr: from, pkShield, pkEnc, name: 'Chrome', caps: [CAP_COIN, CAP_REQUEST] };
      try {
        link = await connect(me, async (ev, h) => {
          if (ev.type === 'peer_hello') { state.peer = ev.hello.name || 'phone'; state.sas = ev.sas; log('Linked. Compare the six digits with the phone.', 'ok'); render(); }
          else if (ev.type === 'coin') {
            // Store FIRST, ack SECOND — the ack is the phone's permission to forget the coin.
            const stored = purse.add({ key: ev.coin.key, uri: ev.coin.uri, amount: ev.coin.amount === null ? null : ev.coin.amount.toString(), from: state.peer, ts: Date.now(), status: 'held', memo: ev.memo });
            await h.writeAll(await h.sess.ackCoin(ev.id, stored, stored ? undefined : 'already have this coin'));
            log((stored ? 'Received ' : 'Duplicate of ') + fmt(ev.coin.amount) + ' SIGIL coin from ' + state.peer + '.', stored ? 'ok' : 'err'); render();
          }
          else if (ev.type === 'coin_refused') log('Refused a coin: ' + ev.reason, 'err');
          else if (ev.type === 'request') { state.request = ev.uri; log('Payment request received.'); render(); }
          else if (ev.type === 'bye') { log('Phone closed the link.'); }
          else if (ev.type === 'disconnected') { log('Disconnected.'); link = null; state = { peer: null, sas: null, request: null }; render(); }
          else if (ev.type === 'fatal') { log('Link broke: ' + ev.why, 'err'); }
        }, (m) => log(m));
      } catch (e) { log('✗ ' + (e && e.message || e), 'err'); }
    }
    fab.addEventListener('click', () => { modal.style.display = 'flex'; render(); });
    modal.querySelector('#sble-close').addEventListener('click', () => { modal.style.display = 'none'; });
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', ui); else ui();

  window.SigilBle = { SERVICE, INFO, RX, TX, VERSION, NETWORK, Frame, Reassembler, Link, Session, parseCoin, connect, purse, claim, selfTest, hex, unhex };
})();
