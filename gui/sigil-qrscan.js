/* sigil-qrscan v1 (2026-11-20) — "Scan QR" for the Send window.
 *
 * Reads a payment QR with the webcam (or from a picture) and puts the text into the
 * recipient field exactly the way the Paste button does: value + an `input` event. Every
 * check after that is the page's own (sigilParsePayee / uniValidateAddr / the Send
 * studio's review step) — this file never decides on its own that something is payable.
 *
 * Decoding: the browser's built-in BarcodeDetector where it exists (Chrome on Android,
 * macOS, ChromeOS). Chrome on Windows and Linux has none, so jsQR 1.4.0 (Apache-2.0,
 * self-hosted beside this file, npm integrity checked) is loaded on first use only.
 *
 * Privacy: frames are decoded in this tab. Nothing is uploaded, nothing is stored, and the
 * camera is released the moment the scanner closes for any reason.
 */
(function () {
  'use strict';
  var modal = document.getElementById('send-modal');
  var addr = modal && modal.querySelector('[id="uniAddr"]');
  if (!modal || !addr || document.getElementById('ssScan')) return;

  var SELF = (document.currentScript && document.currentScript.src) || location.href;
  var JSQR_URL = new URL('jsQR-1.4.0.js', SELF).href;

  /* ── styles (scoped; the overlay lives on <body> so the Send window's Escape and
        Tab handlers never see its keys) ─────────────────────────────────────────── */
  var css = document.createElement('style');
  css.textContent = [
    '#sigilQrScan{position:fixed;inset:0;z-index:2147483000;display:flex;align-items:center;justify-content:center;background:#03060cd9;padding:16px;box-sizing:border-box}',
    '#sigilQrScan[hidden]{display:none}',
    '#sigilQrScan .qs-box{width:100%;max-width:440px;max-height:100%;overflow:auto;background:#0b111c;border:1px solid #354059;border-radius:18px;padding:18px;box-sizing:border-box;color:#e2e8f6;font:13px/1.6 system-ui,sans-serif}',
    '#sigilQrScan h3{margin:0 0 4px;font:500 17px system-ui,sans-serif;color:#e7dbff}',
    '#sigilQrScan .qs-sub{color:#9aa8bf;font-size:12px;margin:0 0 12px}',
    '#sigilQrScan .qs-view{position:relative;width:100%;aspect-ratio:1/1;background:#000;border-radius:14px;overflow:hidden}',
    '#sigilQrScan video{width:100%;height:100%;object-fit:cover;display:block}',
    '#sigilQrScan .qs-frame{position:absolute;inset:14%;border:2px solid #bda0ffcc;border-radius:14px;box-shadow:0 0 0 999px #00000059;pointer-events:none}',
    '#sigilQrScan .qs-view.found .qs-frame{border-color:#00e0c6}',
    '#sigilQrScan .qs-idle{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;text-align:center;padding:24px;color:#9aa8bf;font-size:12.5px}',
    // an author display:flex beats the UA [hidden]{display:none}, so restate it (found by screenshot)
    '#sigilQrScan .qs-idle[hidden],#sigilQrScan [hidden]{display:none!important}',
    '#sigilQrScan .qs-status{min-height:42px;margin:12px 0 4px;font-size:12.5px;color:#c5cde0;overflow-wrap:anywhere}',
    '#sigilQrScan .qs-status.err{color:#ffb4b4}',
    '#sigilQrScan .qs-row{display:flex;gap:8px;flex-wrap:wrap;margin-top:10px}',
    '#sigilQrScan button{border:1px solid #354059;border-radius:10px;background:#ffffff08;color:#c5cde0;padding:9px 13px;font:12.5px/1.4 system-ui,sans-serif;cursor:pointer}',
    '#sigilQrScan button:focus-visible{outline:2px solid #bda0ff;outline-offset:3px}',
    '#sigilQrScan button.qs-cancel{margin-left:auto}',
    '#sigilQrScan .qs-priv{margin-top:12px;color:#7f8ca3;font-size:11px}'
  ].join('\n');
  document.head.appendChild(css);

  /* ── the button, beside Paste (or right under the field if the Send studio is absent) ── */
  var btn = document.createElement('button');
  btn.type = 'button'; btn.id = 'ssScan'; btn.className = 'ss-button';
  btn.textContent = 'Scan QR';
  btn.title = 'Scan a SIGIL address or payment code with your camera';
  var tools = modal.querySelector('.ss-tools');
  if (tools) tools.appendChild(btn);
  else {
    btn.style.cssText = 'margin-top:8px;border:1px solid #354059;border-radius:10px;background:#ffffff05;color:#c5cde0;padding:8px 12px;font:12px/1.5 system-ui,sans-serif;cursor:pointer';
    addr.after(btn);
  }

  /* ── the overlay ────────────────────────────────────────────────────────────── */
  var ov = document.createElement('div');
  ov.id = 'sigilQrScan'; ov.hidden = true;
  ov.setAttribute('role', 'dialog'); ov.setAttribute('aria-modal', 'true'); ov.setAttribute('aria-labelledby', 'qsTitle');
  ov.innerHTML =
    '<div class="qs-box">' +
      '<h3 id="qsTitle">Scan a payment QR</h3>' +
      '<p class="qs-sub">Hold the recipient’s SIGIL QR code inside the frame.</p>' +
      '<div class="qs-view"><video id="qsVideo" playsinline muted autoplay></video><div class="qs-frame"></div>' +
        '<div class="qs-idle" id="qsIdle">Starting the camera…</div></div>' +
      '<div class="qs-status" id="qsStatus" role="status" aria-live="polite"></div>' +
      '<div class="qs-row">' +
        '<button type="button" id="qsFile">Use a picture instead</button>' +
        '<button type="button" id="qsSwitch" hidden>Switch camera</button>' +
        '<button type="button" id="qsCancel" class="qs-cancel">Cancel</button>' +
      '</div>' +
      '<input type="file" id="qsFileIn" accept="image/*" hidden>' +
      '<div class="qs-priv">The camera picture is read in this browser tab only. Nothing is uploaded or saved.</div>' +
    '</div>';
  document.body.appendChild(ov);
  var q = function (id) { return document.getElementById(id); };
  var video = q('qsVideo'), statusEl = q('qsStatus'), idle = q('qsIdle');

  var stream = null, timer = null, open = false, detector = null, jsqr = null, canvas = null, ctx = null;
  var cams = [], camIx = 0, lastBad = '', opener = null;

  function say(s, err) { statusEl.textContent = s || ''; statusEl.classList.toggle('err', !!err); }

  /* ── decoders ───────────────────────────────────────────────────────────────── */
  async function nativeDetector() {
    try {
      if (!('BarcodeDetector' in window)) return null;
      var f = await window.BarcodeDetector.getSupportedFormats();
      return f && f.indexOf('qr_code') >= 0 ? new window.BarcodeDetector({ formats: ['qr_code'] }) : null;
    } catch (_) { return null; }
  }
  var jsqrLoading = null;
  function loadJsQR() {
    if (window.jsQR) return Promise.resolve(window.jsQR);
    if (jsqrLoading) return jsqrLoading;
    jsqrLoading = new Promise(function (res, rej) {
      var s = document.createElement('script');
      s.src = JSQR_URL; s.async = true;
      s.onload = function () { window.jsQR ? res(window.jsQR) : rej(new Error('QR reader did not load')); };
      s.onerror = function () { jsqrLoading = null; rej(new Error('QR reader could not be downloaded')); };
      document.head.appendChild(s);
    });
    return jsqrLoading;
  }
  async function ensureDecoder() {
    if (detector || jsqr) return;
    detector = await nativeDetector();
    if (!detector) jsqr = await loadJsQR();
  }
  // Decode one drawable (video frame or image). Returns the QR text or null.
  async function decode(src, w, h, maxSide) {
    if (detector) {
      try { var codes = await detector.detect(src); if (codes && codes.length) return String(codes[0].rawValue || ''); } catch (_) {}
      if (!jsqr) return null;   // a picture the native reader missed falls through to jsQR below
    }
    if (!jsqr) return null;
    var scale = Math.min(1, maxSide / Math.max(w, h));
    var cw = Math.max(1, Math.round(w * scale)), ch = Math.max(1, Math.round(h * scale));
    if (!canvas) { canvas = document.createElement('canvas'); ctx = canvas.getContext('2d', { willReadFrequently: true }); }
    canvas.width = cw; canvas.height = ch;
    ctx.drawImage(src, 0, 0, cw, ch);
    var img = ctx.getImageData(0, 0, cw, ch);
    // attemptBoth: some screens show the code light-on-dark
    var r = jsqr(img.data, cw, ch, { inversionAttempts: 'attemptBoth' });
    return r && r.data ? String(r.data) : null;
  }

  /* ── what was read: accept, or explain in plain words and keep scanning ─────── */
  function classify(raw) {
    var t = String(raw || '').trim();
    if (/^sigil:\/*coin\?/i.test(t) || /\/claim\.html#/i.test(t))
      return { ok: false, why: 'That QR is a SIGIL coin (it carries the coin’s own key), not a payment address. Don’t pay to it — collect it instead.' };
    if (/^(ethereum:|polygon:)?0x[0-9a-f]{40}\b/i.test(t))
      return { ok: false, why: 'That is a Polygon/Ethereum address. Wrapped SIGIL on Polygon is sent from Bridge & Swap, not from here.' };
    var pp = window.sigilParsePayee ? window.sigilParsePayee(t) : (/^[0-9a-f]{64}$/i.test(t) ? { wallet: t } : null);
    if (!pp) return { ok: false, why: 'A QR was read, but it is not a SIGIL address or payment code: “' + (t.length > 48 ? t.slice(0, 48) + '…' : t) + '”' };
    return { ok: true, text: t, payee: pp };
  }

  // amount= in a payment code is in glyphs (1 SIGIL = 10^10 glyphs), the unit sigilParsePayee keeps.
  function glyphsToSigil(g) {
    try { var b = BigInt(g), u = 10000000000n, f = (b % u).toString().padStart(10, '0').replace(/0+$/, '');
      return (b / u).toString() + (f ? '.' + f : ''); } catch (_) { return String(g) + ' glyphs'; }
  }

  function accept(c) {
    var amt = q('uniAmt'), before = amt ? amt.value.trim() : '';
    close();
    addr.value = c.text;
    addr.dispatchEvent(new Event('input', { bubbles: true }));   // same path as Paste
    var msg = 'Recipient read from a QR code. Check the address before you pay.';
    if (c.payee && c.payee.amount && amt) {
      // uniValidateAddr fills an EMPTY amount field from the code; a typed amount is kept.
      var asked = glyphsToSigil(c.payee.amount);
      msg = before
        ? 'Recipient read from a QR code. The code asks for ' + asked + ' SIGIL; your amount field still says ' + before + ' — check which one you mean.'
        : 'Recipient and amount (' + asked + ' SIGIL) read from a QR code. Check both before you pay.';
    }
    var m = modal.querySelector('[id="uniMsg"]'); if (m) m.textContent = msg;
    try { ((amt && !amt.value.trim()) ? amt : addr).focus(); } catch (_) {}
  }

  /* ── camera loop ────────────────────────────────────────────────────────────── */
  function stopCamera() {
    if (timer) { clearTimeout(timer); timer = null; }
    if (stream) { stream.getTracks().forEach(function (t) { try { t.stop(); } catch (_) {} }); stream = null; }
    try { video.srcObject = null; } catch (_) {}
  }
  async function tick() {
    timer = null;
    if (!open || !stream) return;
    if (video.readyState >= 2 && video.videoWidth) {
      var text = null;
      try { text = await decode(video, video.videoWidth, video.videoHeight, 720); } catch (_) {}
      if (!open) return;
      if (text) {
        var c = classify(text);
        if (c.ok) { ov.querySelector('.qs-view').classList.add('found'); accept(c); return; }
        if (text !== lastBad) { lastBad = text; say(c.why, true); }
      }
    }
    timer = setTimeout(tick, 140);
  }
  async function startCamera() {
    stopCamera();
    idle.hidden = false; idle.textContent = 'Starting the camera…';
    if (!window.isSecureContext || !navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      idle.textContent = 'This browser cannot open a camera on this page.';
      say('Use “Use a picture instead”, or Paste the address.', true);
      return;
    }
    var want = cams.length ? { deviceId: { exact: cams[camIx].deviceId } } : { facingMode: { ideal: 'environment' } };
    try {
      stream = await navigator.mediaDevices.getUserMedia({ video: Object.assign(want, { width: { ideal: 1280 }, height: { ideal: 720 } }), audio: false });
    } catch (e) {
      var n = e && e.name;
      idle.textContent = n === 'NotAllowedError' ? 'Camera permission was refused.'
        : n === 'NotFoundError' || n === 'OverconstrainedError' ? 'No camera was found on this device.'
        : n === 'NotReadableError' ? 'The camera is busy in another app or tab.'
        : 'The camera could not be opened.';
      say(n === 'NotAllowedError'
        ? 'Allow the camera for this site (the camera icon in the address bar), or use a picture instead.'
        : 'You can still use a picture of the QR code, or Paste the address.', true);
      return;
    }
    if (!open) { stopCamera(); return; }
    video.srcObject = stream;
    try { await video.play(); } catch (_) {}
    idle.hidden = true;
    say('Looking for a QR code…');
    if (!cams.length) {
      try {
        var all = await navigator.mediaDevices.enumerateDevices();
        cams = all.filter(function (d) { return d.kind === 'videoinput'; });
        var cur = stream.getVideoTracks()[0], id = cur && cur.getSettings ? cur.getSettings().deviceId : null;
        camIx = Math.max(0, cams.findIndex(function (d) { return d.deviceId === id; }));
      } catch (_) {}
    }
    q('qsSwitch').hidden = cams.length < 2;
    timer = setTimeout(tick, 60);
  }

  /* ── open / close ───────────────────────────────────────────────────────────── */
  async function openScanner() {
    if (open) return;
    open = true; lastBad = ''; opener = document.activeElement;
    ov.querySelector('.qs-view').classList.remove('found');
    ov.hidden = false; say('');
    q('qsCancel').focus();
    try { await ensureDecoder(); }
    catch (e) { idle.hidden = false; idle.textContent = 'The QR reader could not load.'; say((e && e.message) || 'QR reader unavailable. Paste the address instead.', true); return; }
    if (open) startCamera();
  }
  function close() {
    if (!open) return;
    open = false; stopCamera(); ov.hidden = true; say('');
    if (opener && opener.isConnected && typeof opener.focus === 'function') { try { opener.focus(); } catch (_) {} }
  }

  btn.addEventListener('click', openScanner);
  q('qsCancel').addEventListener('click', close);
  q('qsSwitch').addEventListener('click', function () { if (cams.length > 1) { camIx = (camIx + 1) % cams.length; startCamera(); } });
  q('qsFile').addEventListener('click', function () { q('qsFileIn').value = ''; q('qsFileIn').click(); });
  q('qsFileIn').addEventListener('change', async function () {
    var f = this.files && this.files[0]; if (!f) return;
    say('Reading the picture…');
    try {
      await ensureDecoder();
      var bmp = await createImageBitmap(f);
      var text = null, sides = [1600, 1000, 640];   // big photos decode better scaled down
      for (var i = 0; i < sides.length && !text; i++) text = await decode(bmp, bmp.width, bmp.height, sides[i]);
      if (!open) return;
      if (!text) { say('No QR code was found in that picture. Try a sharper, straighter photo.', true); return; }
      var c = classify(text);
      if (c.ok) accept(c); else say(c.why, true);
    } catch (e) { say('That file could not be read as a picture.', true); }
  });
  ov.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); close(); return; }
    if (e.key !== 'Tab') return;
    var f = Array.from(ov.querySelectorAll('button')).filter(function (b) { return !b.hidden && !b.disabled; });
    if (!f.length) return;
    var first = f[0], last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  });
  ov.addEventListener('click', function (e) { if (e.target === ov) close(); });
  // Release the camera whenever the user leaves: tab hidden, page left, Send window closed.
  document.addEventListener('visibilitychange', function () { if (document.hidden) close(); });
  window.addEventListener('pagehide', close);
  var prevClose = window.closeSend;
  if (typeof prevClose === 'function') window.closeSend = function () { close(); return prevClose.apply(this, arguments); };

  window.sigilQrScan = { open: openScanner, close: close, classify: classify, isOpen: function () { return open; }, cameraLive: function () { return !!stream; } };
})();
