// SIGIL state store (Phase B / C migration to zustand)
//
// Owns the dashboard snapshot + balance cache that the apiShim writes to.
// Components subscribe via the hook; no prop drilling, no context boilerplate.
//
// Why zustand here:
//   - The Quillon wallet was using React context + useState for a lot of
//     cross-component state (wallet info, network status, mempool, etc).
//     That works but each provider is its own file + own re-render footgun.
//   - zustand stores are tiny (~1 KB), selector-based re-renders, no provider
//     tree, and easy to consume from non-React code (the apiShim, which
//     runs before any component mounts).
//
// The bridge:
//   apiShim writes status / balances → useSigilStore.getState().setXxx(...)
//   components read via useSigilStore(s => s.status) etc.

import { create } from 'zustand'

export type SigilStatus = {
  network_id: string
  height: number
  peers: number
  symbol: string
  version: string
  note: string
  block_time_ms: number
}

export type SigilBalance = {
  address: string
  balance_sgl: string
  balance_raw: string
  tx_count: number
  note?: string | null
}

export type SigilBlock = {
  height: number
  hash: string
  parent_hash: string
  timestamp_ms: number
  tx_count: number
  miner: string
  state_roots: {
    wallet: string
    dex: string
    event_log: string
    contract: string
  }
}

export type ActivityEvent = {
  id: string
  type: 'tx' | 'block' | 'snapshot' | 'proof' | 'swap' | 'bridge' | 'mine'
  title: string
  detail: string
  hash?: string
  height?: number
  timestamp_ms: number
  snapshot_url?: string  // flux vision shot URL
}

type SigilState = {
  // ── snapshot ──
  status: SigilStatus | null
  recent_blocks: SigilBlock[]
  balances: Record<string, SigilBalance>
  default_balance: SigilBalance | null
  snapshot_fetched_at: number
  // ── runtime ──
  shim_installed: boolean
  intercepts_total: number
  last_path: string | null
  // ── v0.6: Activity + Flux Vision Shot ──
  activity_feed: ActivityEvent[]
  explorer_tab: 'blocks' | 'txs' | 'snapshots' | 'proofs'
  vision_snapshots: { url: string; label: string; taken_at: number }[]
  last_vision_shot_at: number
  // ── actions ──
  setStatus: (s: SigilStatus) => void
  setBlocks: (b: SigilBlock[]) => void
  setBalances: (b: Record<string, SigilBalance>, def: SigilBalance | null) => void
  notePath: (p: string) => void
  markShimInstalled: () => void
  // v0.6 actions
  pushActivity: (ev: ActivityEvent) => void
  setExplorerTab: (tab: 'blocks' | 'txs' | 'snapshots' | 'proofs') => void
  addVisionSnapshot: (url: string, label: string) => void
  clearActivity: () => void
}

export const useSigilStore = create<SigilState>((set) => ({
  status: null,
  recent_blocks: [],
  balances: {},
  default_balance: null,
  snapshot_fetched_at: 0,
  shim_installed: false,
  intercepts_total: 0,
  last_path: null,
  // v0.6: Activity + Flux Vision Shot defaults
  activity_feed: [],
  explorer_tab: 'blocks',
  vision_snapshots: [],
  last_vision_shot_at: 0,

  setStatus: (s) => set({ status: s, snapshot_fetched_at: Date.now() }),
  setBlocks: (b) => set({ recent_blocks: b }),
  setBalances: (b, def) => set({ balances: b, default_balance: def }),
  notePath: (p) => set((st) => ({ intercepts_total: st.intercepts_total + 1, last_path: p })),
  markShimInstalled: () => set({ shim_installed: true }),
  // v0.6 actions
  pushActivity: (ev) => set((st) => ({
    activity_feed: [ev, ...st.activity_feed].slice(0, 100),
  })),
  setExplorerTab: (tab) => set({ explorer_tab: tab }),
  addVisionSnapshot: (url, label) => set((st) => ({
    vision_snapshots: [{ url, label, taken_at: Date.now() }, ...st.vision_snapshots].slice(0, 50),
    last_vision_shot_at: Date.now(),
  })),
  clearActivity: () => set({ activity_feed: [] }),
}))

// Convenience selectors (avoid inline selector identity churn)
export const selectStatus = (s: SigilState) => s.status
export const selectBlocks = (s: SigilState) => s.recent_blocks
export const selectShimAlive = (s: SigilState) => s.shim_installed
export const selectIntercepts = (s: SigilState) => s.intercepts_total
// v0.6 selectors
export const selectActivityFeed = (s: SigilState) => s.activity_feed
export const selectExplorerTab = (s: SigilState) => s.explorer_tab
export const selectVisionSnapshots = (s: SigilState) => s.vision_snapshots
export const selectLastVisionShot = (s: SigilState) => s.last_vision_shot_at
