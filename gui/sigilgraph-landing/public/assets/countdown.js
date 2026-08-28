/* ─────────────────────────────────────────────────────────────────────────────
   SIGIL Graph — mainnet launch countdown
   2026-08-26 (rocky, operator request)

   Plain script, no build step, no dependency on the React app. The markup is
   static in index.html so the panel paints on first byte; this only fills in
   numbers. If this file fails to load the page still renders correctly — the
   digits simply stay at their served placeholder.

   TARGET: set in ONE place, below. Operator confirmed 13:00 (1 pm) on
   18 December 2026, Europe/Copenhagen (CET, UTC+1 in December). Written as an explicit UTC instant so it is the
   same moment for every visitor regardless of their own clock or timezone —
   a countdown that says something different in Copenhagen and New York is a
   bug, and building it from local-time parts is how that happens.
   ──────────────────────────────────────────────────────────────────────────── */
(function () {
  "use strict";

  // 2026-12-18 13:00:00 CET  ==  2026-12-18 12:00:00 UTC
  var TARGET_MS = Date.parse("2026-12-18T12:00:00Z");
  var TARGET_LABEL = "18 DEC 2026 · 13:00 CET";

  // Where the progress rail starts filling from. Today, so the bar reads as
  // "distance covered since we announced" rather than an arbitrary epoch.
  var ORIGIN_MS = Date.parse("2026-08-26T00:00:00Z");

  var root = document.querySelector("[data-sg-countdown]");
  if (!root || !TARGET_MS) return;

  var el = {
    d: root.querySelector('[data-cd="d"]'),
    h: root.querySelector('[data-cd="h"]'),
    m: root.querySelector('[data-cd="m"]'),
    s: root.querySelector('[data-cd="s"]'),
    fill: root.querySelector("[data-cd-fill]"),
    target: root.querySelector("[data-cd-target]"),
    sr: root.querySelector("[data-cd-sr]")
  };
  if (!el.d || !el.h || !el.m || !el.s) return;

  if (el.target) el.target.textContent = TARGET_LABEL;

  function pad(n, width) {
    var s = String(n);
    while (s.length < width) s = "0" + s;
    return s;
  }

  // Only touch the DOM when the value actually changed. Three of the four
  // panels are unchanged on almost every tick, and repainting them would make
  // the tick animation fire on all four at once — which looks broken.
  function put(node, next) {
    if (node.textContent === next) return;
    node.textContent = next;
    node.classList.remove("is-tick");
    // Force reflow so the animation restarts even when ticks are close together.
    void node.offsetWidth;
    node.classList.add("is-tick");
  }

  var lastAnnounce = 0;

  function frame() {
    var now = Date.now();
    var left = TARGET_MS - now;

    if (left <= 0) {
      root.classList.add("is-launched");
      put(el.d, "00"); put(el.h, "00"); put(el.m, "00"); put(el.s, "00");
      if (el.sr) el.sr.textContent = "SIGIL Graph mainnet is live.";
      if (el.fill) el.fill.style.width = "100%";
      return; // stop scheduling — nothing left to count
    }

    var sec = Math.floor(left / 1000);
    var days = Math.floor(sec / 86400);
    var hours = Math.floor((sec % 86400) / 3600);
    var mins = Math.floor((sec % 3600) / 60);
    var secs = sec % 60;

    put(el.d, pad(days, 2));
    put(el.h, pad(hours, 2));
    put(el.m, pad(mins, 2));
    put(el.s, pad(secs, 2));

    if (el.fill) {
      var span = TARGET_MS - ORIGIN_MS;
      var pct = span > 0 ? ((now - ORIGIN_MS) / span) * 100 : 0;
      el.fill.style.width = Math.max(0, Math.min(100, pct)).toFixed(3) + "%";
    }

    // Screen readers get a calm, human sentence once a minute. The digits
    // themselves are aria-hidden — a live region firing every second is
    // genuinely hostile to a screen-reader user.
    if (el.sr && now - lastAnnounce > 60000) {
      lastAnnounce = now;
      el.sr.textContent =
        days + " days, " + hours + " hours and " + mins +
        " minutes until the SIGIL Graph mainnet launch.";
    }

    schedule();
  }

  // Align to the next second boundary instead of setInterval(1000). setInterval
  // drifts, and a countdown that skips or repeats a second is exactly the kind
  // of detail that makes a launch page look unfinished.
  function schedule() {
    var delay = 1000 - (Date.now() % 1000);
    setTimeout(frame, delay < 30 ? delay + 1000 : delay);
  }

  frame();

  // A backgrounded tab throttles timers, so the clock can be badly stale on
  // return. Recompute immediately when the page becomes visible again.
  document.addEventListener("visibilitychange", function () {
    if (!document.hidden) frame();
  });
})();
