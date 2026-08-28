import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import {
  ArrowDownUp, Droplet, Loader2, AlertTriangle, CheckCircle2, Info,
  RefreshCw, Settings, ChevronDown, ChevronRight, ChevronLeft, Lock, Sparkles,
} from 'lucide-react';
import TokenIcon from './TokenIcon';
import SwapSuccessModal from './SwapSuccessModal';
import { qnkAPI } from '../services/api';

// ─────────────────────────────────────────────────────────────────────────
// SIGIL-native DEX page. Talks ONLY to sigil-rpcd's real, live routes:
// GET /pools, GET /tokens, GET /balance, POST /swap, POST /add_liquidity.
// No perps, no DCA, no limit orders, no mixer — those are Quillon q-api-server
// features with no SIGIL backend yet (flux_sigil_dao_vm_dex_bundle marks the
// VM path as scaffolded / NotImplemented pending VM-1 wasmi wiring). This
// screen is deliberately scoped to what actually settles on-chain today, so
// every button here does what it says. See the honesty strip at the bottom.
//
// AMM math mirrors sigil-dex::swap() + sigil-bank::split_swap_output() EXACTLY
// (constant product, fee_bps pool fee + a fixed 30bps/0.30% master cut taken
// from the output) so the on-screen quote matches what the chain will do,
// modulo integer-truncation rounding at the very last decimal.
// ─────────────────────────────────────────────────────────────────────────

const DECIMALS = 8; // SIGIL-native base-unit convention used app-wide (see api.ts sendTransaction)
const SCALE = 10 ** DECIMALS;
const MASTER_SWAP_FEE_BPS = 30; // sigil-bank::MASTER_SWAP_FEE_BPS
const BPS_DENOM = 10_000;

interface Pool {
  id: string;
  label: string;
  token_a: string;
  token_b: string;
  sym_a: string;
  sym_b: string;
  reserve_a: number;
  reserve_b: number;
  lp_shares: number;
  fee_bps: number;
  accrued_fees: number;
}

interface TokenEntry {
  symbol: string;
  id: string;
}

function fmt(n: number, dp = 4): string {
  if (!Number.isFinite(n)) return '0';
  return n.toLocaleString(undefined, { maximumFractionDigits: dp });
}

function toBase(whole: number): number {
  return Math.round(whole * SCALE);
}

function fromBase(base: number): number {
  return base / SCALE;
}

/** Mirrors sigil-dex::swap() exactly: constant-product with pool.fee_bps
 * retained by LPs, integer-truncated the same way (floor division). */
function quoteAmmOut(reserveIn: number, reserveOut: number, amountIn: number, feeBps: number): number {
  if (amountIn <= 0 || reserveIn <= 0 || reserveOut <= 0) return 0;
  const feeComplement = BPS_DENOM - feeBps;
  const amountInWithFee = amountIn * feeComplement;
  const num = amountInWithFee * reserveOut;
  const den = reserveIn * BPS_DENOM + amountInWithFee;
  return Math.floor(num / den);
}

/** Mirrors sigil-bank::split_swap_output() — the fixed 0.30% master cut taken
 * from the gross AMM output, in the output token. */
function splitMasterFee(amountOut: number): { userShare: number; masterShare: number } {
  const masterShare = Math.floor((amountOut * MASTER_SWAP_FEE_BPS) / BPS_DENOM);
  return { userShare: amountOut - masterShare, masterShare };
}

const SLIPPAGE_OPTIONS = [0.5, 1, 3];

export default function SigilDexScreen({ isActive = true }: { isActive?: boolean }) {
  const [pools, setPools] = useState<Pool[]>([]);
  const [tokens, setTokens] = useState<TokenEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const [selectedPoolId, setSelectedPoolId] = useState<string>('');
  const [dir, setDir] = useState<'AtoB' | 'BtoA'>('AtoB');
  const [amountInStr, setAmountInStr] = useState('');
  const [slippagePct, setSlippagePct] = useState(1);
  const [showSlippageMenu, setShowSlippageMenu] = useState(false);
  const [isSwapCollapsed, setIsSwapCollapsed] = useState(false);

  const [swapping, setSwapping] = useState(false);
  const [swapError, setSwapError] = useState<string | null>(null);
  const [successOpen, setSuccessOpen] = useState(false);
  const [successData, setSuccessData] = useState<{ from: string; to: string; amtIn: number; amtOut: number } | null>(null);

  const [liqAmountAStr, setLiqAmountAStr] = useState('');
  const [liqAmountBStr, setLiqAmountBStr] = useState('');
  const [addingLiq, setAddingLiq] = useState(false);
  const [liqError, setLiqError] = useState<string | null>(null);
  const [liqSuccess, setLiqSuccess] = useState<string | null>(null);

  const [walletAddress, setWalletAddress] = useState<string>('');
  const [balances, setBalances] = useState<Record<string, number>>({});
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const loadMarket = useCallback(async (silent = false) => {
    if (!silent) setLoading(true);
    setRefreshing(true);
    try {
      const [poolsRes, tokensRes] = await Promise.all([qnkAPI.sigilDexPools(), qnkAPI.sigilDexTokens()]);
      if (poolsRes.success && poolsRes.data?.pools) {
        setPools(poolsRes.data.pools);
        setLoadError(null);
        setSelectedPoolId(prev => prev || poolsRes.data.pools[0]?.id || '');
      } else if (!silent) {
        setLoadError(poolsRes.error || 'Could not reach the SIGIL DEX (sigil-rpcd /pools).');
      }
      if (tokensRes.success && tokensRes.data?.tokens) {
        setTokens(tokensRes.data.tokens);
      }
    } catch (e) {
      if (!silent) setLoadError(e instanceof Error ? e.message : 'Network error reaching sigil-rpcd.');
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    setWalletAddress(localStorage.getItem('walletAddress') || '');
  }, []);

  useEffect(() => {
    if (!isActive) return;
    loadMarket();
    pollRef.current = setInterval(() => loadMarket(true), 10000);
    return () => { if (pollRef.current) clearInterval(pollRef.current); };
  }, [isActive, loadMarket]);

  const refreshBalances = useCallback(async (addr: string, pool: Pool | undefined) => {
    if (!addr || !pool) return;
    try {
      const [balA, balB] = await Promise.all([
        qnkAPI.sigilDexBalance(addr, pool.token_a),
        qnkAPI.sigilDexBalance(addr, pool.token_b),
      ]);
      setBalances(prev => ({
        ...prev,
        [pool.sym_a]: balA.success ? fromBase(Number(balA.data?.balance ?? 0)) : prev[pool.sym_a] ?? 0,
        [pool.sym_b]: balB.success ? fromBase(Number(balB.data?.balance ?? 0)) : prev[pool.sym_b] ?? 0,
      }));
    } catch {
      // balances are a display nicety — silently skip on failure
    }
  }, []);

  const selectedPool = useMemo(() => pools.find(p => p.id === selectedPoolId), [pools, selectedPoolId]);

  useEffect(() => {
    if (walletAddress && selectedPool) refreshBalances(walletAddress, selectedPool);
  }, [walletAddress, selectedPool, refreshBalances]);

  const { symIn, symOut, reserveIn, reserveOut } = useMemo(() => {
    if (!selectedPool) return { symIn: '', symOut: '', reserveIn: 0, reserveOut: 0 };
    return dir === 'AtoB'
      ? { symIn: selectedPool.sym_a, symOut: selectedPool.sym_b, reserveIn: selectedPool.reserve_a, reserveOut: selectedPool.reserve_b }
      : { symIn: selectedPool.sym_b, symOut: selectedPool.sym_a, reserveIn: selectedPool.reserve_b, reserveOut: selectedPool.reserve_a };
  }, [selectedPool, dir]);

  const amountInBase = useMemo(() => {
    const v = parseFloat(amountInStr);
    return Number.isFinite(v) && v > 0 ? toBase(v) : 0;
  }, [amountInStr]);

  const quote = useMemo(() => {
    if (!selectedPool || amountInBase <= 0) return null;
    const grossOut = quoteAmmOut(reserveIn, reserveOut, amountInBase, selectedPool.fee_bps);
    if (grossOut <= 0) return null;
    const { userShare, masterShare } = splitMasterFee(grossOut);
    const midPrice = reserveOut / Math.max(reserveIn, 1);
    const execPrice = grossOut / amountInBase;
    const priceImpactPct = midPrice > 0 ? Math.max(0, (1 - execPrice / midPrice) * 100) : 0;
    const minOutBase = Math.floor(userShare * (1 - slippagePct / 100));
    return { grossOut, userShare, masterShare, priceImpactPct, minOutBase };
  }, [selectedPool, amountInBase, reserveIn, reserveOut, slippagePct]);

  const flipDirection = () => setDir(d => (d === 'AtoB' ? 'BtoA' : 'AtoB'));

  const handleSwap = async () => {
    if (!selectedPool || !quote || amountInBase <= 0) return;
    setSwapping(true);
    setSwapError(null);
    try {
      const res = await qnkAPI.sigilDexSwap(selectedPool.id, dir, amountInBase, quote.minOutBase);
      if (res.success) {
        setSuccessData({
          from: symIn, to: symOut,
          amtIn: fromBase(amountInBase),
          amtOut: fromBase(Number(res.data?.amount_out ?? quote.grossOut)),
        });
        setSuccessOpen(true);
        setAmountInStr('');
        loadMarket(true);
        if (walletAddress) refreshBalances(walletAddress, selectedPool);
      } else {
        setSwapError(res.error || 'Swap rejected by sigil-rpcd.');
      }
    } catch (e) {
      setSwapError(e instanceof Error ? e.message : 'Swap failed.');
    } finally {
      setSwapping(false);
    }
  };

  const liqShareHint = useMemo(() => {
    if (!selectedPool || selectedPool.reserve_a <= 0) return null;
    const ratio = selectedPool.reserve_b / selectedPool.reserve_a;
    return ratio;
  }, [selectedPool]);

  const onLiqAmountAChange = (v: string) => {
    setLiqAmountAStr(v);
    if (liqShareHint && v) {
      const a = parseFloat(v);
      if (Number.isFinite(a)) setLiqAmountBStr((a * liqShareHint).toFixed(DECIMALS));
    }
  };

  const handleAddLiquidity = async () => {
    if (!selectedPool) return;
    const a = parseFloat(liqAmountAStr);
    const b = parseFloat(liqAmountBStr);
    if (!Number.isFinite(a) || !Number.isFinite(b) || a <= 0 || b <= 0) {
      setLiqError('Enter positive amounts for both sides.');
      return;
    }
    setAddingLiq(true);
    setLiqError(null);
    setLiqSuccess(null);
    try {
      const res = await qnkAPI.sigilDexAddLiquidity(selectedPool.id, toBase(a), toBase(b));
      if (res.success) {
        setLiqSuccess(`+${fmt(Number(res.data?.shares_minted ?? 0) / SCALE, 6)} LP shares minted.`);
        setLiqAmountAStr('');
        setLiqAmountBStr('');
        loadMarket(true);
        if (walletAddress) refreshBalances(walletAddress, selectedPool);
      } else {
        setLiqError(res.error || 'Add-liquidity rejected by sigil-rpcd.');
      }
    } catch (e) {
      setLiqError(e instanceof Error ? e.message : 'Add-liquidity failed.');
    } finally {
      setAddingLiq(false);
    }
  };

  if (!isActive) return null;

  return (
    <div className="min-h-full bg-gray-950 text-gray-100 p-4 md:p-6 space-y-6">
      {/* Header */}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold flex items-center gap-2">
            <ArrowDownUp className="w-6 h-6 text-emerald-400" />
            SIGIL DEX
            <span className="text-xs font-semibold px-2 py-0.5 rounded-full bg-emerald-500/15 text-emerald-400 border border-emerald-500/30">
              live · sigil-g0
            </span>
          </h1>
          <p className="text-sm text-gray-400 mt-1">
            Constant-product AMM settling directly on sigil-state — every swap and liquidity add here writes a real state transition.
          </p>
        </div>
        <button
          onClick={() => loadMarket()}
          className="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-gray-800 hover:bg-gray-700 text-sm text-gray-300 transition-colors"
        >
          <RefreshCw className={`w-4 h-4 ${refreshing ? 'animate-spin' : ''}`} />
          Refresh
        </button>
      </div>

      {loadError && (
        <div className="flex items-center gap-2 px-4 py-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-300 text-sm">
          <AlertTriangle className="w-4 h-4 flex-shrink-0" />
          {loadError}
        </div>
      )}

      {loading ? (
        <div className="flex items-center justify-center py-24 text-gray-400">
          <Loader2 className="w-6 h-6 animate-spin mr-2" /> Loading live pools from sigil-rpcd…
        </div>
      ) : pools.length === 0 ? (
        <div className="text-center py-24 text-gray-400">
          <Droplet className="w-10 h-10 mx-auto mb-3 opacity-40" />
          No pools registered on this node yet.
        </div>
      ) : (
        <div className="flex flex-col lg:flex-row gap-4">
          {/* Swap card — collapsible: click the chevron to fold it into a thin
              rail so the token table can take the full width, or expand it
              back out. Mirrors the same collapse pattern as Quillon's DEX. */}
          {isSwapCollapsed ? (
            <motion.button
              layout
              onClick={() => setIsSwapCollapsed(false)}
              className="flex-shrink-0 w-10 flex flex-col items-center gap-2 py-4 bg-gray-900/80 border border-gray-800 rounded-2xl hover:bg-gray-800/80 transition-colors"
              title="Expand swap panel"
            >
              <ChevronRight className="w-4 h-4 text-emerald-400" />
              <span className="[writing-mode:vertical-lr] rotate-180 text-xs font-semibold text-gray-300 tracking-wide">Swap</span>
            </motion.button>
          ) : (
          <motion.div layout className="w-full lg:w-[380px] flex-shrink-0 bg-gray-900/80 border border-gray-800 rounded-2xl p-5 space-y-4">
            <div className="flex items-center justify-between">
              <h2 className="font-semibold text-gray-200">Swap</h2>
              <div className="flex items-center gap-1">
                <div className="relative">
                  <button
                    onClick={() => setShowSlippageMenu(s => !s)}
                    className="flex items-center gap-1 text-xs text-gray-400 hover:text-gray-200 px-2 py-1 rounded-md bg-gray-800/70"
                  >
                    <Settings className="w-3.5 h-3.5" /> {slippagePct}% slippage <ChevronDown className="w-3 h-3" />
                  </button>
                  {showSlippageMenu && (
                    <div className="absolute right-0 mt-1 bg-gray-800 border border-gray-700 rounded-lg shadow-xl z-10 p-2 flex gap-1">
                      {SLIPPAGE_OPTIONS.map(v => (
                        <button
                          key={v}
                          onClick={() => { setSlippagePct(v); setShowSlippageMenu(false); }}
                          className={`px-2 py-1 rounded text-xs ${slippagePct === v ? 'bg-emerald-500/20 text-emerald-300' : 'text-gray-300 hover:bg-gray-700'}`}
                        >
                          {v}%
                        </button>
                      ))}
                    </div>
                  )}
                </div>
                <button
                  onClick={() => setIsSwapCollapsed(true)}
                  className="p-1.5 rounded-md text-gray-500 hover:text-gray-200 hover:bg-gray-800/70"
                  title="Collapse swap panel"
                >
                  <ChevronLeft className="w-3.5 h-3.5" />
                </button>
              </div>
            </div>

            {/* Pool picker */}
            <select
              value={selectedPoolId}
              onChange={e => { setSelectedPoolId(e.target.value); setDir('AtoB'); }}
              className="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-gray-200"
            >
              {pools.map(p => (
                <option key={p.id} value={p.id}>{p.sym_a} / {p.sym_b} — {p.label || p.id.slice(0, 8)}</option>
              ))}
            </select>

            {/* From */}
            <div className="bg-gray-800/60 rounded-xl p-3 space-y-2">
              <div className="flex items-center justify-between text-xs text-gray-400">
                <span>From</span>
                <span>Balance: {fmt(balances[symIn] ?? 0)}</span>
              </div>
              <div className="flex items-center gap-3">
                <TokenIcon symbol={symIn} size={28} />
                <span className="font-semibold text-gray-200 w-20">{symIn}</span>
                <input
                  type="number"
                  min="0"
                  value={amountInStr}
                  onChange={e => setAmountInStr(e.target.value)}
                  placeholder="0.0"
                  className="flex-1 bg-transparent text-right text-lg font-mono outline-none text-gray-100"
                />
              </div>
            </div>

            <div className="flex justify-center -my-2 relative z-10">
              <button
                onClick={flipDirection}
                className="p-2 rounded-full bg-gray-800 border-4 border-gray-950 hover:bg-gray-700 transition-colors"
              >
                <ArrowDownUp className="w-4 h-4 text-emerald-400" />
              </button>
            </div>

            {/* To */}
            <div className="bg-gray-800/60 rounded-xl p-3 space-y-2">
              <div className="flex items-center justify-between text-xs text-gray-400">
                <span>To (estimated)</span>
                <span>Balance: {fmt(balances[symOut] ?? 0)}</span>
              </div>
              <div className="flex items-center gap-3">
                <TokenIcon symbol={symOut} size={28} />
                <span className="font-semibold text-gray-200 w-20">{symOut}</span>
                <span className="flex-1 text-right text-lg font-mono text-gray-100">
                  {quote ? fmt(fromBase(quote.userShare)) : '0.0'}
                </span>
              </div>
            </div>

            {quote && (
              <div className="text-xs text-gray-400 space-y-1 px-1">
                <div className="flex justify-between">
                  <span>Price impact</span>
                  <span className={quote.priceImpactPct > 5 ? 'text-amber-400' : 'text-gray-300'}>{fmt(quote.priceImpactPct, 3)}%</span>
                </div>
                <div className="flex justify-between">
                  <span>LP fee ({(selectedPool?.fee_bps ?? 0) / 100}%) + protocol fee (0.30%)</span>
                  <span>{fmt(fromBase(quote.masterShare), 6)} {symOut}</span>
                </div>
                <div className="flex justify-between">
                  <span>Min received ({slippagePct}% slippage)</span>
                  <span>{fmt(fromBase(quote.minOutBase))} {symOut}</span>
                </div>
              </div>
            )}

            {swapError && (
              <div className="flex items-center gap-2 text-xs text-red-300 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2">
                <AlertTriangle className="w-3.5 h-3.5 flex-shrink-0" /> {swapError}
              </div>
            )}

            <button
              onClick={handleSwap}
              disabled={swapping || !quote || amountInBase <= 0}
              className="w-full py-3 rounded-xl font-semibold bg-emerald-500 hover:bg-emerald-400 disabled:bg-gray-800 disabled:text-gray-500 text-gray-950 transition-colors flex items-center justify-center gap-2"
            >
              {swapping ? <Loader2 className="w-4 h-4 animate-spin" /> : <ArrowDownUp className="w-4 h-4" />}
              {swapping ? 'Signing & submitting…' : !quote ? 'Enter an amount' : `Swap ${symIn} → ${symOut}`}
            </button>
            {!walletAddress && (
              <p className="text-xs text-gray-500 flex items-center gap-1.5"><Lock className="w-3 h-3" /> No wallet detected — unlock or import a wallet to swap.</p>
            )}
          </motion.div>
          )}

          {/* Pools + liquidity — fills whatever width the swap panel isn't using */}
          <div className="flex-1 min-w-0 space-y-6">
            <div className="bg-gray-900/80 border border-gray-800 rounded-2xl p-5">
              <h2 className="font-semibold text-gray-200 mb-3 flex items-center gap-2"><Droplet className="w-4 h-4 text-emerald-400" /> Live pools</h2>
              <div className="overflow-x-auto">
                <table className="w-full text-sm">
                  <thead className="text-gray-500 text-xs uppercase">
                    <tr>
                      <th className="text-left pb-2">Pair</th>
                      <th className="text-right pb-2">Reserves</th>
                      <th className="text-right pb-2">Fee</th>
                      <th className="text-right pb-2">LP shares</th>
                    </tr>
                  </thead>
                  <tbody>
                    {pools.map(p => (
                      <tr
                        key={p.id}
                        onClick={() => { setSelectedPoolId(p.id); setDir('AtoB'); }}
                        className={`cursor-pointer border-t border-gray-800/70 hover:bg-gray-800/40 ${p.id === selectedPoolId ? 'bg-emerald-500/5' : ''}`}
                      >
                        <td className="py-2 flex items-center gap-2">
                          <TokenIcon symbol={p.sym_a} size={18} /><TokenIcon symbol={p.sym_b} size={18} />
                          <span className="text-gray-200">{p.sym_a}/{p.sym_b}</span>
                        </td>
                        <td className="py-2 text-right text-gray-300 font-mono text-xs">
                          {fmt(fromBase(p.reserve_a), 2)} / {fmt(fromBase(p.reserve_b), 2)}
                        </td>
                        <td className="py-2 text-right text-gray-400">{(p.fee_bps / 100).toFixed(2)}%</td>
                        <td className="py-2 text-right text-gray-400 font-mono text-xs">{fmt(fromBase(p.lp_shares), 2)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              {tokens.length > 0 && (
                <p className="text-xs text-gray-500 mt-3">{tokens.length} token{tokens.length === 1 ? '' : 's'} registered on sigil-g0.</p>
              )}
            </div>

            <div className="bg-gray-900/80 border border-gray-800 rounded-2xl p-5 space-y-3">
              <h2 className="font-semibold text-gray-200 flex items-center gap-2"><Sparkles className="w-4 h-4 text-emerald-400" /> Add liquidity — {selectedPool ? `${selectedPool.sym_a}/${selectedPool.sym_b}` : '—'}</h2>
              <div className="grid grid-cols-2 gap-3">
                <div className="bg-gray-800/60 rounded-lg p-3">
                  <div className="text-xs text-gray-400 mb-1 flex items-center gap-1"><TokenIcon symbol={selectedPool?.sym_a || ''} size={16} />{selectedPool?.sym_a}</div>
                  <input
                    type="number" min="0" value={liqAmountAStr}
                    onChange={e => onLiqAmountAChange(e.target.value)}
                    placeholder="0.0"
                    className="w-full bg-transparent text-lg font-mono outline-none text-gray-100"
                  />
                </div>
                <div className="bg-gray-800/60 rounded-lg p-3">
                  <div className="text-xs text-gray-400 mb-1 flex items-center gap-1"><TokenIcon symbol={selectedPool?.sym_b || ''} size={16} />{selectedPool?.sym_b}</div>
                  <input
                    type="number" min="0" value={liqAmountBStr}
                    onChange={e => setLiqAmountBStr(e.target.value)}
                    placeholder="0.0"
                    className="w-full bg-transparent text-lg font-mono outline-none text-gray-100"
                  />
                </div>
              </div>
              {liqShareHint && (
                <p className="text-xs text-gray-500">Pool ratio ≈ 1 {selectedPool?.sym_a} : {fmt(liqShareHint, 6)} {selectedPool?.sym_b} — the {selectedPool?.sym_b} field auto-fills to match.</p>
              )}
              {liqError && (
                <div className="flex items-center gap-2 text-xs text-red-300 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2">
                  <AlertTriangle className="w-3.5 h-3.5 flex-shrink-0" /> {liqError}
                </div>
              )}
              {liqSuccess && (
                <div className="flex items-center gap-2 text-xs text-emerald-300 bg-emerald-500/10 border border-emerald-500/30 rounded-lg px-3 py-2">
                  <CheckCircle2 className="w-3.5 h-3.5 flex-shrink-0" /> {liqSuccess}
                </div>
              )}
              <button
                onClick={handleAddLiquidity}
                disabled={addingLiq || !selectedPool}
                className="w-full py-2.5 rounded-lg font-semibold bg-gray-800 hover:bg-gray-700 disabled:opacity-50 text-gray-200 transition-colors flex items-center justify-center gap-2"
              >
                {addingLiq ? <Loader2 className="w-4 h-4 animate-spin" /> : <Droplet className="w-4 h-4" />}
                {addingLiq ? 'Signing & submitting…' : 'Add liquidity'}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Honesty strip — what's live vs pending, straight from the source */}
      <div className="flex items-start gap-3 px-4 py-3 rounded-xl bg-gray-900/60 border border-gray-800 text-xs text-gray-400">
        <Info className="w-4 h-4 text-gray-500 flex-shrink-0 mt-0.5" />
        <div>
          <span className="text-gray-300 font-medium">What's real here:</span> swap and add-liquidity are signed, wallet-authorized
          transactions that settle through <code className="text-gray-400">sigil-state::commit_state_transition</code> on the sigil-g0
          testnet — the quote above uses the exact AMM formula the chain runs, so what you see is what settles.{' '}
          <span className="text-gray-300 font-medium">Still pending:</span> permissionless, VM-deployed pools — <code className="text-gray-400">sigil-vm::execute()</code>{' '}
          is a scaffold until the VM-1 wasmi runtime lands, so today's pools are natively registered by the node, not contract-created.
        </div>
      </div>

      <AnimatePresence>
        {successOpen && successData && (
          <SwapSuccessModal
            isOpen={successOpen}
            onClose={() => setSuccessOpen(false)}
            fromToken={successData.from}
            toToken={successData.to}
            fromAmount={successData.amtIn}
            toAmount={successData.amtOut}
          />
        )}
      </AnimatePresence>
    </div>
  );
}
