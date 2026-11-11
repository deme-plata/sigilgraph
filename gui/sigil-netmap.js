// sigil-netmap.js — live 3D map of the SIGIL nodes the answering node is connected to.
// 2026-11-09, Rocky (Viktor: "clearly stated in the network stats through a three.js animation which nodes are
// connected to form the perfect decentralized and data integrity visual").
//
// Two views, toggled top-right:
//   NOW   measured. Positions are layout; every colour, label and count is data from /v1/network/topology:
//         green line   = that node's tip AND state root EQUAL this node's own block at the same height (peer_views);
//         amber        = a mismatch seen fewer than 3 refreshes in a row (one sample is inconclusive);
//         red          = a mismatch that persisted for 3 refreshes;
//         blue + ring  = still syncing; the ring fills to its share of the chain;
//         grey dashed  = connected, but it has not reported a comparable view;
//         big shell    = ONE machine (every node connected over 127.0.0.1 lives inside it);
//         outer shells = nodes on other machines;
//         purple ◇     = what is still missing for BFT finality: independent signers (BFT needs 4).
//   GOAL  the mainnet target, drawn from the same rule and captioned as NOT LIVE: 4 independent block-makers
//         on 4 machines that check each other, and every follower checks them.
// three.js r180 is served from this same site (/three/), never from an outside CDN: this runs on a wallet page.
import * as THREE from '/three/three.module.min.js';

const HEX = { agree: 0x00e0c6, checking: 0xfbbf24, differ: 0xff5c7a, syncing: 0x7c9cff, unknown: 0x5a93a8, maker: 0xfbbf24, self: 0x21d4fd, shell: 0x21d4fd, ghost: 0xc084fc };
const CSS = { agree: '#00e0c6', checking: '#fbbf24', differ: '#ff5c7a', syncing: '#9db4ff', unknown: '#8fb3c4', maker: '#fbbf24', self: '#6df3ff', ghost: '#c084fc', shell: '#5fd4ff' };
const MARK = { agree: ' ✓', checking: ' …', differ: ' ✕', unknown: ' ?' };

function hash01(s) { let h = 2166136261 >>> 0; for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619) >>> 0; } return h / 4294967296; }

function glowTexture() {
  const c = document.createElement('canvas'); c.width = c.height = 64;
  const g = c.getContext('2d'); const r = g.createRadialGradient(32, 32, 0, 32, 32, 32);
  r.addColorStop(0, 'rgba(255,255,255,1)'); r.addColorStop(0.22, 'rgba(255,255,255,.5)'); r.addColorStop(1, 'rgba(255,255,255,0)');
  g.fillStyle = r; g.fillRect(0, 0, 64, 64);
  const t = new THREE.CanvasTexture(c); t.colorSpace = THREE.SRGBColorSpace; return t;
}

export function mountNetMap(host) {
  host.style.position = 'relative';
  const W = () => Math.max(240, host.clientWidth || 600), H = () => Math.max(220, host.clientHeight || 330);
  const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true, powerPreference: 'low-power' }); // throws without WebGL → caller hides the map
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.setSize(W(), H()); renderer.setClearColor(0x000000, 0);
  const el = renderer.domElement;
  el.style.cssText = 'display:block;width:100%;height:100%;touch-action:pan-y;cursor:grab';
  host.appendChild(el);
  const overlay = document.createElement('div');
  overlay.style.cssText = 'position:absolute;inset:0;pointer-events:none;overflow:hidden';
  host.appendChild(overlay);
  const mono = '"JetBrains Mono",monospace';
  const caption = document.createElement('div');
  caption.style.cssText = 'position:absolute;left:10px;top:8px;right:120px;font:10px ' + mono + ';color:#8fb3c4;pointer-events:none;line-height:1.5;text-shadow:0 0 4px #000';
  host.appendChild(caption);
  const toggle = document.createElement('div');
  toggle.style.cssText = 'position:absolute;right:8px;top:7px;display:flex;border:1px solid rgba(33,212,253,.35);border-radius:999px;overflow:hidden;font:600 10px ' + mono + ';letter-spacing:.8px;background:rgba(4,8,12,.75)';
  const btn = (txt) => { const b = document.createElement('button'); b.type = 'button'; b.textContent = txt; b.style.cssText = 'border:0;background:transparent;color:#5a93a8;padding:5px 10px;cursor:pointer;font:inherit;letter-spacing:inherit'; toggle.appendChild(b); return b; };
  const bNow = btn('NOW'), bGoal = btn('GOAL');
  host.appendChild(toggle);
  const legend = document.createElement('div');
  legend.style.cssText = 'position:absolute;left:10px;bottom:8px;right:90px;font:10px ' + mono + ';color:#8fb3c4;pointer-events:none;line-height:1.55;text-shadow:0 0 4px #000';
  host.appendChild(legend);
  const hint = document.createElement('div');
  hint.style.cssText = 'position:absolute;right:10px;bottom:8px;font:10px ' + mono + ';color:#3d6b7d;pointer-events:none';
  hint.textContent = 'drag to turn';
  host.appendChild(hint);

  const scene = new THREE.Scene();
  scene.fog = new THREE.FogExp2(0x04080c, 0.016);
  const camera = new THREE.PerspectiveCamera(42, W() / H(), 0.1, 400);
  const narrow = () => W() < 520;
  const placeCamera = () => { const n = narrow(); camera.position.set(0, n ? 12 : 8.5, n ? 47 : 31); camera.lookAt(0, 0, 0); };
  placeCamera();
  const root = new THREE.Group(); root.rotation.x = 0.16; scene.add(root);
  const dyn = new THREE.Group(); root.add(dyn);
  const glow = glowTexture();

  { // a quiet star field so the map has depth
    const n = 420, pos = new Float32Array(n * 3);
    for (let i = 0; i < n; i++) { const r = 40 + Math.random() * 60, a = Math.random() * Math.PI * 2, y = (Math.random() - 0.5) * 70; pos.set([Math.cos(a) * r, y, Math.sin(a) * r], i * 3); }
    const g = new THREE.BufferGeometry(); g.setAttribute('position', new THREE.Float32BufferAttribute(pos, 3));
    scene.add(new THREE.Points(g, new THREE.PointsMaterial({ color: 0x3f7f99, size: 0.18, transparent: true, opacity: 0.55, depthWrite: false })));
  }

  const reduce = !!(window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  let model = null, sig = '', mode = 'now', labels = [], packets = [], pulses = [], billboards = [];
  let raf = 0, running = true, last = performance.now(), drag = null, idleUntil = 0, dead = false;
  const v = new THREE.Vector3(), q = new THREE.Quaternion();
  const ORIGIN = new THREE.Vector3(0, 0, 0);

  function disposeGroup(g) {
    g.traverse(o => { if (o.geometry) o.geometry.dispose(); if (o.material) (Array.isArray(o.material) ? o.material : [o.material]).forEach(m => m.dispose()); });
    g.clear();
  }
  function label(text, pos, css, strong) {
    const d = document.createElement('div');
    d.textContent = text; // network-supplied names: text only, never HTML
    d.style.cssText = 'position:absolute;left:0;top:0;display:none;white-space:nowrap;font:' + (strong ? '600 11px' : '10px')
      + ' ' + mono + ';color:' + css + ';text-shadow:0 0 6px #000,0 0 2px #000';
    overlay.appendChild(d);
    labels.push({ el: d, pos: pos.clone(), dy: strong ? -24 : -16, strong: !!strong, w: text.length * (strong ? 7.2 : 6.2) + 6 });
  }
  function halo(pos, size, color, opacity) {
    const s = new THREE.Sprite(new THREE.SpriteMaterial({ map: glow, color, transparent: true, opacity, blending: THREE.AdditiveBlending, depthWrite: false }));
    s.position.copy(pos); s.scale.setScalar(size); dyn.add(s); return s;
  }
  function ball(pos, r, color, pulse) {
    const m = new THREE.Mesh(new THREE.IcosahedronGeometry(r, 2), new THREE.MeshBasicMaterial({ color }));
    m.position.copy(pos); dyn.add(m);
    const h = halo(pos, r * 5, color, 0.75);
    if (pulse && !reduce) pulses.push({ obj: h, base: r * 5, phase: hash01(pos.x + ',' + pos.z) * 6.28 });
    return m;
  }
  function ring(pos, r, frac, color) { // sync progress: an arc that always faces the viewer
    const g = new THREE.Group(); g.position.copy(pos);
    const back = new THREE.Mesh(new THREE.TorusGeometry(r, 0.035, 6, 64), new THREE.MeshBasicMaterial({ color: 0x1d3442, transparent: true, opacity: 0.8, depthWrite: false }));
    const arc = new THREE.Mesh(new THREE.TorusGeometry(r, 0.075, 6, 64, Math.max(0.05, Math.min(1, frac)) * Math.PI * 2), new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.95, depthWrite: false }));
    arc.rotation.z = Math.PI / 2; // start at twelve o'clock
    g.add(back, arc); dyn.add(g); billboards.push(g);
  }
  function shell(center, radius, opacity, color) {
    const geo = new THREE.IcosahedronGeometry(radius, radius > 3 ? 2 : 1);
    const ls = new THREE.LineSegments(new THREE.EdgesGeometry(geo), new THREE.LineBasicMaterial({ color: color || HEX.shell, transparent: true, opacity, blending: THREE.AdditiveBlending, depthWrite: false }));
    geo.dispose(); ls.position.copy(center); dyn.add(ls); return ls;
  }
  function line(a, b, color, dashed, opacity) {
    const g = new THREE.BufferGeometry().setFromPoints([a, b]);
    const mat = dashed
      ? new THREE.LineDashedMaterial({ color, dashSize: 0.35, gapSize: 0.3, transparent: true, opacity: opacity || 0.45, depthWrite: false })
      : new THREE.LineBasicMaterial({ color, transparent: true, opacity: opacity || 0.6, blending: THREE.AdditiveBlending, depthWrite: false });
    const l = new THREE.Line(g, mat); if (dashed) l.computeLineDistances(); dyn.add(l); return l;
  }
  function packet(a, b, color, phase, speed) {
    const s = new THREE.Sprite(new THREE.SpriteMaterial({ map: glow, color, transparent: true, opacity: 0, blending: THREE.AdditiveBlending, depthWrite: false }));
    s.scale.setScalar(0.55); dyn.add(s);
    packets.push({ sprite: s, a: a.clone(), b: b.clone(), t: phase % 1, speed });
  }
  function ghost(pos, phase) {
    const geo = new THREE.OctahedronGeometry(0.85, 0);
    const ls = new THREE.LineSegments(new THREE.EdgesGeometry(geo), new THREE.LineBasicMaterial({ color: HEX.ghost, transparent: true, opacity: 0.6, depthWrite: false }));
    geo.dispose(); ls.position.copy(pos); dyn.add(ls);
    if (!reduce) pulses.push({ obj: ls, base: 1, phase, spin: true });
  }

  function signature(m) {
    return JSON.stringify([mode, narrow(), m.self.label, m.self.isMaker, m.ghosts, m.serverLabel, m.peers.map(p => [p.id, p.local, p.state, p.label, p.state === 'syncing' ? Math.round((p.frac || 0) * 100) : 0, !!p.stale])]);
  }
  function paintToggle() {
    const set = (b, bg, fg) => { b.style.background = bg; b.style.color = fg; };
    mode === 'now' ? set(bNow, 'rgba(33,212,253,.18)', '#6df3ff') : set(bNow, 'transparent', '#5a93a8');
    mode === 'goal' ? set(bGoal, 'rgba(192,132,252,.2)', '#d8b4fe') : set(bGoal, 'transparent', '#5a93a8');
  }

  function buildNow() {
    const locals = model.peers.filter(p => p.local), remotes = model.peers.filter(p => !p.local);
    const out = model.self.isMaker; // blocks leave a block-maker; a follower receives them
    const speed = reduce ? 0 : 0.42;
    caption.innerHTML = '<span style="color:#6df3ff;font-weight:600">NOW</span> · measured' + (narrow() ? '' : ' · each line = one connection, its colour = does that node hold this node’s exact state at its own height');
    legend.innerHTML = narrow()
      ? '<span style="color:#00e0c6">━</span> same &nbsp;<span style="color:#ff5c7a">━</span> differs &nbsp;<span style="color:#9db4ff">◔</span> sync &nbsp;<span style="color:#5fd4ff">◯</span> machine &nbsp;<span style="color:#fbbf24">●</span> maker &nbsp;<span style="color:#c084fc">◇</span> missing'
      : '<span style="color:#00e0c6">━</span> same state ✓ &nbsp;<span style="color:#fbbf24">━</span> checking &nbsp;<span style="color:#ff5c7a">━</span> differs &nbsp;<span style="color:#9db4ff">◔</span> syncing &nbsp;<span style="color:#5a93a8">┅</span> no view<br>'
      + '<span style="color:#5fd4ff">◯</span> one machine &nbsp;<span style="color:#fbbf24">●</span> block-maker &nbsp;<span style="color:#c084fc">◇</span> missing for real BFT';

    shell(ORIGIN, 5.6, 0.15);
    label(model.serverLabel, new THREE.Vector3(0, 6.3, 0), CSS.shell, false);
    ball(ORIGIN, 0.95, model.self.isMaker ? HEX.maker : HEX.self, true);
    label(model.self.label, ORIGIN, model.self.isMaker ? CSS.maker : CSS.self, true);

    const place = (p, pos, r) => {
      const c = HEX[p.state] || HEX.unknown;
      line(ORIGIN, pos, c, p.state === 'unknown' || p.state === 'checking' || p.state === 'syncing', p.local ? 0.5 : 0.75);
      ball(pos, r, c, p.state === 'agree');
      if (p.state === 'syncing') ring(pos, r * 2.1, p.frac || 0, c);
      label(p.label + (p.state === 'syncing' ? ' ◔ ' + Math.floor((p.frac || 0) * 100) + ' %' + (p.stale ? ' (stale)' : '') : (MARK[p.state] || '')), pos, CSS[p.state] || CSS.unknown, false);
      if (p.state === 'agree') for (let k = 0; k < 2; k++) {
        const ph = k / 2 + hash01(p.id);
        out ? packet(ORIGIN, pos, c, ph, speed) : packet(pos, ORIGIN, c, ph, speed);
      }
      if (p.state === 'syncing') packet(out ? ORIGIN : pos, out ? pos : ORIGIN, c, hash01(p.id), speed * 0.6);
    };
    locals.forEach((p, i) => {
      const a = (i / Math.max(1, locals.length)) * Math.PI * 2 + hash01(p.id) * 0.3;
      const y = (hash01(p.id + 'y') - 0.5) * 3.2;
      place(p, new THREE.Vector3(Math.cos(a) * 4.1, y, Math.sin(a) * 4.1), 0.4);
    });
    remotes.forEach((p, i) => {
      const a = (i / Math.max(1, remotes.length)) * Math.PI * 2 + 0.55 + hash01(p.id) * 0.25;
      const y = (hash01(p.id + 'y') - 0.5) * 3.2;
      const pos = new THREE.Vector3(Math.cos(a) * 11, y, Math.sin(a) * 11);
      shell(pos, 1.3, 0.24);
      place(p, pos, 0.58);
    });
    for (let g = 0; g < model.ghosts; g++) { // the shape a decentralized network still needs
      const a = (g / model.ghosts) * Math.PI * 2 + Math.PI / Math.max(1, model.ghosts) + 0.3;
      const pos = new THREE.Vector3(Math.cos(a) * 15, g % 2 ? 1.8 : -1.4, Math.sin(a) * 15);
      ghost(pos, g * 2.1);
      line(ORIGIN, pos, HEX.ghost, true, 0.22);
      label('missing: independent signer ' + (4 - model.ghosts + g + 1) + ' of 4', pos, CSS.ghost, false);
    }
  }

  function buildGoal() { // the target, not data: 4 independent block-makers on 4 machines + the followers we have today
    const speed = reduce ? 0 : 0.5;
    const n = 4, R = 7.5, signers = [];
    caption.innerHTML = '<span style="color:#d8b4fe;font-weight:600">GOAL</span> · the mainnet target, <b style="color:#fbbf24">not live</b>' + (narrow() ? '' : ' · 4 independent block-makers on 4 machines; a block is final when 3 of 4 sign it');
    legend.innerHTML = narrow()
      ? '<span style="color:#fbbf24">●</span> maker + signer, own machine &nbsp;<span style="color:#00e0c6">━</span> cross-check &nbsp;<span style="color:#21d4fd">●</span> follower'
      : '<span style="color:#fbbf24">●</span> block-maker + finality signer, own machine &nbsp;<span style="color:#00e0c6">━</span> they check each other<br>'
      + '<span style="color:#21d4fd">●</span> followers (today’s ' + model.peers.length + ' nodes) — each one checks every block';
    for (let i = 0; i < n; i++) {
      const a = (i / n) * Math.PI * 2 + Math.PI / 4;
      const pos = new THREE.Vector3(Math.cos(a) * R, (i % 2 ? 1 : -1) * 0.9, Math.sin(a) * R);
      signers.push(pos);
      shell(pos, 2.0, 0.2);
      ball(pos, 0.8, HEX.maker, true);
      label(i === 0 ? 'Epsilon (today’s only one)' : 'independent operator ' + (i + 1), pos, i === 0 ? CSS.maker : '#f5d58a', i === 0);
    }
    for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) { // full mesh: every signer checks every other
      line(signers[i], signers[j], HEX.agree, false, 0.55);
      packet(signers[i], signers[j], HEX.agree, hash01(i + ':' + j), speed);
      packet(signers[j], signers[i], HEX.agree, hash01(j + ':' + i), speed);
    }
    const f = Math.max(6, Math.min(16, model.peers.length || 9));
    for (let k = 0; k < f; k++) { // followers, each tied to two signers it checks against
      const a = (k / f) * Math.PI * 2 + 0.2;
      const pos = new THREE.Vector3(Math.cos(a) * 14.5, (hash01('fy' + k) - 0.5) * 4, Math.sin(a) * 14.5);
      ball(pos, 0.32, HEX.self, false);
      const s1 = signers[k % n], s2 = signers[(k + 1) % n];
      line(pos, s1, HEX.self, false, 0.18); line(pos, s2, HEX.self, false, 0.12);
      if (k % 2 === 0) packet(s1, pos, HEX.self, hash01('p' + k), speed * 0.7);
    }
    label('finality: 3 of 4 signatures', new THREE.Vector3(0, 3.6, 0), '#d8b4fe', true);
  }

  function build() {
    disposeGroup(dyn); overlay.textContent = ''; labels = []; packets = []; pulses = []; billboards = [];
    paintToggle();
    if (!model) return;
    mode === 'goal' ? buildGoal() : buildNow();
  }
  function setMode(m) { if (m === mode) return; mode = m; sig = ''; if (model) { sig = signature(model); build(); } else paintToggle(); }
  bNow.addEventListener('click', () => setMode('now'));
  bGoal.addEventListener('click', () => setMode('goal'));
  paintToggle();

  function frame(t) {
    raf = requestAnimationFrame(frame);
    if (!running || document.hidden) { last = t; return; }
    const dt = Math.min(0.05, (t - last) / 1000); last = t;
    if (!drag && t > idleUntil && !reduce) root.rotation.y += dt * 0.09;
    for (const k of packets) {
      k.t = (k.t + dt * k.speed) % 1;
      k.sprite.position.lerpVectors(k.a, k.b, k.t);
      k.sprite.material.opacity = Math.sin(k.t * Math.PI) * 0.95;
    }
    for (const p of pulses) {
      if (p.spin) { p.obj.rotation.y += dt * 0.6; p.obj.rotation.x += dt * 0.25; }
      else p.obj.scale.setScalar(p.base * (1 + Math.sin(t / 1000 * 2.1 + p.phase) * 0.12));
    }
    root.updateMatrixWorld();
    if (billboards.length) { root.getWorldQuaternion(q).invert().multiply(camera.quaternion); for (const b of billboards) b.quaternion.copy(q); }
    renderer.render(scene, camera);
    const w = W(), h = H();
    for (const L of labels) {
      v.copy(L.pos).applyMatrix4(root.matrixWorld);
      L.depth = v.distanceTo(camera.position);
      v.project(camera);
      L.vis = v.z > -1 && v.z < 1;
      L.x = (v.x * 0.5 + 0.5) * w; L.y = (-v.y * 0.5 + 0.5) * h + L.dy;
      const half = Math.min(w / 2, L.w / 2 + 4); L.x = Math.min(w - half, Math.max(half, L.x)); // slide inward, never cut off
    }
    const order = labels.slice().sort((a, b) => (b.strong - a.strong) || (a.depth - b.depth)), placed = [];
    for (const L of order) {
      L.el.style.display = L.vis ? '' : 'none';
      if (!L.vis) continue;
      const r = [L.x - L.w / 2, L.y - 7, L.x + L.w / 2, L.y + 7];
      const hit = placed.some(q => r[0] < q[2] && r[2] > q[0] && r[1] < q[3] && r[3] > q[1]);
      if (!hit) placed.push(r);
      L.el.style.transform = 'translate(-50%,-50%) translate(' + L.x.toFixed(1) + 'px,' + L.y.toFixed(1) + 'px)';
      L.el.style.opacity = hit ? '0.13' : String(Math.max(0.4, Math.min(1, 1.6 - L.depth / 45)));
    }
  }
  raf = requestAnimationFrame(frame);

  el.addEventListener('pointerdown', e => { drag = { x: e.clientX, y: e.clientY, ry: root.rotation.y, rx: root.rotation.x }; try { el.setPointerCapture(e.pointerId); } catch (_) {} el.style.cursor = 'grabbing'; });
  el.addEventListener('pointermove', e => { if (!drag) return; root.rotation.y = drag.ry + (e.clientX - drag.x) * 0.008; root.rotation.x = Math.max(-0.5, Math.min(0.9, drag.rx + (e.clientY - drag.y) * 0.005)); });
  const up = () => { drag = null; idleUntil = performance.now() + 3000; el.style.cursor = 'grab'; };
  el.addEventListener('pointerup', up); el.addEventListener('pointercancel', up);
  const ro = new ResizeObserver(() => { renderer.setSize(W(), H()); camera.aspect = W() / H(); camera.updateProjectionMatrix(); placeCamera(); if (model) { const s2 = signature(model); if (s2 !== sig) { sig = s2; build(); } } });
  ro.observe(host);

  return {
    update(m) {
      if (dead || !m) return;
      model = m; const s = signature(m);
      if (s !== sig) { sig = s; build(); }
    },
    pause() { running = false; },
    resume() { running = true; last = performance.now(); },
    destroy() { dead = true; cancelAnimationFrame(raf); ro.disconnect(); disposeGroup(dyn); renderer.dispose(); host.textContent = ''; },
  };
}
