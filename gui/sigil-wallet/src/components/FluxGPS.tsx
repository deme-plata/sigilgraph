/**
 * FluxGPS.tsx — Flux Global Positioning System
 *
 * Renders the Flux P2P gossipsub mesh as a 3D GPS starfield constellation.
 * Each peer is a star with a pulsating glow; connections between peers are
 * constellation lines with animated data pulses flowing along them.
 *
 * The "GPS" metaphor: just as GPS satellites triangulate position on Earth,
 * Flux GPS triangulates your wallet's position in the P2P mesh — showing
 * which peers are near, which are far, and where data flows.
 *
 * Integrates with:
 *   - LibP2PContext for live peer list
 *   - useP2PData hook for mesh topology
 *   - Three.js / react-three-fiber for 3D rendering
 */
import { useRef, useMemo, useEffect, useState } from 'react';
import { useFrame } from '@react-three/fiber';
import { Sphere, Line, Text } from '@react-three/drei';
import * as THREE from 'three';

// ─── Types ──────────────────────────────────────────────────────────────────

export interface GPSSatellite {
  id: string;
  name: string;
  /** Peer ID for identification */
  peerId: string;
  /** Role in the mesh: bootstrap, validator, miner, relay */
  role: 'bootstrap' | 'validator' | 'miner' | 'relay';
  /** Normalized position in 3D space (unit sphere) */
  theta: number;
  phi: number;
  /** Signal strength 0..1 — affects star brightness */
  signal: number;
  /** Latency in ms */
  latencyMs: number;
  /** Whether this peer is currently connected */
  connected: boolean;
}

export interface GPSConstellation {
  /** Satellites (peers) in view */
  satellites: GPSSatellite[];
  /** Connections between satellites */
  links: { source: number; target: number; bandwidth: number }[];
}

interface FluxGPSProps {
  constellation: GPSConstellation;
  /** Auto-rotate the constellation */
  autoRotate?: boolean;
  /** Show orbital rings */
  showOrbits?: boolean;
}

// ─── Colors ─────────────────────────────────────────────────────────────────

const ROLE_COLORS: Record<GPSSatellite['role'], string> = {
  bootstrap: '#7c3aed', // violet-600
  validator: '#8b5cf6', // violet-500
  miner:     '#d4af37', // gold
  relay:     '#f59e0b', // amber-500
};

const ROLE_GLOW: Record<GPSSatellite['role'], string> = {
  bootstrap: '#a78bfa',
  validator: '#a78bfa',
  miner:     '#fbbf24',
  relay:     '#fbbf24',
};

// ─── Star (Peer) Component ─────────────────────────────────────────────────

function SatelliteStar({ satellite, index }: { satellite: GPSSatellite; index: number }) {
  const meshRef = useRef<THREE.Mesh>(null);
  const glowRef = useRef<THREE.Sprite>(null);

  // Position on unit sphere
  const pos = useMemo(() => {
    const t = satellite.theta;
    const p = satellite.phi;
    return new THREE.Vector3(
      Math.sin(p) * Math.cos(t),
      Math.cos(p),
      Math.sin(p) * Math.sin(t),
    );
  }, [satellite.theta, satellite.phi]);

  const color = ROLE_COLORS[satellite.role];
  const glow = ROLE_GLOW[satellite.role];
  const size = 0.04 + satellite.signal * 0.06;
  const glowSize = size * 3;

  useFrame((state) => {
    if (!meshRef.current || !glowRef.current) return;
    const t = state.clock.getElapsedTime();
    const pulse = 1 + Math.sin(t * 1.5 + index * 0.7) * 0.15 * satellite.signal;
    meshRef.current.scale.setScalar(pulse);
    // Glow pulse
    const gp = 1 + Math.sin(t * 0.8 + index * 0.3) * 0.2;
    glowRef.current.scale.setScalar(gp * satellite.signal);
    // Opacity oscillation for disconnected peers
    if (!satellite.connected) {
      const blink = Math.sin(t * 3 + index) * 0.3 + 0.3;
      (glowRef.current.material as THREE.SpriteMaterial).opacity = blink;
    } else {
      (glowRef.current.material as THREE.SpriteMaterial).opacity = 0.6;
    }
  });

  return (
    <group position={pos}>
      {/* Glow halo */}
      <sprite ref={glowRef} scale={[glowSize, glowSize, 1]}>
        <spriteMaterial
          attach="material"
          color={glow}
          transparent
          opacity={0.4}
          blending={THREE.AdditiveBlending}
          depthWrite={false}
        />
      </sprite>
      {/* Core star */}
      <mesh ref={meshRef}>
        <sphereGeometry args={[size, 12, 12]} />
        <meshBasicMaterial color={color} />
      </mesh>
      {/* Peer label */}
      <Text
        position={[0, size * 2.5, 0]}
        fontSize={0.03}
        color={color}
        outlineWidth={0.002}
        outlineColor="#000000"
      >
        {satellite.name}
      </Text>
    </group>
  );
}

// ─── Constellation Link ─────────────────────────────────────────────────────

function ConstellationLink({
  start,
  end,
  bandwidth,
  autoRotate,
}: {
  start: THREE.Vector3;
  end: THREE.Vector3;
  bandwidth: number;
  autoRotate: boolean;
}) {
  const lineRef = useRef<THREE.Mesh>(null);
  const pulseRef = useRef<{ t: number }>({ t: Math.random() * 100 });

  // Arc between points on unit sphere
  const points = useMemo(() => {
    const mid = new THREE.Vector3().addVectors(start, end).normalize();
    // Slight arc outward
    const arcFactor = 0.15;
    const arcMid = new THREE.Vector3()
      .copy(mid)
      .multiplyScalar(1 + arcFactor);
    const curve = new THREE.QuadraticBezierCurve3(start, arcMid, end);
    return curve.getPoints(20);
  }, [start, end]);

  const thickness = 0.002 + bandwidth * 0.008;

  useFrame((state) => {
    if (!lineRef.current) return;
    pulseRef.current.t += 0.02;
    const p = (Math.sin(pulseRef.current.t * 2) + 1) / 2; // 0..1
    // Position a glow dot along the curve
    const idx = Math.floor(p * (points.length - 1));
    const pt = points[idx];
    if (pt && lineRef.current) {
      lineRef.current.position.copy(pt);
    }
  });

  return (
    <>
      {/* Arc line */}
      <Line
        points={points}
        color={`hsl(270, 80%, ${50 + bandwidth * 30}%)`}
        lineWidth={1}
        transparent
        opacity={0.3 + bandwidth * 0.4}
      />
      {/* Data pulse */}
      <mesh ref={lineRef}>
        <sphereGeometry args={[thickness * 2, 6, 6]} />
        <meshBasicMaterial
          color="#a78bfa"
          transparent
          opacity={0.8}
          blending={THREE.AdditiveBlending}
        />
      </mesh>
    </>
  );
}

// ─── Orbital Ring ──────────────────────────────────────────────────────────

function OrbitalRing({ radius, tilt }: { radius: number; tilt: number }) {
  const points = useMemo(() => {
    const pts: THREE.Vector3[] = [];
    const segments = 48;
    for (let i = 0; i <= segments; i++) {
      const theta = (i / segments) * Math.PI * 2;
      pts.push(
        new THREE.Vector3(
          Math.cos(theta) * radius,
          Math.sin(theta) * radius * Math.sin(tilt),
          Math.sin(theta) * radius * Math.cos(tilt),
        ),
      );
    }
    return pts;
  }, [radius, tilt]);

  return (
    <Line
      points={points}
      color="#4a4a6a"
      lineWidth={1}
      transparent
      opacity={0.15}
    />
  );
}

// ─── Main FluxGPS Component ────────────────────────────────────────────────

export default function FluxGPS({
  constellation,
  autoRotate = true,
  showOrbits = true,
}: FluxGPSProps) {
  const groupRef = useRef<THREE.Group>(null);

  useFrame((state) => {
    if (!groupRef.current || !autoRotate) return;
    // Slow rotation — like a GPS satellite sweep
    groupRef.current.rotation.y = state.clock.getElapsedTime() * 0.05;
    groupRef.current.rotation.x =
      Math.sin(state.clock.getElapsedTime() * 0.02) * 0.05;
  });

  const starPositions = useMemo(
    () =>
      constellation.satellites.map((sat) => {
        const t = sat.theta;
        const p = sat.phi;
        return new THREE.Vector3(
          Math.sin(p) * Math.cos(t),
          Math.cos(p),
          Math.sin(p) * Math.sin(t),
        );
      }),
    [constellation.satellites],
  );

  return (
    <group ref={groupRef}>
      {/* Orbital rings — GPS constellation layers */}
      {showOrbits && (
        <>
          <OrbitalRing radius={0.6} tilt={0} />
          <OrbitalRing radius={0.85} tilt={0.3} />
          <OrbitalRing radius={1.1} tilt={-0.2} />
        </>
      )}

      {/* Constellation links */}
      {constellation.links.map((link, i) => (
        <ConstellationLink
          key={`link-${i}`}
          start={starPositions[link.source]}
          end={starPositions[link.target]}
          bandwidth={link.bandwidth}
          autoRotate={autoRotate}
        />
      ))}

      {/* Satellite stars */}
      {constellation.satellites.map((sat, i) => (
        <SatelliteStar key={sat.id} satellite={sat} index={i} />
      ))}

      {/* Center — your wallet position */}
      <mesh>
        <sphereGeometry args={[0.025, 16, 16]} />
        <meshBasicMaterial color="#d4af37" />
      </mesh>
    </group>
  );
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/**
 * Generate a demo constellation from a list of peer names.
 * In production, this comes from the live LibP2PContext.
 */
export function generateDemoConstellation(
  peers: { id: string; name: string; role: GPSSatellite['role'] }[],
): GPSConstellation {
  const satellites: GPSSatellite[] = peers.map((p, i) => {
    const offset = (2 * Math.PI * i) / peers.length;
    return {
      id: p.id,
      name: p.name,
      peerId: p.id,
      role: p.role,
      theta: offset + Math.random() * 0.3,
      phi: (Math.random() - 0.5) * Math.PI * 0.7,
      signal: 0.5 + Math.random() * 0.5,
      latencyMs: Math.floor(10 + Math.random() * 200),
      connected: Math.random() > 0.2,
    };
  });

  const links: { source: number; target: number; bandwidth: number }[] = [];
  // Connect nearby peers
  for (let i = 0; i < satellites.length; i++) {
    for (let j = i + 1; j < satellites.length; j++) {
      if (Math.random() > 0.6) continue; // sparse connections
      links.push({
        source: i,
        target: j,
        bandwidth: 0.2 + Math.random() * 0.8,
      });
    }
  }

  return { satellites, links };
}
