/**
 * StarshipBackground.tsx — 3D Starship Cockpit with Flux GPS
 *
 * A react-three-fiber scene that renders:
 *   1. A starship cockpit frame (HUD overlay in 3D space)
 *   2. Starfield tunnel effect (hyperspace travel)
 *   3. The Flux GPS constellation showing peer positions
 *   4. Holographic data panels with real-time network stats
 *
 * Replaces the canvas-based QuantumFieldBackground on the LoginScreen
 * with a true 3D WebGL scene.
 */
import { useRef, useMemo, useEffect, useState } from 'react';
import { Canvas, useFrame, useThree } from '@react-three/fiber';
import { OrbitControls, Text } from '@react-three/drei';
import * as THREE from 'three';
import FluxGPS, { generateDemoConstellation } from './FluxGPS';

// ─── Starfield Tunnel (Hyperspace Effect) ───────────────────────────────────

function Starfield({ count = 1500, speed = 0.3 }) {
  const points = useMemo(() => {
    const positions = new Float32Array(count * 3);
    for (let i = 0; i < count; i++) {
      const theta = Math.random() * Math.PI * 2;
      const phi = Math.acos(2 * Math.random() - 1);
      const r = 2 + Math.random() * 8; // depth 2..10
      positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
      positions[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
      positions[i * 3 + 2] = r * Math.cos(phi) - 5; // push back
    }
    return positions;
  }, [count]);

  const colors = useMemo(() => {
    const cols = new Float32Array(count * 3);
    for (let i = 0; i < count; i++) {
      // Violet/blue/white color palette
      const shade = 0.4 + Math.random() * 0.6;
      if (Math.random() > 0.7) {
        // Violet star
        cols[i * 3] = shade * 0.8;
        cols[i * 3 + 1] = shade * 0.4;
        cols[i * 3 + 2] = shade;
      } else if (Math.random() > 0.5) {
        // Blue star
        cols[i * 3] = shade * 0.3;
        cols[i * 3 + 1] = shade * 0.5;
        cols[i * 3 + 2] = shade;
      } else {
        // White star
        cols[i * 3] = shade;
        cols[i * 3 + 1] = shade;
        cols[i * 3 + 2] = shade;
      }
    }
    return cols;
  }, [count]);

  const pointsRef = useRef<THREE.Points>(null);

  useFrame((state) => {
    if (!pointsRef.current) return;
    const positions = pointsRef.current.geometry.attributes.position
      .array as Float32Array;
    const delta = state.clock.getDelta();
    const moveSpeed = speed * delta * 2;

    for (let i = 0; i < count; i++) {
      // Move stars toward camera (Z axis)
      positions[i * 3 + 2] += moveSpeed;

      // Reset stars that pass behind camera
      if (positions[i * 3 + 2] > 7) {
        const theta = Math.random() * Math.PI * 2;
        const phi = Math.acos(2 * Math.random() - 1);
        const r = 2 + Math.random() * 8;
        positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
        positions[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
        positions[i * 3 + 2] = -12;
      }
    }
    pointsRef.current.geometry.attributes.position.needsUpdate = true;
  });

  return (
    <points ref={pointsRef}>
      <bufferGeometry>
        <bufferAttribute args={[points, 3]} attach="attributes-position" />
        <bufferAttribute args={[colors, 3]} attach="attributes-color" />
      </bufferGeometry>
      <pointsMaterial
        size={0.02}
        vertexColors
        transparent
        opacity={0.8}
        blending={THREE.AdditiveBlending}
        depthWrite={false}
        sizeAttenuation
      />
    </points>
  );
}

// ─── Cockpit Frame ─────────────────────────────────────────────────────────

function CockpitFrame() {
  return (
    <group>
      {/* Holographic ring */}
      <mesh rotation={[Math.PI / 2, 0, 0]} position={[0, 0, -1.5]}>
        <ringGeometry args={[1.8, 2.0, 64]} />
        <meshBasicMaterial
          color="#7c3aed"
          transparent
          opacity={0.08}
          side={THREE.DoubleSide}
          depthWrite={false}
        />
      </mesh>
      <mesh rotation={[Math.PI / 2, 0, 0]} position={[0, 0, -1.5]}>
        <ringGeometry args={[1.3, 1.35, 64]} />
        <meshBasicMaterial
          color="#8b5cf6"
          transparent
          opacity={0.05}
          side={THREE.DoubleSide}
          depthWrite={false}
        />
      </mesh>

      {/* Corner brackets — cockpit HUD */}
      {[
        [-1.7, 1.0],
        [1.7, 1.0],
        [-1.7, -1.0],
        [1.7, -1.0],
      ].map(([x, y], i) => (
        <group key={i} position={[x, y, -1.8]}>
          <lineSegments>
            <bufferGeometry>
              <bufferAttribute
                attach="attributes-position"
                args={[
                  new Float32Array([
                    0, 0, 0, 0.12 * Math.sign(x),
                    0, 0, 0, 0, 0, 0.12 * Math.sign(y),
                  ]),
                  3,
                ]}
              />
            </bufferGeometry>
            <lineBasicMaterial color="#8b5cf6" transparent opacity={0.3} />
          </lineSegments>
        </group>
      ))}
    </group>
  );
}

// ─── Holographic HUD Text ──────────────────────────────────────────────────

function HUDText({
  position,
  text,
  color = '#8b5cf6',
  size = 0.06,
}: {
  position: [number, number, number];
  text: string;
  color?: string;
  size?: number;
}) {
  return (
    <Text
      position={position}
      fontSize={size}
      color={color}
      font="/fonts/SpaceGrotesk-Medium.ttf"
      outlineWidth={0.003}
      outlineColor="#000000"
    >
      {text}
    </Text>
  );
}

function HUDOverlay({ height, peers, bps }: { height: number; peers: number; bps: number }) {
  return (
    <group position={[0, 0, -2]}>
      {/* Top-left: Network status */}
      <HUDText position={[-1.9, 1.3, 0]} text={`SIGIL • sigil-g0`} size={0.07} color="#d4af37" />
      <HUDText position={[-1.9, 1.15, 0]} text={`HEIGHT: ${height.toLocaleString()}`} size={0.045} />
      <HUDText position={[-1.9, 1.05, 0]} text={`PEERS: ${peers}`} size={0.045} />
      <HUDText position={[-1.9, 0.95, 0]} text={`BLOCKS/S: ${bps.toFixed(1)}`} size={0.045} />

      {/* Bottom-right: System status */}
      {/* Right-aligned via negative x offset for the full width */}
      <HUDText position={[1.0, -1.3, 0]} text={`FLUX GPS • ONLINE`} size={0.05} color="#22c55e" />
      <HUDText position={[1.0, -1.4, 0]} text={`CONSTELLATION: ${peers} SATELLITES`} size={0.035} color="#a78bfa" />
    </group>
  );
}

// ─── Scene Camera Controller ──────────────────────────────────────────────

function CameraController() {
  const { camera } = useThree();

  useEffect(() => {
    camera.position.set(0, 0.2, 2.5);
    camera.lookAt(0, 0, -1);
  }, [camera]);

  useFrame((state) => {
    // Gentle drift
    camera.position.y = 0.2 + Math.sin(state.clock.getElapsedTime() * 0.1) * 0.05;
    camera.lookAt(0, 0, -1);
  });

  return null;
}

// ─── Props ─────────────────────────────────────────────────────────────────

interface StarshipBackgroundProps {
  height?: number;
  peers?: number;
  blocksPerSec?: number;
  /** Optional peer list for GPS constellation. Auto-generates demo if empty. */
  peersList?: { id: string; name: string; role: 'bootstrap' | 'validator' | 'miner' | 'relay' }[];
}

// ─── Main Component ────────────────────────────────────────────────────────

export default function StarshipBackground({
  height = 108288,
  peers = 4,
  blocksPerSec = 19,
  peersList,
}: StarshipBackgroundProps) {
  const constellation = useMemo(() => {
    const list = peersList ?? [
      { id: 'eps-1', name: 'Epsilon', role: 'bootstrap' as const },
      { id: 'dlt-1', name: 'Delta', role: 'validator' as const },
      { id: 'gmm-1', name: 'Gamma', role: 'miner' as const },
      { id: 'bet-1', name: 'Beta', role: 'validator' as const },
      { id: 'alp-1', name: 'Alpha', role: 'relay' as const },
      { id: 'zet-1', name: 'Zeta', role: 'miner' as const },
    ];
    return generateDemoConstellation(list);
  }, [peersList]);

  return (
    <div className="starship-background" style={{ position: 'fixed', inset: 0, zIndex: 0 }}>
      <Canvas
        dpr={[1, 2]}
        gl={{
          antialias: true,
          alpha: true,
          powerPreference: 'high-performance',
        }}
        style={{ background: 'transparent' }}
      >
        {/* Deep space fog */}
        <fog attach="fog" args={['#0a0a1a', 5, 15]} />

        {/* Starfield tunnel */}
        <Starfield count={2000} speed={0.25} />

        {/* Starship cockpit frame */}
        <CockpitFrame />

        {/* Flux GPS constellation */}
        <FluxGPS
          constellation={constellation}
          autoRotate={true}
          showOrbits={true}
        />

        {/* HUD overlay */}
        <HUDOverlay height={height} peers={peers} bps={blocksPerSec} />

        {/* Camera */}
        <CameraController />
      </Canvas>
    </div>
  );
}

// ─── CSS — injected via App.css or index.css ──────────────────────────────
// .starship-background {
//   position: fixed;
//   inset: 0;
//   z-index: 0;
//   pointer-events: none;
// }
// .starship-background canvas {
//   pointer-events: none;
// }
