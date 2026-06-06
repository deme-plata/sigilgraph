
export const GOLD = "#fbbf24", VIOL = "#c084fc", ACC = "#8b5cf6";
export const NS = "http://www.w3.org/2000/svg";

export function el(n, a = {}) {
  const e = document.createElementNS(NS, n);
  for (const k in a) e.setAttribute(k, a[k]);
  return e;
}
export const rnd = (a, b) => a + Math.random() * (b - a);

export function computeKappa(mass, radius, temp, entropy, eulerChi = 0) {
  const lp = 1.616e-35, mp = 2.176e-8, kb = 1.38e-23, c = 3e8;
  const thermal = (kb * temp) / (mp * c * c);
  const geometric = (lp / radius) ** 2;
  const massTerm = (mp / mass) ** 0.5;
  const kappaBase = Math.min(1, thermal * geometric * massTerm);
  const topology = Math.exp(-Math.abs(eulerChi));
  const delta = Math.exp(-temp / 300);
  const lambda = 1 - Math.exp(-mass / 1e-24);
  const omega = 1 - Math.exp(-entropy / 1e12);
  const kappa = Math.max(1e-10, Math.min(2, kappaBase * topology * delta * lambda * omega));
  const decoherence = delta * (1 - lambda);
  const merit = kappa * omega * (1 - decoherence);
  return { kappa, delta, lambda, omega, merit, decoherence, kappaBase, topology };
}

export function phaseFromKappa(k) {
  if (k < 0.3) return { id: "classical", label: "Classical", aura: "crystalline determinism" };
  if (k < 1) return { id: "transitional", label: "Transitional", aura: "Lindblad edge" };
  return { id: "quantum_coherent", label: "Quantum Coherent", aura: "Harlow-QEC weave" };
}

export function tierFor(k, merit, decoherence) {
  const ph = phaseFromKappa(k);
  if (ph.id === "classical" && merit > 0.05 && decoherence < 0.8) return 1;
  if (ph.id === "transitional" && merit > 0.15) return 2;
  if (ph.id === "quantum_coherent" && merit > 0.25) return 3;
  return 0;
}

export const TIER_NAMES = [
  "Spectator (Nullius)", "Candidate (Liminal)",
  "Cohærens (Quantum Citizen)", "Propositor (Sigil Steward)"
];

export async function sha256hex(s) {
  const b = new TextEncoder().encode(s);
  const h = await crypto.subtle.digest("SHA-256", b);
  return [...new Uint8Array(h)].map(x => x.toString(16).padStart(2, "0")).join("");
}

export function injectNav(active) {
  const nav = document.querySelector("nav.site");
  if (!nav) return;
  const links = [
    ["index.html", "⬡ Hub"],
    ["kappa.html", "κ Engine"],
    ["ritual.html", "ProbationeKappa"],
    ["lindblad.html", "Lindblad Δ/Λ"],
    ["nations.html", "Nations"],
    ["bridge.html", "Beta Bridge"],
    ["mcp.html", "MCP Tools"],
  ];
  nav.innerHTML = links.map(([href, label]) =>
    `<a href="${href}" class="${active === href ? "active" : ""}">${label}</a>`
  ).join("") +
    `<a href="/sigil-nation/index.html">🌍 Sigil Nation</a>` +
    `<a href="https://fluxapp.xyz/sigil-wallet-tron.html">🪙 Wallet</a>`;
}

export function drawNationsMap(svgId) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  svg.innerHTML = "";
  const defs = el("defs");
  const rg = el("radialGradient", { id: "halo", cx: "50%", cy: "50%", r: "50%" });
  rg.appendChild(el("stop", { offset: "0%", "stop-color": "rgba(192,132,252,.35)" }));
  rg.appendChild(el("stop", { offset: "100%", "stop-color": "rgba(192,132,252,0)" }));
  defs.appendChild(rg);
  svg.appendChild(defs);
  for (let i = 0; i < 70; i++)
    svg.appendChild(el("circle", { cx: rnd(0, 1460), cy: rnd(0, 720), r: rnd(0.5, 1.5),
      fill: i % 7 ? ACC : GOLD, opacity: rnd(0.15, 0.6) }));
  const NATIONS = [
    { name: "SIGIL NATION", cx: 730, cy: 360, r: 200, home: true, pop: 4200, M: 3, N: 5 },
    { name: "FLUXHOLD", cx: 280, cy: 280, r: 120, pop: 2310, M: 5, N: 7 },
    { name: "PROVENANCE", cx: 1100, cy: 260, r: 115, pop: 1840, M: 4, N: 6 },
    { name: "QUORUM VALE", cx: 320, cy: 560, r: 110, pop: 1490, M: 3, N: 5 },
    { name: "KEELPORT", cx: 1050, cy: 540, r: 118, pop: 2070, M: 5, N: 8 },
  ];
  for (const n of NATIONS) {
    const g = el("g");
    g.appendChild(el("circle", { cx: n.cx, cy: n.cy, r: n.r + 18, fill: "url(#halo)" }));
    g.appendChild(el("circle", { cx: n.cx, cy: n.cy, r: n.r, fill: "#14101f",
      stroke: n.home ? GOLD : VIOL, "stroke-width": n.home ? 3 : 1.5 }));
    const t = el("text", { x: n.cx, y: n.cy - 8, "text-anchor": "middle",
      fill: n.home ? GOLD : VIOL, "font-size": n.home ? 16 : 12, "font-family": "monospace" });
    t.textContent = n.name;
    g.appendChild(t);
    const s = el("text", { x: n.cx, y: n.cy + 14, "text-anchor": "middle",
      fill: "#8b8ba0", "font-size": 10, "font-family": "monospace" });
    s.textContent = `${n.pop} citizens · ${n.M}/${n.N} quorum`;
    g.appendChild(s);
    svg.appendChild(g);
  }
}

export function ritualAdmission(node, swarm) {
  const admitted = node.tier >= 2 && swarm.boundary < 0.35 &&
    swarm.decoherence < 0.55 && node.merit > 0.12;
  return { admitted, iou: admitted ? 1000 : 0 };
}
