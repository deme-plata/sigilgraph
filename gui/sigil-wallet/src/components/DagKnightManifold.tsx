// DagKnightManifold.tsx — SIGIL's BlockDAG braid rendered as a Lorentzian
// 4-manifold: standalone visualization page, driven by REAL live GHOSTDAG
// data from GET /v1/dagknight/recent (sigil-api), refreshed every 5s.
//
// The vocabulary is SIGIL's own, not invented here — see
// sigil/SIGIL_MASTER_EQUATION_v0.md: "SIGIL's BlockDAG braid 𝒢 evolves as a
// Lorentzian 4-manifold (3 spatial + 1 temporal degrees of freedom)...
// Vertices = blocks. Edges = causal links." This is a STYLIZED reading of
// that language, not a literal general-relativity simulation: two spatial
// axes (x, y) fan out blocks that share a height (parallel candidates —
// SIGIL mints several before GHOSTDAG picks the blue winner), the third
// axis (z, depth) is time — newest blocks nearest the camera, older ones
// receding away. Causal edges (parent_hash + merge_parents, per the block's
// real header) are drawn as glowing lines with a light-cone taper: wider at
// the earlier (parent) end, narrowing toward the later (child) block,
// echoing a light cone's forward-narrowing causal boundary. Blue/red
// coloring comes straight from `is_blue` in the API response — the real
// GHOSTDAG k-cluster verdict for each block, not a demo flag.
//
// Reuses this app's existing @react-three/fiber + @react-three/drei
// dependencies (already used by QuantumMixerVisualization.tsx — this file
// follows that component's Canvas/lighting/palette conventions) and SIGIL's
// locked obsidian+violet visual identity (see the `sigil` skill's "Visual
// identity" section): --bg #0a0a0f, --accent #8b5cf6, --accent-bright
// #c084fc, --sigil-gold #fbbf24, JetBrains Mono.

import { useEffect, useRef, useState, useMemo, useCallback } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { OrbitControls, Line, Text } from '@react-three/drei';
import * as THREE from 'three';

// ── real API shape (mirrors sigil_api::dagknight::DagSnapshot / BlockSummary) ──
interface BlockSummary {
  hash: string;          // hex
  parent: string;        // hex
  merge_parents: string[];
  height: number;
  producer: string;      // hex
  blue_score: number;
  is_blue: boolean;
}
interface DagSnapshot {
  blocks: BlockSummary[];
  k: number | null;
  updated_at_ms: number;
}
interface ApiEnvelope<T> {
  ok: boolean;
  data?: T;
  error?: string;
  ts: number;
}

// sigilgraph.org's q-flux vhost only proxies bare /v1/* to the real sigil-api
// backend (confirmed live 2026-08-21 fixing the wallet page's balance call —
// /api/v1/* silently falls through to static-file serving there). The local
// sigil-top :9800 server is the opposite. Same context-aware prefix pattern.
const API_BASE = /(^|\.)sigilgraph\.org$/.test(window.location.hostname) ? '/v1' : '/api/v1';

const COLORS = {
  bg: '#0a0a0f',
  panel: '#1a1428',
  accent: '#8b5cf6',
  accentBright: '#c084fc',
  gold: '#fbbf24',
  text: '#e2e8f0',
  blue: '#38bdf8',      // GHOSTDAG "blue" — accepted into the k-cluster
  red: '#f87171',       // GHOSTDAG "red" — anticone-excluded, not on the spine
};

const HEIGHT_SPACING = 3.2;   // z distance between consecutive heights (time depth)
const LANE_SPREAD = 2.4;      // x/y fan-out radius for parallel blocks at one height
const MAX_HEIGHTS_SHOWN = 40; // depth window — keeps the scene legible + fast

// Deterministic per-hash lane position within a height (stable across
// refetches so a block doesn't jump around as new siblings arrive).
function laneOffset(hash: string, siblingCount: number, indexInHeight: number): [number, number] {
  if (siblingCount <= 1) return [0, 0];
  const angle = (indexInHeight / siblingCount) * Math.PI * 2;
  // hash-derived jitter so the fan doesn't look perfectly mechanical
  let h = 0;
  for (let i = 0; i < 8 && i < hash.length; i++) h = (h * 31 + hash.charCodeAt(i)) >>> 0;
  const jitter = ((h % 100) / 100 - 0.5) * 0.4;
  const r = LANE_SPREAD * (0.6 + Math.abs(jitter));
  return [Math.cos(angle) * r, Math.sin(angle) * r];
}

// One block vertex — a glowing sphere, brighter/larger with higher blue_score.
function BlockVertex({
  pos, isBlue, blueScore, maxBlueScore, hash, onHover,
}: {
  pos: [number, number, number];
  isBlue: boolean;
  blueScore: number;
  maxBlueScore: number;
  hash: string;
  onHover: (hash: string | null) => void;
}) {
  const meshRef = useRef<THREE.Mesh>(null);
  const color = isBlue ? COLORS.blue : COLORS.red;
  const weight = maxBlueScore > 0 ? blueScore / maxBlueScore : 0.5;
  const size = 0.14 + weight * 0.16;

  useFrame((state) => {
    if (!meshRef.current) return;
    const t = state.clock.getElapsedTime();
    // gentle breathing — consensus "tension" made visible, not decorative noise
    const pulse = 1 + Math.sin(t * 1.6 + pos[2]) * (isBlue ? 0.05 : 0.12);
    meshRef.current.scale.setScalar(pulse);
  });

  return (
    <mesh
      ref={meshRef}
      position={pos}
      onPointerOver={(e) => { e.stopPropagation(); onHover(hash); }}
      onPointerOut={() => onHover(null)}
    >
      <sphereGeometry args={[size, 20, 20]} />
      <meshStandardMaterial color={color} emissive={color} emissiveIntensity={isBlue ? 0.9 : 0.5} />
    </mesh>
  );
}

// A causal edge (parent → child), tapered like a forward light cone: drawn
// as a short cone mesh instead of a plain line — wide at the causal past
// (parent), narrow at the causal future (child), oriented along the edge.
function CausalEdge({ from, to, isBlue }: { from: [number, number, number]; to: [number, number, number]; isBlue: boolean }) {
  const ref = useRef<THREE.Mesh>(null);
  const { mid, length, quat } = useMemo(() => {
    const a = new THREE.Vector3(...from);
    const b = new THREE.Vector3(...to);
    const dir = new THREE.Vector3().subVectors(b, a);
    const len = dir.length();
    const midpoint = new THREE.Vector3().addVectors(a, b).multiplyScalar(0.5);
    const q = new THREE.Quaternion().setFromUnitVectors(new THREE.Vector3(0, 1, 0), dir.clone().normalize());
    return { mid: midpoint, length: len, quat: q };
  }, [from, to]);

  return (
    <mesh ref={ref} position={mid} quaternion={quat}>
      {/* radiusTop small (child/future end), radiusBottom wide (parent/past
          end) — narrowing forward-in-time cone, per the header comment. */}
      <coneGeometry args={[0.05, length, 8, 1, true]} />
      <meshBasicMaterial
        color={isBlue ? COLORS.blue : COLORS.accent}
        transparent
        opacity={isBlue ? 0.22 : 0.14}
        side={THREE.DoubleSide}
        depthWrite={false}
      />
    </mesh>
  );
}

function Scene({ snapshot, onHover }: { snapshot: DagSnapshot; onHover: (h: string | null) => void }) {
  const byHash = useMemo(() => {
    const m = new Map<string, BlockSummary>();
    for (const b of snapshot.blocks) m.set(b.hash, b);
    return m;
  }, [snapshot]);

  const maxHeight = useMemo(
    () => snapshot.blocks.reduce((m, b) => Math.max(m, b.height), 0),
    [snapshot]
  );
  const minHeightShown = Math.max(0, maxHeight - MAX_HEIGHTS_SHOWN);
  const maxBlueScore = useMemo(
    () => snapshot.blocks.reduce((m, b) => Math.max(m, b.blue_score), 0),
    [snapshot]
  );

  const byHeight = useMemo(() => {
    const m = new Map<number, BlockSummary[]>();
    for (const b of snapshot.blocks) {
      if (b.height < minHeightShown) continue;
      (m.get(b.height) ?? m.set(b.height, []).get(b.height)!).push(b);
    }
    for (const arr of m.values()) arr.sort((a, b) => a.hash.localeCompare(b.hash));
    return m;
  }, [snapshot, minHeightShown]);

  const posOf = useCallback(
    (b: BlockSummary): [number, number, number] => {
      const siblings = byHeight.get(b.height) ?? [b];
      const idx = siblings.findIndex((s) => s.hash === b.hash);
      const [x, y] = laneOffset(b.hash, siblings.length, Math.max(0, idx));
      const z = -(maxHeight - b.height) * HEIGHT_SPACING;
      return [x, y, z];
    },
    [byHeight, maxHeight]
  );

  return (
    <>
      <ambientLight intensity={0.35} />
      <pointLight position={[10, 10, 4]} intensity={0.9} color={COLORS.accentBright} />
      <pointLight position={[-10, -6, -20]} intensity={0.6} color={COLORS.blue} />
      <fog attach="fog" args={[COLORS.bg, 10, 90]} />

      {snapshot.blocks
        .filter((b) => b.height >= minHeightShown)
        .map((b) => {
          const pos = posOf(b);
          return (
            <BlockVertex
              key={b.hash}
              pos={pos}
              isBlue={b.is_blue}
              blueScore={b.blue_score}
              maxBlueScore={maxBlueScore}
              hash={b.hash}
              onHover={onHover}
            />
          );
        })}

      {snapshot.blocks
        .filter((b) => b.height >= minHeightShown)
        .flatMap((b) => {
          const to = posOf(b);
          const edges: React.ReactNode[] = [];
          const parents = [b.parent, ...b.merge_parents].filter((p, i, arr) => p && arr.indexOf(p) === i);
          for (const p of parents) {
            const parentBlock = byHash.get(p);
            if (!parentBlock || parentBlock.height < minHeightShown) continue;
            const from = posOf(parentBlock);
            edges.push(<CausalEdge key={`${p}-${b.hash}`} from={from} to={to} isBlue={b.is_blue} />);
          }
          return edges;
        })}

      {/* height-axis time labels, every ~5 rows */}
      {Array.from(byHeight.keys())
        .filter((h) => h % 5 === 0)
        .map((h) => (
          <Text
            key={`h-${h}`}
            position={[LANE_SPREAD + 1.2, 0, -(maxHeight - h) * HEIGHT_SPACING]}
            fontSize={0.32}
            color={COLORS.text}
            anchorX="left"
            font={undefined}
          >
            {`H ${h}`}
          </Text>
        ))}

      <OrbitControls enableDamping dampingFactor={0.06} minDistance={4} maxDistance={140} />
    </>
  );
}

export default function DagKnightManifold() {
  const [snapshot, setSnapshot] = useState<DagSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const [staleSecs, setStaleSecs] = useState(0);

  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const r = await fetch(`${API_BASE}/dagknight/recent`, { cache: 'no-store' });
        const j: ApiEnvelope<DagSnapshot> = await r.json();
        if (cancelled) return;
        if (j.ok && j.data) { setSnapshot(j.data); setError(null); }
        else setError(j.error || 'no data');
      } catch (e) {
        if (!cancelled) setError('offline — cannot reach sigil-api');
      }
    }
    poll();
    const id = setInterval(poll, 5000);
    return () => { cancelled = true; clearInterval(id); };
  }, []);

  useEffect(() => {
    const id = setInterval(() => {
      if (snapshot?.updated_at_ms) setStaleSecs(Math.round((Date.now() - snapshot.updated_at_ms) / 1000));
    }, 1000);
    return () => clearInterval(id);
  }, [snapshot]);

  const blueCount = snapshot?.blocks.filter((b) => b.is_blue).length ?? 0;
  const redCount = (snapshot?.blocks.length ?? 0) - blueCount;
  const hoveredBlock = snapshot?.blocks.find((b) => b.hash === hovered) ?? null;

  return (
    <div style={{ position: 'fixed', inset: 0, background: COLORS.bg, color: COLORS.text, fontFamily: "'JetBrains Mono', monospace", overflow: 'hidden' }}>
      <Canvas camera={{ position: [6, 5, 10], fov: 55 }}>
        <color attach="background" args={[COLORS.bg]} />
        {snapshot && snapshot.blocks.length > 0 ? (
          <Scene snapshot={snapshot} onHover={setHovered} />
        ) : null}
      </Canvas>

      {/* HUD overlay */}
      <div style={{ position: 'absolute', top: 20, left: 20, maxWidth: 420, pointerEvents: 'none' }}>
        <div style={{ fontSize: 22, fontWeight: 700, color: COLORS.accentBright, letterSpacing: 0.5 }}>
          𝒢 — the SIGIL BlockDAG braid
        </div>
        <div style={{ fontSize: 12.5, color: COLORS.text, opacity: 0.75, marginTop: 6, lineHeight: 1.5 }}>
          Rendered as a Lorentzian 4-manifold: vertices are blocks, edges are causal
          links. Two axes fan out parallel candidates at one height; depth is time —
          newest nearest you. Color is the real GHOSTDAG verdict, not decoration:{' '}
          <span style={{ color: COLORS.blue }}>blue</span> = accepted into the k-cluster,{' '}
          <span style={{ color: COLORS.red }}>red</span> = excluded (still causally
          real, just not on the winning spine).
        </div>
        <div style={{ marginTop: 14, fontSize: 12, opacity: 0.9 }}>
          <div>blocks shown: <b style={{ color: COLORS.gold }}>{snapshot?.blocks.length ?? 0}</b></div>
          <div><span style={{ color: COLORS.blue }}>● blue</span> {blueCount} &nbsp; <span style={{ color: COLORS.red }}>● red</span> {redCount}</div>
          <div>k (cluster bound): <b>{snapshot?.k ?? '—'}</b></div>
          <div style={{ opacity: 0.6 }}>
            {error ? <span style={{ color: COLORS.red }}>⚠ {error}</span> : `live — updated ${staleSecs}s ago`}
          </div>
        </div>
      </div>

      {hoveredBlock && (
        <div style={{
          position: 'absolute', bottom: 20, left: 20, background: COLORS.panel,
          border: `1px solid ${hoveredBlock.is_blue ? COLORS.blue : COLORS.red}`,
          borderRadius: 8, padding: '10px 14px', fontSize: 11.5, minWidth: 320,
        }}>
          <div style={{ color: hoveredBlock.is_blue ? COLORS.blue : COLORS.red, fontWeight: 700 }}>
            {hoveredBlock.is_blue ? '● BLUE' : '● RED'} — height {hoveredBlock.height}
          </div>
          <div style={{ opacity: 0.8, marginTop: 4 }}>hash&nbsp; {hoveredBlock.hash.slice(0, 16)}…</div>
          <div style={{ opacity: 0.8 }}>parent {hoveredBlock.parent.slice(0, 16)}…</div>
          {hoveredBlock.merge_parents.length > 0 && (
            <div style={{ opacity: 0.8 }}>+{hoveredBlock.merge_parents.length} merge parent(s)</div>
          )}
          <div style={{ opacity: 0.8 }}>blue_score {hoveredBlock.blue_score}</div>
        </div>
      )}

      {!snapshot && !error && (
        <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center', fontSize: 14, opacity: 0.6 }}>
          connecting to the braid…
        </div>
      )}
      {snapshot && snapshot.blocks.length === 0 && (
        <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center', fontSize: 14, opacity: 0.6, textAlign: 'center' }}>
          no snapshot yet — dag_mode may be off on this producer,<br />or the 5s tick hasn't fired since restart
        </div>
      )}
    </div>
  );
}
