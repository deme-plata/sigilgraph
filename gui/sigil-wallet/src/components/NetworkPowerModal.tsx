// NetworkPowerModal.tsx — SIGIL edition
// Full-screen slide-in modal showing miner list + hashrate history + radial power ring.
// Opened from the "Network power" / "Miners" badges in the global TopBar.
//
// 2026-08-25: history now comes from sigil-api's durable flux-db-backed
// mining_history store (raw 1-min samples for 48h, hourly rollups for 60d,
// daily rollups forever) instead of a 24h-only localStorage cache — the
// timeframe selector below (24h/7d/30d/1y/all) is a real server-side query,
// not a client-side illusion. The miner list is also real: SIGIL's
// MiningBridge only tracks (wallet, rig) → (rate, last-seen, cpu/gpu), so
// this deliberately shows only those fields instead of inventing
// blocks-found/rewards-earned/source columns the way a naive Quillon port
// would.

import { useState, useEffect, useRef, useCallback, useMemo } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { X, Search, TrendingUp, Users, Activity, Zap, Award, Clock, Cpu, MonitorSmartphone } from 'lucide-react';
import { qnkAPI } from '../services/api';

// ──────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────

interface NetworkPowerModalProps {
  isOpen: boolean;
  onClose: () => void;
  networkHashRate: number;
  connectedMiners: number;
}

type MinerKind = 'cpu' | 'gpu' | 'unknown';

interface ServerMiner {
  wallet: string;
  rig: string;
  hash_rate: number;
  last_seen_secs_ago: number;
  kind: MinerKind;
}

interface HashrateHistoryPoint {
  hashrate: number;
  miners: number;
  timestamp: number;
}

type SortKey = 'hash_rate' | 'last_seen_secs_ago';
type SortDir = 'asc' | 'desc';
type TimeRange = '24h' | '7d' | '30d' | '1y' | 'all';

// ──────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────

const RANGE_OPTIONS: { key: TimeRange; label: string }[] = [
  { key: '24h', label: '24H' },
  { key: '7d', label: '7D' },
  { key: '30d', label: '30D' },
  { key: '1y', label: '1Y' },
  { key: 'all', label: 'ALL' },
];

// ──────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────

function fmtHash(hps: number): string {
  if (hps >= 1e12) return `${(hps / 1e12)?.toFixed(2)} TH/s`;
  if (hps >= 1e9) return `${(hps / 1e9)?.toFixed(2)} GH/s`;
  if (hps >= 1e6) return `${(hps / 1e6)?.toFixed(2)} MH/s`;
  if (hps >= 1e3) return `${(hps / 1e3)?.toFixed(2)} kH/s`;
  return `${(hps ?? 0)?.toFixed(0)} H/s`;
}

function truncAddr(addr: string): string {
  if (addr.length <= 14) return addr;
  return `${addr.slice(0, 6)}...${addr.slice(-4)}`;
}

function secsAgo(secs: number): string {
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

const RANK_BADGE = ['text-yellow-400', 'text-gray-300', 'text-amber-600'];

function kindLabel(kind: MinerKind): string {
  if (kind === 'cpu') return 'CPU';
  if (kind === 'gpu') return 'GPU';
  return '?';
}

function kindBadgeClass(kind: MinerKind): string {
  switch (kind) {
    case 'cpu': return 'bg-quantum-cyan/20 text-quantum-cyan';
    case 'gpu': return 'bg-violet-500/20 text-violet-400';
    default: return 'bg-white/10 text-gray-500';
  }
}

// ──────────────────────────────────────────────────────────────
// Main Chart (hashrate + miners line chart)
// ──────────────────────────────────────────────────────────────

function drawMainChart(
  canvas: HTMLCanvasElement,
  data: HashrateHistoryPoint[],
  mouseX: number | null
) {
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth || 600;
  const H = canvas.clientHeight || 220;

  if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  ctx.clearRect(0, 0, W, H);

  const pl = 58, pr = 48, pt = 16, pb = 28;
  const cW = W - pl - pr;
  const cH = H - pt - pb;

  if (data.length < 2) {
    ctx.font = '11px sans-serif';
    ctx.fillStyle = 'rgba(148,163,184,0.4)';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText('Collecting hashrate history…', W / 2, H / 2);
    ctx.font = '9px sans-serif';
    ctx.fillText('Samples arrive every 60s', W / 2, H / 2 + 16);
    return;
  }

  const t0 = data[0].timestamp;
  const tN = data[data.length - 1].timestamp;
  const tRange = tN - t0 || 1;

  let maxH = 0, maxM = 0;
  for (const d of data) {
    if (d.hashrate > maxH) maxH = d.hashrate;
    if (d.miners > maxM) maxM = d.miners;
  }
  maxH = maxH * 1.15 || 1;
  maxM = Math.ceil(maxM * 1.15) || 1;

  // Grid lines
  ctx.setLineDash([2, 4]);
  ctx.strokeStyle = 'rgba(148,163,184,0.07)';
  ctx.lineWidth = 1;
  for (let i = 0; i <= 4; i++) {
    const y = pt + (i / 4) * cH;
    ctx.beginPath(); ctx.moveTo(pl, y); ctx.lineTo(pl + cW, y); ctx.stroke();
  }
  ctx.setLineDash([]);

  // Y-left labels (hashrate)
  ctx.font = '9px sans-serif';
  ctx.fillStyle = 'rgba(96,165,250,0.55)';
  ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
  for (let i = 0; i <= 4; i++) {
    ctx.fillText(fmtHash(maxH * (1 - i / 4)), pl - 5, pt + (i / 4) * cH);
  }

  // Y-right labels (miners)
  ctx.fillStyle = 'rgba(74,222,128,0.55)';
  ctx.textAlign = 'left';
  for (let i = 0; i <= 4; i++) {
    ctx.fillText(`${Math.round(maxM * (1 - i / 4))}`, pl + cW + 5, pt + (i / 4) * cH);
  }

  // X-axis labels
  ctx.fillStyle = 'rgba(148,163,184,0.35)';
  ctx.textAlign = 'center'; ctx.textBaseline = 'top';
  for (let i = 0; i < 6; i++) {
    const t = t0 + (tRange * i) / 5;
    const x = pl + (i / 5) * cW;
    ctx.fillText(new Date(t * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }), x, pt + cH + 5);
  }

  // Hashrate fill + stroke
  const hashGrad = ctx.createLinearGradient(0, pt, 0, pt + cH);
  hashGrad.addColorStop(0, 'rgba(59,130,246,0.28)');
  hashGrad.addColorStop(1, 'rgba(59,130,246,0)');

  ctx.beginPath();
  for (let i = 0; i < data.length; i++) {
    const x = pl + ((data[i].timestamp - t0) / tRange) * cW;
    const y = pt + cH - (data[i].hashrate / maxH) * cH;
    i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
  }
  ctx.strokeStyle = 'rgba(59,130,246,0.9)';
  ctx.lineWidth = 2; ctx.lineJoin = 'round'; ctx.stroke();

  const lastD = data[data.length - 1];
  const lastX = pl + ((lastD.timestamp - t0) / tRange) * cW;
  ctx.lineTo(lastX, pt + cH); ctx.lineTo(pl, pt + cH); ctx.closePath();
  ctx.fillStyle = hashGrad; ctx.fill();

  // Miner count line
  ctx.beginPath();
  for (let i = 0; i < data.length; i++) {
    const x = pl + ((data[i].timestamp - t0) / tRange) * cW;
    const y = pt + cH - (data[i].miners / maxM) * cH;
    i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
  }
  ctx.strokeStyle = 'rgba(74,222,128,0.75)';
  ctx.lineWidth = 1.5; ctx.lineJoin = 'round'; ctx.stroke();

  // Hover crosshair
  if (mouseX !== null && mouseX >= pl && mouseX <= pl + cW) {
    const tAtMouse = t0 + ((mouseX - pl) / cW) * tRange;
    let nearest = 0, nearestDist = Infinity;
    for (let i = 0; i < data.length; i++) {
      const d = Math.abs(data[i].timestamp - tAtMouse);
      if (d < nearestDist) { nearestDist = d; nearest = i; }
    }
    const dp = data[nearest];
    const dpX = pl + ((dp.timestamp - t0) / tRange) * cW;
    const dpYH = pt + cH - (dp.hashrate / maxH) * cH;
    const dpYM = pt + cH - (dp.miners / maxM) * cH;

    ctx.setLineDash([3, 3]);
    ctx.strokeStyle = 'rgba(148,163,184,0.2)'; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(dpX, pt); ctx.lineTo(dpX, pt + cH); ctx.stroke();
    ctx.setLineDash([]);

    ctx.beginPath(); ctx.arc(dpX, dpYH, 4, 0, Math.PI * 2);
    ctx.fillStyle = '#7c3aed'; ctx.fill();
    ctx.beginPath(); ctx.arc(dpX, dpYM, 3, 0, Math.PI * 2);
    ctx.fillStyle = '#c084fc'; ctx.fill();

    const timeStr = new Date(dp.timestamp * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const boxW = 122, boxH = 54;
    let bx = dpX + 10;
    if (bx + boxW > pl + cW) bx = dpX - boxW - 10;
    ctx.fillStyle = 'rgba(10,15,30,0.93)'; ctx.strokeStyle = 'rgba(59,130,246,0.3)'; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.roundRect(bx, pt + 8, boxW, boxH, 6); ctx.fill(); ctx.stroke();
    ctx.font = '10px sans-serif'; ctx.textAlign = 'left'; ctx.textBaseline = 'top';
    ctx.fillStyle = 'rgba(148,163,184,0.65)'; ctx.fillText(timeStr, bx + 8, pt + 14);
    ctx.fillStyle = 'rgba(96,165,250,0.9)'; ctx.fillText(fmtHash(dp.hashrate), bx + 8, pt + 28);
    ctx.fillStyle = 'rgba(74,222,128,0.9)'; ctx.fillText(`${dp.miners} miners`, bx + 8, pt + 42);
  }

  // Axis labels
  ctx.save(); ctx.translate(11, pt + cH / 2); ctx.rotate(-Math.PI / 2);
  ctx.font = '9px sans-serif'; ctx.fillStyle = 'rgba(96,165,250,0.45)'; ctx.textAlign = 'center';
  ctx.fillText('Hash Rate', 0, 0); ctx.restore();

  ctx.save(); ctx.translate(W - 7, pt + cH / 2); ctx.rotate(Math.PI / 2);
  ctx.font = '9px sans-serif'; ctx.fillStyle = 'rgba(74,222,128,0.45)'; ctx.textAlign = 'center';
  ctx.fillText('Miners', 0, 0); ctx.restore();
}

// ──────────────────────────────────────────────────────────────
// Radial Power Ring chart (24h by hour — Times-magazine quality)
// ──────────────────────────────────────────────────────────────

function drawRadialChart(canvas: HTMLCanvasElement, data: HashrateHistoryPoint[], windowLabel: string) {
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth || 300;
  const H = canvas.clientHeight || 300;

  if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  ctx.clearRect(0, 0, W, H);

  const cx = W / 2;
  const cy = H / 2;
  const outerR = Math.min(W, H) * 0.44;
  const innerR = outerR * 0.38;

  if (data.length < 2) {
    ctx.font = '10px sans-serif';
    ctx.fillStyle = 'rgba(148,163,184,0.35)';
    ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
    ctx.fillText('Building power ring…', cx, cy);
    return;
  }

  // Bucket data by hour-of-day
  const hourBuckets: number[][] = Array.from({ length: 24 }, () => []);
  for (const p of data) {
    const h = new Date(p.timestamp * 1000).getHours();
    if (p.hashrate > 0) hourBuckets[h].push(p.hashrate);
  }
  const hourAvg = hourBuckets.map((b) =>
    b.length > 0 ? b.reduce((a, x) => a + x, 0) / b.length : 0
  );
  const maxAvg = Math.max(...hourAvg, 1);

  const segAngle = (Math.PI * 2) / 24;
  const gapAngle = 0.012;
  const startOffset = -Math.PI / 2; // 12 o'clock = hour 0

  // Background ring (faint track)
  ctx.beginPath();
  ctx.arc(cx, cy, outerR, 0, Math.PI * 2);
  ctx.strokeStyle = 'rgba(148,163,184,0.06)';
  ctx.lineWidth = outerR - innerR;
  ctx.stroke();

  // Draw each hour segment
  for (let h = 0; h < 24; h++) {
    const pct = hourAvg[h] / maxAvg;
    const segInner = innerR;
    const segOuter = innerR + (outerR - innerR) * Math.max(pct, 0.04);

    const a0 = startOffset + h * segAngle + gapAngle / 2;
    const a1 = startOffset + (h + 1) * segAngle - gapAngle / 2;

    // Color: quantum gradient from cyan (low) to purple (high)
    const hue = 200 + pct * 70; // 200 = cyan, 270 = purple
    const sat = 70 + pct * 25;
    const lum = 45 + pct * 20;
    const alpha = 0.25 + pct * 0.65;

    // Glow for high-power segments
    if (pct > 0.6) {
      ctx.shadowColor = `hsla(${hue},${sat}%,${lum}%,0.5)`;
      ctx.shadowBlur = 8 + pct * 12;
    } else {
      ctx.shadowBlur = 0;
    }

    ctx.beginPath();
    ctx.arc(cx, cy, segOuter, a0, a1);
    ctx.arc(cx, cy, segInner, a1, a0, true);
    ctx.closePath();
    ctx.fillStyle = `hsla(${hue},${sat}%,${lum}%,${(alpha ?? 0)?.toFixed(2)})`;
    ctx.fill();

    // Thin bright edge on outer arc
    ctx.beginPath();
    ctx.arc(cx, cy, segOuter, a0, a1);
    ctx.strokeStyle = `hsla(${hue},${sat + 10}%,${lum + 25}%,${(alpha * 0.7)?.toFixed(2)})`;
    ctx.lineWidth = 1;
    ctx.shadowBlur = 0;
    ctx.stroke();
  }
  ctx.shadowBlur = 0;

  // Hour tick labels (0, 6, 12, 18)
  ctx.font = `bold ${Math.round(outerR * 0.13)}px sans-serif`;
  ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
  const labelR = outerR * 1.18;
  for (const h of [0, 6, 12, 18]) {
    const angle = startOffset + h * segAngle + segAngle / 2;
    const lx = cx + Math.cos(angle) * labelR;
    const ly = cy + Math.sin(angle) * labelR;
    ctx.fillStyle = 'rgba(148,163,184,0.5)';
    ctx.fillText(`${h}h`, lx, ly);
  }

  // Inner circle: dark background + current stats
  const innerGrad = ctx.createRadialGradient(cx, cy, 0, cx, cy, innerR * 0.9);
  innerGrad.addColorStop(0, 'rgba(6,10,22,0.97)');
  innerGrad.addColorStop(1, 'rgba(3,8,24,0.93)');
  ctx.beginPath(); ctx.arc(cx, cy, innerR * 0.95, 0, Math.PI * 2);
  ctx.fillStyle = innerGrad; ctx.fill();

  // Ring border glow
  ctx.beginPath(); ctx.arc(cx, cy, innerR, 0, Math.PI * 2);
  ctx.strokeStyle = 'rgba(0,255,255,0.12)'; ctx.lineWidth = 1; ctx.stroke();

  // Current hashrate text
  const currentHr = data[data.length - 1]?.hashrate ?? 0;
  const fontSize = Math.round(innerR * 0.25);
  ctx.font = `bold ${fontSize}px 'SF Pro Display', system-ui, sans-serif`;
  ctx.fillStyle = '#fff';
  ctx.textAlign = 'center'; ctx.textBaseline = 'middle';

  const hrStr = fmtHash(currentHr);
  // Split number and unit for two-tone rendering
  const parts = hrStr.match(/^([\d.]+)\s*(.+)$/) ?? [hrStr, hrStr, ''];
  ctx.fillText(parts[1], cx, cy - innerR * 0.12);

  ctx.font = `${Math.round(fontSize * 0.48)}px sans-serif`;
  ctx.fillStyle = 'rgba(0,240,255,0.7)';
  ctx.fillText(parts[2], cx, cy + innerR * 0.2);

  // "24h pattern" label
  ctx.font = `${Math.round(innerR * 0.12)}px sans-serif`;
  ctx.fillStyle = 'rgba(148,163,184,0.4)';
  ctx.fillText(`${windowLabel} pattern`, cx, cy + innerR * 0.52);
}

// ──────────────────────────────────────────────────────────────
// Component
// ──────────────────────────────────────────────────────────────

export default function NetworkPowerModal({
  isOpen,
  onClose,
  networkHashRate,
  connectedMiners,
}: NetworkPowerModalProps) {
  const [search, setSearch] = useState('');
  const [sortKey, setSortKey] = useState<SortKey>('hash_rate');
  const [sortDir, setSortDir] = useState<SortDir>('desc');
  const [range, setRange] = useState<TimeRange>('24h');
  const [historyData, setHistoryData] = useState<HashrateHistoryPoint[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [serverMiners, setServerMiners] = useState<ServerMiner[]>([]);
  const [totalMinerCount, setTotalMinerCount] = useState(0);
  const [activeTab, setActiveTab] = useState<'timeline' | 'ring'>('timeline');

  const mainCanvasRef = useRef<HTMLCanvasElement>(null);
  const ringCanvasRef = useRef<HTMLCanvasElement>(null);
  const animRef = useRef<number>(0);
  const historyRef = useRef<HashrateHistoryPoint[]>(historyData);
  const mouseXRef = useRef<number | null>(null);

  useEffect(() => { historyRef.current = historyData; }, [historyData]);

  // 2026-08-25: the server now owns durable history (flux-db rollups), so
  // this just replaces `historyData` with whatever the selected range
  // returns — no client-side merge/dedup needed, that job moved server-side.
  const fetchHistory = useCallback(async (r: TimeRange) => {
    setHistoryLoading(true);
    try {
      const res = await qnkAPI.getHashrateHistory(r);
      if (res.success) {
        setHistoryData(res.history);
      }
    } catch (err) {
      console.error('[NetworkPowerModal] Failed to fetch hashrate history:', err);
    } finally {
      setHistoryLoading(false);
    }
  }, []);

  const fetchMiners = useCallback(async () => {
    try {
      const res = await qnkAPI.getNetworkMiners();
      if (res.success && res.miners) {
        setServerMiners(res.miners);
        setTotalMinerCount(res.total_miners);
      }
    } catch (err) {
      console.error('[NetworkPowerModal] Failed to fetch miners:', err);
    }
  }, []);

  // Refetch history whenever the modal opens OR the selected range changes.
  useEffect(() => {
    if (!isOpen) return;
    fetchHistory(range);
  }, [isOpen, range, fetchHistory]);

  // Miner list + a light auto-refresh of the CURRENT range while open (new
  // samples land server-side every 60s).
  useEffect(() => {
    if (!isOpen) return;
    fetchMiners();
    const hi = setInterval(() => fetchHistory(range), 60_000);
    const mi = setInterval(fetchMiners, 30_000);
    return () => { clearInterval(hi); clearInterval(mi); };
  }, [isOpen, range, fetchHistory, fetchMiners]);

  // Derived stats
  const stats = useMemo(() => {
    if (historyData.length === 0) return null;
    const hrs = historyData.map((d) => d.hashrate);
    const avg = hrs.reduce((a, b) => a + b, 0) / hrs.length;
    const peak = Math.max(...hrs);
    const peakMiners = Math.max(...historyData.map((d) => d.miners));
    const first = historyData[0]?.hashrate ?? 0;
    const last = historyData[historyData.length - 1]?.hashrate ?? 0;
    const growth = first > 0 ? ((last - first) / first) * 100 : 0;
    const coverage = historyData.length; // number of samples
    return { avg, peak, peakMiners, growth, coverage };
  }, [historyData]);

  const rangeLabel = RANGE_OPTIONS.find((o) => o.key === range)?.label ?? range;

  // Sorted miner list
  const sortedMiners = useMemo(() => {
    let arr = [...serverMiners];
    if (search) {
      const q = search.toLowerCase();
      arr = arr.filter((m) => m.wallet.toLowerCase().includes(q) || m.rig.toLowerCase().includes(q));
    }
    arr.sort((a, b) => {
      let cmp = 0;
      if (sortKey === 'hash_rate') cmp = a.hash_rate - b.hash_rate;
      else cmp = b.last_seen_secs_ago - a.last_seen_secs_ago;
      return sortDir === 'desc' ? -cmp : cmp;
    });
    return arr;
  }, [serverMiners, search, sortKey, sortDir]);

  const handleSort = useCallback(
    (key: SortKey) => {
      if (sortKey === key) setSortDir((d) => (d === 'desc' ? 'asc' : 'desc'));
      else { setSortKey(key); setSortDir('desc'); }
    },
    [sortKey]
  );
  const sortArrow = (key: SortKey) => sortKey === key ? (sortDir === 'desc' ? ' ▼' : ' ▲') : '';

  // Animation loop for main chart
  const drawLoop = useCallback(() => {
    if (activeTab === 'timeline' && mainCanvasRef.current) {
      drawMainChart(mainCanvasRef.current, historyRef.current, mouseXRef.current);
    }
    animRef.current = requestAnimationFrame(drawLoop);
  }, [activeTab]);

  // Draw ring chart (static — no animation needed)
  useEffect(() => {
    if (activeTab === 'ring' && ringCanvasRef.current) {
      drawRadialChart(ringCanvasRef.current, historyRef.current, rangeLabel);
    }
  }, [activeTab, historyData, rangeLabel]);

  useEffect(() => {
    if (!isOpen) return;
    animRef.current = requestAnimationFrame(drawLoop);
    return () => { if (animRef.current) cancelAnimationFrame(animRef.current); };
  }, [isOpen, drawLoop]);

  const handleCanvasMouseMove = useCallback((e: React.MouseEvent<HTMLCanvasElement>) => {
    mouseXRef.current = e.clientX - e.currentTarget.getBoundingClientRect().left;
  }, []);
  const handleCanvasMouseLeave = useCallback(() => { mouseXRef.current = null; }, []);

  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [isOpen, onClose]);

  return (
    <AnimatePresence>
      {isOpen && (
        <>
          <motion.div
            initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
            transition={{ duration: 0.2 }}
            className="fixed inset-0 z-50 bg-black/65 backdrop-blur-sm"
            onClick={onClose}
          />

          <motion.div
            initial={{ x: '100%' }} animate={{ x: 0 }} exit={{ x: '100%' }}
            transition={{ type: 'spring', damping: 30, stiffness: 300 }}
            className="fixed inset-y-0 right-0 z-50 w-full max-w-[1280px] bg-gradient-to-bl from-[#080d1c] to-[#020710] border-l border-quantum-cyan/20 shadow-2xl overflow-hidden flex flex-col"
            onClick={(e) => e.stopPropagation()}
          >
            {/* ── Header ── */}
            <div className="flex items-center justify-between px-6 py-4 border-b border-white/5 flex-shrink-0">
              <div className="flex items-center gap-3">
                <Activity className="w-5 h-5 text-quantum-cyan" />
                <h2 className="text-lg font-bold text-white tracking-tight">Network Power</h2>
                <span className="text-xs bg-quantum-cyan/15 text-quantum-cyan px-2.5 py-0.5 rounded-full font-mono">
                  {fmtHash(networkHashRate)}
                </span>
                {connectedMiners > 0 && (
                  <span className="text-xs bg-quantum-green/15 text-quantum-green px-2.5 py-0.5 rounded-full">
                    {connectedMiners} miners
                  </span>
                )}
                {historyLoading && (
                  <span className="text-[10px] text-gray-600 ml-1">loading…</span>
                )}
              </div>
              <div className="flex items-center gap-3">
                {/* Timeframe selector — durable server-side history, not a
                    24h-only client cache. See mining_history module docs. */}
                <div className="flex items-center gap-0.5 bg-white/5 border border-white/8 rounded-lg p-0.5">
                  {RANGE_OPTIONS.map((o) => (
                    <button
                      key={o.key}
                      onClick={() => setRange(o.key)}
                      className={`px-2.5 py-1 rounded-md text-[10px] font-semibold font-mono transition-colors ${
                        range === o.key
                          ? 'bg-quantum-cyan/20 text-quantum-cyan'
                          : 'text-gray-500 hover:text-gray-300 hover:bg-white/5'
                      }`}
                    >
                      {o.label}
                    </button>
                  ))}
                </div>
                <button onClick={onClose} className="p-1.5 rounded-lg hover:bg-white/10 transition-colors">
                  <X className="w-5 h-5 text-gray-400" />
                </button>
              </div>
            </div>

            {/* ── Stats bar ── */}
            {stats && (
              <div className="flex items-center gap-px border-b border-white/5 flex-shrink-0 bg-white/[0.015]">
                {[
                  { icon: <Zap className="w-3.5 h-3.5" />, label: `Peak ${rangeLabel}`, value: fmtHash(stats.peak), color: 'text-purple-400' },
                  { icon: <Activity className="w-3.5 h-3.5" />, label: `Avg ${rangeLabel}`, value: fmtHash(stats.avg), color: 'text-violet-400' },
                  { icon: <Users className="w-3.5 h-3.5" />, label: 'Peak Miners', value: `${stats.peakMiners}`, color: 'text-violet-400' },
                  {
                    icon: <TrendingUp className="w-3.5 h-3.5" />,
                    label: `${rangeLabel} Change`,
                    value: `${stats.growth >= 0 ? '+' : ''}${stats.growth?.toFixed(1)}%`,
                    color: stats.growth >= 0 ? 'text-violet-400' : 'text-red-400',
                  },
                  { icon: <Clock className="w-3.5 h-3.5" />, label: 'Samples', value: `${Math.round(stats.coverage)}`, color: 'text-purple-400' },
                  { icon: <Award className="w-3.5 h-3.5" />, label: 'Current', value: fmtHash(networkHashRate), color: 'text-yellow-400' },
                ].map((s, i) => (
                  <div key={i} className="flex-1 flex items-center gap-2 px-4 py-2.5">
                    <span className={`${s.color} opacity-60`}>{s.icon}</span>
                    <div>
                      <div className="text-[9px] text-gray-600 uppercase tracking-wider">{s.label}</div>
                      <div className={`text-xs font-bold ${s.color} font-mono`}>{s.value}</div>
                    </div>
                  </div>
                ))}
              </div>
            )}

            {/* ── Body ── */}
            <div className="flex-1 flex flex-col lg:flex-row overflow-hidden min-h-0">

              {/* Left: Miner List */}
              <div className="w-full lg:w-[420px] flex-shrink-0 flex flex-col border-r border-white/5 min-h-0">
                <div className="px-4 pt-3 pb-2 flex items-center justify-between flex-shrink-0">
                  <div className="flex items-center gap-2">
                    <Users className="w-4 h-4 text-quantum-cyan" />
                    <span className="text-sm font-semibold text-white">
                      {totalMinerCount > 0 ? totalMinerCount : sortedMiners.length} active
                    </span>
                  </div>
                </div>

                <div className="px-4 pb-2 flex-shrink-0">
                  <div className="relative">
                    <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-gray-600" />
                    <input
                      type="text"
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                      placeholder="Filter by wallet or rig…"
                      className="w-full pl-8 pr-3 py-1.5 bg-white/5 border border-white/8 rounded-lg text-xs text-white placeholder-gray-600 focus:outline-none focus:border-quantum-cyan/40"
                    />
                  </div>
                </div>

                <div className="grid grid-cols-[36px_1fr_82px_60px_46px] gap-0.5 px-4 pb-1 text-[9px] text-gray-600 uppercase tracking-wider select-none flex-shrink-0">
                  <span>#</span>
                  <span>Miner</span>
                  <button onClick={() => handleSort('hash_rate')} className="text-left hover:text-gray-400">Hash{sortArrow('hash_rate')}</button>
                  <button onClick={() => handleSort('last_seen_secs_ago')} className="text-left hover:text-gray-400">Seen{sortArrow('last_seen_secs_ago')}</button>
                  <span>HW</span>
                </div>

                <div className="flex-1 overflow-y-auto min-h-0 px-2">
                  {sortedMiners.length === 0 ? (
                    <div className="flex flex-col items-center justify-center h-32 text-gray-600 text-xs gap-1">
                      {search ? 'No miners match filter' : 'Loading miners…'}
                    </div>
                  ) : (
                    sortedMiners.map((miner, idx) => (
                      <div
                        key={`${miner.wallet}-${miner.rig}`}
                        className="grid grid-cols-[36px_1fr_82px_60px_46px] gap-0.5 items-center px-2 py-1.5 rounded-md hover:bg-white/[0.04] transition-colors text-xs"
                      >
                        <span className={`font-mono font-bold text-[10px] ${idx < 3 ? RANK_BADGE[idx] : 'text-gray-600'}`}>
                          {idx < 3 ? ['1st','2nd','3rd'][idx] : `#${idx+1}`}
                        </span>
                        <div className="flex flex-col truncate">
                          <span className="text-gray-300 font-mono truncate text-[10px]" title={miner.wallet}>
                            {truncAddr(miner.wallet)}
                          </span>
                          {miner.rig && (
                            <span className="text-[9px] text-gray-600 truncate">{miner.rig}</span>
                          )}
                        </div>
                        <span className="text-purple-400 font-mono text-[10px]">{fmtHash(miner.hash_rate)}</span>
                        <span className="text-gray-600 font-mono text-[9px]">{secsAgo(miner.last_seen_secs_ago)}</span>
                        <span
                          className={`flex items-center justify-center gap-0.5 text-[8px] px-1 py-0.5 rounded ${kindBadgeClass(miner.kind)}`}
                          title={miner.kind === 'unknown' ? 'Hardware not reported (older miner client)' : undefined}
                        >
                          {miner.kind === 'gpu' ? <MonitorSmartphone className="w-2.5 h-2.5" /> : <Cpu className="w-2.5 h-2.5" />}
                          {kindLabel(miner.kind)}
                        </span>
                      </div>
                    ))
                  )}
                </div>
              </div>

              {/* Right: Charts */}
              <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
                {/* Tab bar */}
                <div className="flex items-center gap-1 px-4 pt-3 pb-2 border-b border-white/5 flex-shrink-0">
                  <button
                    onClick={() => setActiveTab('timeline')}
                    className={`flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
                      activeTab === 'timeline'
                        ? 'bg-purple-500/20 text-purple-400'
                        : 'text-gray-500 hover:text-gray-300 hover:bg-white/5'
                    }`}
                  >
                    <TrendingUp className="w-3.5 h-3.5" />
                    Hashrate Timeline
                  </button>
                  <button
                    onClick={() => setActiveTab('ring')}
                    className={`flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
                      activeTab === 'ring'
                        ? 'bg-violet-500/20 text-violet-400'
                        : 'text-gray-500 hover:text-gray-300 hover:bg-white/5'
                    }`}
                  >
                    <Activity className="w-3.5 h-3.5" />
                    Power Ring
                  </button>
                  <div className="ml-auto flex items-center gap-3 text-[10px]">
                    {activeTab === 'timeline' && (
                      <>
                        <span className="flex items-center gap-1"><span className="w-2 h-2 rounded-full bg-purple-500" /><span className="text-gray-500">Hash Rate</span></span>
                        <span className="flex items-center gap-1"><span className="w-2 h-2 rounded-full bg-violet-400" /><span className="text-gray-500">Miners</span></span>
                      </>
                    )}
                    {activeTab === 'ring' && (
                      <span className="text-gray-600">Hourly distribution across the {rangeLabel} window — brighter = more power</span>
                    )}
                  </div>
                </div>

                {/* Chart area */}
                <div className="flex-1 min-h-0 p-4 flex flex-col">
                  {/* Timeline tab */}
                  {activeTab === 'timeline' && (
                    <>
                      <div className="flex-1 relative bg-white/[0.015] rounded-xl border border-white/[0.06] overflow-hidden min-h-[180px]">
                        <canvas
                          ref={mainCanvasRef}
                          className="w-full h-full"
                          onMouseMove={handleCanvasMouseMove}
                          onMouseLeave={handleCanvasMouseLeave}
                        />
                      </div>

                      {/* Stats cards */}
                      <div className="grid grid-cols-3 gap-3 mt-3 flex-shrink-0">
                        <div className="bg-white/[0.025] border border-white/[0.06] rounded-xl p-3">
                          <div className="text-[9px] text-gray-600 uppercase tracking-wider mb-1.5">Peak Hashrate</div>
                          <div className="text-sm font-bold text-purple-400 font-mono">
                            {stats ? fmtHash(stats.peak) : '--'}
                          </div>
                          <div className="text-[9px] text-gray-600 mt-0.5">last {rangeLabel}</div>
                        </div>
                        <div className="bg-white/[0.025] border border-white/[0.06] rounded-xl p-3">
                          <div className="text-[9px] text-gray-600 uppercase tracking-wider mb-1.5">Avg Hashrate</div>
                          <div className="text-sm font-bold text-violet-400 font-mono">
                            {stats ? fmtHash(stats.avg) : '--'}
                          </div>
                          <div className="text-[9px] text-gray-600 mt-0.5">{rangeLabel} average</div>
                        </div>
                        <div className="bg-white/[0.025] border border-white/[0.06] rounded-xl p-3">
                          <div className="text-[9px] text-gray-600 uppercase tracking-wider mb-1.5">{rangeLabel} Trend</div>
                          <div className={`text-sm font-bold font-mono ${stats && stats.growth >= 0 ? 'text-violet-400' : 'text-red-400'}`}>
                            {stats ? `${stats.growth >= 0 ? '+' : ''}${stats.growth?.toFixed(1)}%` : '--'}
                          </div>
                          <div className="text-[9px] text-gray-600 mt-0.5">start vs now</div>
                        </div>
                      </div>
                    </>
                  )}

                  {/* Power Ring tab */}
                  {activeTab === 'ring' && (
                    <div className="flex-1 flex flex-col lg:flex-row gap-4 min-h-0">
                      {/* Ring canvas */}
                      <div className="flex items-center justify-center flex-1 min-h-0">
                        <div className="relative bg-white/[0.015] rounded-2xl border border-white/[0.06] overflow-hidden"
                             style={{ width: '100%', maxWidth: 380, aspectRatio: '1' }}>
                          <canvas
                            ref={ringCanvasRef}
                            className="w-full h-full"
                          />
                        </div>
                      </div>

                      {/* Ring legend / breakdown */}
                      <div className="lg:w-52 flex flex-col gap-3 flex-shrink-0 justify-center">
                        <div>
                          <div className="text-[9px] text-gray-600 uppercase tracking-wider mb-2">Hourly Breakdown</div>
                          <div className="space-y-1 max-h-64 overflow-y-auto pr-1">
                            {Array.from({ length: 24 }, (_, h) => {
                              const pts = historyData.filter(
                                (d) => new Date(d.timestamp * 1000).getHours() === h && d.hashrate > 0
                              );
                              const avg = pts.length > 0 ? pts.reduce((a, p) => a + p.hashrate, 0) / pts.length : 0;
                              const peakAll = stats?.peak ?? 1;
                              const pct = avg / peakAll;
                              return (
                                <div key={h} className="flex items-center gap-2">
                                  <span className="text-[9px] text-gray-600 w-5 text-right font-mono">{h}h</span>
                                  <div className="flex-1 h-1.5 bg-white/5 rounded-full overflow-hidden">
                                    <div
                                      className="h-full rounded-full transition-all"
                                      style={{
                                        width: `${pct * 100}%`,
                                        background: `hsl(${200 + pct * 70},${70 + pct * 25}%,${45 + pct * 20}%)`,
                                        opacity: 0.7 + pct * 0.3,
                                      }}
                                    />
                                  </div>
                                  <span className="text-[9px] text-gray-600 font-mono w-14 text-right">
                                    {avg > 0 ? fmtHash(avg) : '—'}
                                  </span>
                                </div>
                              );
                            })}
                          </div>
                        </div>

                        <div className="border-t border-white/5 pt-3 space-y-2">
                          <div className="text-[9px] text-gray-600 uppercase tracking-wider">Ring Legend</div>
                          <div className="flex items-center gap-2">
                            <div className="w-6 h-2 rounded" style={{ background: 'hsl(200,70%,45%)' }} />
                            <span className="text-[9px] text-gray-500">Low activity</span>
                          </div>
                          <div className="flex items-center gap-2">
                            <div className="w-6 h-2 rounded" style={{ background: 'hsl(235,82%,55%)' }} />
                            <span className="text-[9px] text-gray-500">Medium activity</span>
                          </div>
                          <div className="flex items-center gap-2">
                            <div className="w-6 h-2 rounded" style={{ background: 'hsl(270,95%,65%)' }} />
                            <span className="text-[9px] text-gray-500">Peak activity</span>
                          </div>
                        </div>
                      </div>
                    </div>
                  )}
                </div>
              </div>
            </div>
          </motion.div>
        </>
      )}
    </AnimatePresence>
  );
}
