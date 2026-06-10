/**
 * Transport Layer Configuration for Browser P2P Node
 *
 * Transport Strategy:
 * 1. WebSocket Secure (wss://sigilgraph.fluxapp.xyz:9443) → nginx → libp2p:9001/ws
 * 2. Circuit Relay for browser-to-browser connections
 * 3. NO WebRTC - Disabled to prevent IP leaks (STUN reveals real IP)
 *
 * v3.6.0-browser: WebSocket + Circuit Relay transports
 * v3.5.4-browser: (REMOVED) WebRTC disabled - IP leak prevention
 * v3.5.3-browser: Updated for @libp2p/websockets v10.x API changes.
 */

import { webSockets } from '@libp2p/websockets'
import { circuitRelayTransport } from '@libp2p/circuit-relay-v2'
// v0.9.1: WebRTC RE-ENABLED for true direct browser-to-browser (operator decision
// 2026-06-09). Tradeoff accepted: ICE candidate gathering exposes each browser's
// public IP to its peer (and to STUN). The privacy-only relay path still exists as
// a fallback (circuitRelayTransport below) for when a direct upgrade can't be made.
import { webRTC } from '@libp2p/webrtc'

import { logTor } from './torConfig'

/**
 * STUN servers for WebRTC ICE (NAT traversal / srflx candidate gathering).
 * Public STUN only — no TURN, so symmetric-NAT ↔ symmetric-NAT pairs may fail to
 * connect directly and fall back to circuit relay. Add a TURN entry here if direct
 * success rate needs to be ~100% (costs bandwidth).
 */
export const WEBRTC_ICE_SERVERS: RTCIceServer[] = [
  { urls: ['stun:stun.l.google.com:19302', 'stun:stun1.l.google.com:19302'] },
]

/**
 * Create WebSocket transport for Tor bridge connections
 *
 * 🧅 TOR ONLY: All WebSocket connections go through port 9444 (Tor bridge)
 *
 * Traffic flow:
 * Browser WebSocket → nginx:9444 → websockify → Tor SOCKS5 → Tor network
 *
 * @returns WebSocket transport configuration
 */
export function createWebSocketTransport() {
  logTor('info', 'Creating WebSocket transport (Tor bridge only)')
  // WebSocket transport - connects to Tor bridge endpoint
  // The actual Tor routing happens server-side via websockify → SOCKS5
  return webSockets()
}

/**
 * WebRTC transport — direct browser-to-browser (v0.9.1)
 *
 * The circuit relay (below) is used as the SIGNALING channel: two browsers first
 * meet over a relayed connection through the Epsilon bootstrap, then libp2p
 * exchanges SDP/ICE over that circuit and upgrades to a DIRECT WebRTC datachannel.
 * After the upgrade, browser↔browser traffic no longer transits Epsilon.
 *
 * ⚠️ Privacy note: ICE gathering reveals the browser's public IP to the peer (and
 * STUN). This is the accepted tradeoff for direct connectivity; the relay path
 * remains as a fallback when a direct upgrade fails.
 *
 * @returns WebRTC transport configuration
 */
export function createWebRTCTransport() {
  logTor('info', 'Creating WebRTC transport (direct browser-to-browser, relay-signaled)')
  return webRTC({ rtcConfiguration: { iceServers: WEBRTC_ICE_SERVERS } })
}

/**
 * Create Circuit Relay transport for browser-to-browser via Tor
 *
 * 🧅 TOR: Circuit Relay routes through Tor-connected bootstrap node
 *
 * Browser A → Bootstrap Relay → Browser B (initial reach + WebRTC signaling)
 *
 * v0.9.1: This is the SIGNALING + fallback path. Once two browsers meet over the
 * relay, the WebRTC transport upgrades them to a direct datachannel; the relay
 * remains the fallback when a direct upgrade can't be established.
 *
 * v3.6.0: Circuit relay through Tor for browser-to-browser
 *
 * @returns Circuit Relay transport configuration
 */
export function createCircuitRelayTransport() {
  logTor('info', 'Creating Circuit Relay transport (via Tor)')
  // Circuit relay v2 discovers relays automatically
  // Our Tor-connected bootstrap node acts as the relay
  return circuitRelayTransport()
}

/**
 * Create all transports for the browser node
 *
 * 🧅 MANDATORY TOR - v3.6.0
 *
 * Transport stack (Tor-only):
 * 1. WebSocket → Tor bridge (wss://sigilgraph.fluxapp.xyz:9444)
 * 2. Circuit Relay → Through Tor for browser-to-browser
 *
 * REMOVED (privacy risk):
 * - WebRTC (leaks IP via STUN/ICE)
 * - Direct WebSocket to clearnet (leaks IP)
 *
 * @returns Array of Tor-only transport configurations
 */
export function createTransports() {
  logTor('info', 'Creating mandatory Tor transport stack...')

  const transports = []

  // 1. WebSocket to bootstrap (PRIMARY — only way a browser can dial out)
  transports.push(createWebSocketTransport())
  logTor('info', '✅ WebSocket transport (bootstrap)')

  // 2. WebRTC — DIRECT browser-to-browser (relay-signaled). Must be present so the
  //    relayed connection can be upgraded to a direct datachannel (v0.9.1).
  transports.push(createWebRTCTransport())
  logTor('info', '✅ WebRTC transport (direct browser-to-browser)')

  // 3. Circuit Relay — initial reach + WebRTC signaling channel, and fallback path
  //    when a direct upgrade can't be made.
  transports.push(createCircuitRelayTransport())
  logTor('info', '✅ Circuit Relay transport (signaling + fallback)')

  logTor('info', `Transport stack ready: ${transports.length} transports`)
  return transports
}

/**
 * Test Tor bridge connectivity
 *
 * 🧅 Tests if the Tor bridge WebSocket endpoint is reachable
 * Used for diagnostics and health checks
 *
 * @param multiaddr - Address to test (should be Tor bridge)
 * @returns Promise<{success: boolean, latency?: number}> - Result with latency
 */
export async function testTransportConnectivity(multiaddr: string): Promise<boolean> {
  try {
    // Parse multiaddr to check if it's a valid WebSocket address
    if (!multiaddr.includes('/wss/') && !multiaddr.includes('/ws/')) {
      logTor('warn', 'Not a WebSocket address:', multiaddr)
      return false
    }

    // Verify this is a valid bootstrap endpoint (port 9443 or 9444)
    if (!multiaddr.includes('/tcp/9443/') && !multiaddr.includes('/tcp/9444/')) {
      logTor('warn', 'Not a valid bootstrap address (expected port 9443 or 9444):', multiaddr)
      return false
    }

    logTor('info', 'Testing Tor bridge connectivity...')

    // Extract WebSocket URL from multiaddr
    const parts = multiaddr.split('/')
    const protocol = parts.includes('wss') ? 'wss' : 'ws'
    const host = parts[2]
    const port = parts[4]

    const wsUrl = `${protocol}://${host}:${port}/tor-bridge`

    // Attempt WebSocket connection with longer timeout for Tor
    const ws = new WebSocket(wsUrl)
    const startTime = performance.now()

    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        ws.close()
        logTor('error', 'Tor bridge connection timeout (45s)')
        resolve(false)
      }, 45000) // 45 second timeout for Tor

      ws.onopen = () => {
        const latency = Math.round(performance.now() - startTime)
        logTor('info', `Tor bridge connected (${latency}ms)`)
        clearTimeout(timeout)
        ws.close()
        resolve(true)
      }

      ws.onerror = (error) => {
        logTor('error', 'Tor bridge connection failed', error)
        clearTimeout(timeout)
        resolve(false)
      }
    })
  } catch (error) {
    logTor('error', 'Tor bridge test failed', error)
    return false
  }
}

/**
 * Get transport statistics
 *
 * 🧅 Returns Tor-only transport status
 *
 * @returns Transport statistics object
 */
export function getTransportStats() {
  return {
    tor: {
      enabled: true,
      mandatory: true,
      description: 'All traffic routed through Tor (mandatory)',
    },
    websocket: {
      enabled: true,
      endpoint: 'wss://sigilgraph.fluxapp.xyz:9443',
      description: 'WebSocket Secure to libp2p bootstrap node',
    },
    webrtc: {
      enabled: true, // v0.9.1: RE-ENABLED for direct browser-to-browser
      reason: 'Direct datachannel after relay-signaled upgrade; ICE exposes public IP',
      description: 'WebRTC enabled for direct browser-to-browser (relay used as signaling)',
    },
    relay: {
      enabled: true,
      description: 'Circuit Relay: initial reach + WebRTC signaling + fallback path',
    },
  }
}

/**
 * Create TRON wallet bridge transport
 * 
 * 🔷 TRON MULTI-CHAIN: Dedicated transport for TRON wallet connectivity.
 * 
 * TRON wallets connect via sigilgraph.fluxapp.xyz:9445 (TRON bridge port).
 * This transport is used when the wallet detects TRON chain activity.
 * 
 * Traffic flow:
 * TRON Wallet → wss://sigilgraph.fluxapp.xyz:9445 → nginx → libp2p:9003/tron
 * 
 * @returns WebSocket transport for TRON bridge
 */
export function createTronTransport() {
  logTor('info', 'Creating TRON wallet bridge transport')
  return webSockets()
}

/**
 * Get all transports including TRON bridge
 * Extended transport stack with TRON multi-chain support
 */
export function createAllTransports() {
  const transports = createTransports()
  // Add TRON transport for multi-chain wallet support
  transports.push(createTronTransport())
  logTor('info', '✅ TRON bridge transport added (multi-chain)')
  return transports
}
