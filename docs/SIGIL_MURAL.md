# SIGIL — The Red Line of Least Action
### Companion document to the 8-piece mural

> `δS_SIGIL[g, A, φ, J, K, Σ] = 0`
>
> Live: **https://quillon.xyz/sigil-mural.html** · still image: `sigil-mural.png`
> Source: `/home/orobit/q-narwhalknight/dist-final/sigil-mural.html` (pure SVG, 2520 × 1320, framed)

---

## 1. What the painting *is*

Eight panels, one painting. The whole chain is written as a **variational principle**:
there is a single action functional `S_SIGIL` over a Lorentzian BlockDAG braid, and the
chain *is* the configuration that makes that action **stationary** — `δS = 0`. Each of the
six fields (`g, A, φ, J, K, Σ`) gets its own Euler–Lagrange cell (`δS/δ·`), plus two
"operator" cells (the **fold** `∮` and the **forge** `Σ→∞`) that show what the stationary
chain is *for*: a light client that joins in one proof, and 21M struck once at genesis.

The mural is the [[project_sigil_master_equation]] made visible for a visual learner —
not decoration, a map. Every panel points at a real crate.

The single **crimson thread** is the worldline of least action — the path `δS = 0`
carves through the whole braid. It is deliberately the same crimson as the **compute
fabric's +10% red line** ([[project_flux_compute_fabric_vast]]): the economic seam that
runs through every rented GPU is the same seam that runs through the physics. One line,
one principle, top to bottom.

---

## 2. The eight panels, term by term

Read **boustrophedon** (ox-plough order) so the red line never lifts:
`1 → 2 → 3 → 4` across the top, drop into `5`, then `5 → 6 → 7 → 8` back across the bottom.

| # | Cell | Name | What it depicts | Backed by (real code) |
|---|------|------|-----------------|-----------------------|
| 1 | `δS/δg` | **The Braid** | BlockDAG geometry — woven parents→child lattice, *not* one chain. The metric term: how block-time is shaped. | `sigil-state`, `sigil-chronos` DAG ([[project_sigil_chronos]]) |
| 2 | `δS/δA` | **Emission** | The mint gauge field under the dashed **21,000,000 cap**, *committed in the state roots* — the fix for Quillon's 3-day wrong-emission bug. | `sigil-state` emission ([[project_sigil_emission_oracle]], [[project_sigil_supply_cap]]) |
| 3 | `δS/δφ` | **Settlement** | The field of value — sends, swaps, DEX. Every transfer is a ripple in φ. | `sigil-rpc::execute_swap` ([[project_sigil_rpc_keystone]]) |
| 4 | `δS/δJ` | **Witness current** | DagKnight **leaderless** ordering — the knight ♞ picks the canonical braid with no leader to bribe. | `sigil-chronos` DagKnight order |
| 5 | `δS/δK` | **Topology** | The gossip mesh. *One unique `peer_id` per node — collapse them and the mesh dies.* | `flux-p2p` (the peer_id-collapse lesson, hard-won this session) |
| 6 | `δS/δΣ` | **Crypto triskelion** | Defense-in-depth: **SQIsign ⊕ RLWE ⊕ BLAKE**. Break one ring, the other two still hold. | `flux-sqisign`, `zk-flux`, `flux-eternal-cypher` ([[project_zk_flux]]) |
| 7 | `∮ fold` | **The Fold** | Light client — join at height N, verify **one** constant-size fold proof instead of replaying 100k blocks. | `flux-fold` + late-join catch-up ([[project_sigil_fold_late_join]]) |
| 8 | `Σ→∞` | **The Forge & Horizon** | The crucible: genesis → testnet **Day 200 (2026-12-17)** dawn. 21M forged once; the line runs to the horizon. | [[project_sigil_200day_testnet_runway]] |

---

## 3. The art technique (why it reads as *one* painting)

The challenge of an 8-cell mural is the rectangular grid — the eye sees a contact sheet,
not a painting. The seams are dissolved with **light, never cropping**:

- **Stitch layer A** (under the art, `mix-blend-mode:screen`): a wide mauve nebula band
  fusing the top and bottom rows, plus broad cyan/gold washes that each span several cells.
- **Stitch layer B** (over the art): radial **blooms placed exactly on the seam crossings**,
  so the brightest light sits where the hard edges would otherwise read.
- **Two diagonal light shafts** crossing all panels at ±, tying the composition corner-to-corner.
- A **vignette** dissolve that pulls the eye inward and kills the grid feel at the margins.
- The **red line** itself ignores all seams — one continuous Bézier path from top-left to
  bottom-left, with a dashed white "flow" overlay animated along it (`stroke-dashoffset`) so
  the worldline visibly *moves* (toggle to a still for print/export).

Frame: an "epic" gilt frame (`goldframe` gradient) with hexagonal **sigil corner ornaments**,
a top **S I G I L** cartouche, and a bottom plate carrying the master equation. The hexagon-
with-descending-stroke is the SIGIL glyph, repeated at the forge core in panel 8.

---

## 4. Controls (in the live page)

- **Show the 8 seams** — lifts the grid: dashed seam lines + a numbered overlay (1–8 in
  boustrophedon order). Use this to **export each piece individually** for printing.
- **Red line: flowing / still** — toggles the animated flow for a static export.

---

## 5. Honest checklist (what's real vs. illustrative)

- ✅ **Real:** the six-field action and panel→crate mapping are the actual SIGIL architecture;
  the 21M cap, root-committed emission, DagKnight ordering, unique-peer_id rule, triple-crypto
  stack, fold light-client, and Day-200 date are all live design facts in the named crates/memories.
- ⚠️ **Illustrative:** the fold panel's `2,568 B · 342 ms` figures are a representative size/time
  label, not a fixed benchmark — the measured late-join fold proof was constant-size (~392 B in the
  `run_late_join_fold` report); treat the on-canvas numbers as a poster figure, and cite
  [[project_sigil_fold_late_join]] for the measured values.
- ⚠️ **Artistic:** the braid/settlement/mesh node positions are composed for balance, not laid
  out from live `xray`/peer data. (A future lane could drive panel 3/5 from real `sigil-rpc`
  state, the way `garden.html` is driven by `fluxc xray` — see §6.)

---

## 6. Possible next lanes

- **Live-data mural** — drive panels 3 (settlement) and 5 (topology) from real `sigil-rpc`
  state + `flux_peer_list`, turning the static art into a breathing dashboard (the
  "Living Tide" infographic Viktor liked is the same idea, one register up).
- **Per-piece export** — a small script to slice the SVG into 8 print-ready tiles using the
  seam coordinates already in the file (600/1200/1800 verticals, 600 horizontal).
- **Wire into `desktop.html`** — register the mural as a QuillonOS launcher surface (APPS +
  quick-launch + dock, per the flux-dev 3-place pattern).

---

*Tagline on the plate:* **"the red line: the path the action makes stationary · the fabric's +10% thread."**
Every settlement makes the action stationary. Every node a vertex of the braid. Together, one painting.
