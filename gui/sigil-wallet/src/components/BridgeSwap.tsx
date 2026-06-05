// BridgeSwap — SigilGraph wallet's window to the rest of crypto, powered by flux-0x (0x Swap v2 +
// Cross-Chain incl. Solana). The 0x key stays SERVER-SIDE: this screen only calls the gateway
// (/v1/0x/*). Read-only quote preview — signing/broadcast is the gated next phase.
import { useMemo, useState } from 'react';
import type { CSSProperties } from 'react';
import { Repeat, ArrowRight, Loader2, AlertTriangle, ShieldCheck, Info } from 'lucide-react';

const GW = 'https://sigilgraph.quillon.xyz:8447';
const CYAN = '#22d3ee';

const CHAINS = [
  { id: 1, name: 'Ethereum' }, { id: 8453, name: 'Base' }, { id: 42161, name: 'Arbitrum' },
  { id: 137, name: 'Polygon' }, { id: 10, name: 'Optimism' }, { id: 56, name: 'BNB Chain' },
  { id: 43114, name: 'Avalanche' }, { id: 999999999991, name: 'Solana' },
];
const isSolana = (id: number) => id === 999999999991;

type Tok = { label: string; address: string; dec: number };
const TOKENS: Record<number, Tok[]> = {
  1:   [{ label: 'ETH', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'WETH', address: '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2', dec: 18 }, { label: 'USDC', address: '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48', dec: 6 }],
  8453:[{ label: 'ETH', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913', dec: 6 }],
  42161:[{ label: 'ETH', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0xaf88d065e77c8cC2239327C5EDb3A432268e5831', dec: 6 }],
  137: [{ label: 'POL', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0x3c499c542cEf5e3811e1192cE70d8cC03d5c3359', dec: 6 }],
  10:  [{ label: 'ETH', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85', dec: 6 }],
  56:  [{ label: 'BNB', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d', dec: 18 }],
  43114:[{ label: 'AVAX', address: '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE', dec: 18 }, { label: 'USDC', address: '0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E', dec: 6 }],
  999999999991:[{ label: 'SOL', address: 'So11111111111111111111111111111111111111112', dec: 9 }, { label: 'USDC', address: 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', dec: 6 }],
};

// human amount → base units, as an integer string (no float precision loss)
function toBaseUnits(amt: string, dec: number): string {
  if (!amt || isNaN(Number(amt))) return '';
  const [w, f = ''] = amt.replace(/,/g, '.').split('.');
  const frac = (f + '0'.repeat(dec)).slice(0, dec);
  return ((w || '0') + frac).replace(/^0+(?=\d)/, '') || '0';
}
function fromBaseUnits(base: string, dec: number): string {
  if (!base) return '0';
  const s = base.padStart(dec + 1, '0');
  const w = s.slice(0, s.length - dec), f = s.slice(s.length - dec).replace(/0+$/, '');
  return f ? `${w}.${f}` : w;
}

export default function BridgeSwap() {
  const [fromChain, setFromChain] = useState(1);
  const [toChain, setToChain] = useState(999999999991);
  const [sellTok, setSellTok] = useState(TOKENS[1][2].address);  // USDC
  const [buyTok, setBuyTok] = useState(TOKENS[999999999991][1].address); // USDC (Solana)
  const [amount, setAmount] = useState('10');
  const [taker, setTaker] = useState('');
  const [destAddr, setDestAddr] = useState('');
  const [sort, setSort] = useState<'price' | 'speed'>('price');
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<any>(null);
  const [error, setError] = useState('');

  const crossChain = fromChain !== toChain;
  const sellMeta = useMemo(() => (TOKENS[fromChain] || []).find(t => t.address === sellTok), [fromChain, sellTok]);
  const buyMeta = useMemo(() => (TOKENS[toChain] || []).find(t => t.address === buyTok), [toChain, buyTok]);
  const needDestAddr = crossChain && isSolana(toChain);

  const pickChain = (which: 'from' | 'to', id: number) => {
    if (which === 'from') { setFromChain(id); setSellTok((TOKENS[id]?.[0]) ? TOKENS[id][TOKENS[id].length - 1].address : ''); }
    else { setToChain(id); setBuyTok((TOKENS[id]?.[0]) ? TOKENS[id][TOKENS[id].length - 1].address : ''); }
    setResult(null); setError('');
  };

  async function getQuote() {
    setError(''); setResult(null);
    const dec = sellMeta?.dec ?? 18;
    const base = toBaseUnits(amount, dec);
    if (!base || base === '0') { setError('Enter an amount > 0'); return; }
    if (!sellTok || !buyTok) { setError('Pick both tokens'); return; }
    if (crossChain && !taker) { setError('Cross-chain needs your sender (origin) address'); return; }
    if (needDestAddr && !destAddr) { setError('Sending to Solana needs a destination (Solana) address'); return; }
    setLoading(true);
    try {
      let url: string;
      if (!crossChain) {
        url = `${GW}/v1/0x/price?chainId=${fromChain}&sell=${sellTok}&buy=${buyTok}&amount=${base}`;
      } else {
        url = `${GW}/v1/0x/xquote?originChain=${fromChain}&destChain=${toChain}&sell=${sellTok}&buy=${buyTok}&amount=${base}&taker=${encodeURIComponent(taker)}&destAddr=${encodeURIComponent(destAddr)}&sort=${sort}`;
      }
      const r = await fetch(url);
      const j = await r.json();
      if (!r.ok || j.error) { setError(String(j.error || `HTTP ${r.status}`)); }
      else setResult(j);
    } catch (e: any) { setError('Gateway unreachable: ' + (e?.message || e)); }
    finally { setLoading(false); }
  }

  const panel: CSSProperties = { background: 'rgba(15,23,42,0.6)', border: '1px solid rgba(34,211,238,0.25)', borderRadius: 14, padding: 16 };
  const input: CSSProperties = { width: '100%', background: 'rgba(2,6,23,0.7)', border: '1px solid rgba(34,211,238,0.3)', borderRadius: 10, color: '#e2e8f0', padding: '10px 12px', fontSize: 14, outline: 'none' };
  const sel: CSSProperties = { ...input, appearance: 'none' };

  const ChainSel = ({ value, onPick }: { value: number; onPick: (id: number) => void }) => (
    <select style={sel} value={value} onChange={e => onPick(Number(e.target.value))}>
      {CHAINS.map(c => <option key={c.id} value={c.id}>{c.name}{isSolana(c.id) ? ' ◎' : ''}</option>)}
    </select>
  );
  const TokSel = ({ chain, value, onChange }: { chain: number; value: string; onChange: (a: string) => void }) => (
    <select style={sel} value={value} onChange={e => { onChange(e.target.value); setResult(null); }}>
      {(TOKENS[chain] || []).map(t => <option key={t.address} value={t.address}>{t.label}</option>)}
      {!(TOKENS[chain] || []).length && <option value="">— no presets —</option>}
    </select>
  );

  return (
    <div style={{ maxWidth: 560, margin: '0 auto', padding: 20, color: '#e2e8f0' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 6 }}>
        <Repeat size={22} color={CYAN} />
        <h2 style={{ fontSize: 22, fontWeight: 700, color: CYAN, margin: 0 }}>Bridge &amp; Swap</h2>
      </div>
      <div style={{ fontSize: 12, color: '#94a3b8', marginBottom: 16 }}>
        Powered by <b style={{ color: CYAN }}>flux-0x</b> — 0x Swap v2 + Cross-Chain across 25+ chains incl. Solana. Key stays server-side.
      </div>

      <div style={{ ...panel, marginBottom: 14 }}>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
          <div>
            <label style={{ fontSize: 11, color: '#94a3b8' }}>From</label>
            <ChainSel value={fromChain} onPick={id => pickChain('from', id)} />
            <div style={{ height: 8 }} />
            <TokSel chain={fromChain} value={sellTok} onChange={setSellTok} />
          </div>
          <div>
            <label style={{ fontSize: 11, color: '#94a3b8' }}>To {crossChain && <span style={{ color: CYAN }}>· bridge</span>}</label>
            <ChainSel value={toChain} onPick={id => pickChain('to', id)} />
            <div style={{ height: 8 }} />
            <TokSel chain={toChain} value={buyTok} onChange={setBuyTok} />
          </div>
        </div>

        <div style={{ marginTop: 12 }}>
          <label style={{ fontSize: 11, color: '#94a3b8' }}>Amount ({sellMeta?.label || 'token'})</label>
          <input style={input} value={amount} onChange={e => { setAmount(e.target.value); setResult(null); }} inputMode="decimal" placeholder="0.0" />
        </div>

        {crossChain && (
          <div style={{ marginTop: 12 }}>
            <label style={{ fontSize: 11, color: '#94a3b8' }}>Your sending address (origin {CHAINS.find(c => c.id === fromChain)?.name})</label>
            <input style={input} value={taker} onChange={e => setTaker(e.target.value)} placeholder="0x… (or your EVM address)" />
          </div>
        )}
        {needDestAddr && (
          <div style={{ marginTop: 12 }}>
            <label style={{ fontSize: 11, color: CYAN }}>Destination Solana address (required ◎)</label>
            <input style={input} value={destAddr} onChange={e => setDestAddr(e.target.value)} placeholder="Solana wallet address" />
          </div>
        )}
        {crossChain && (
          <div style={{ marginTop: 12, display: 'flex', gap: 8, alignItems: 'center' }}>
            <span style={{ fontSize: 11, color: '#94a3b8' }}>Optimize for</span>
            {(['price', 'speed'] as const).map(s => (
              <button key={s} onClick={() => setSort(s)} style={{ ...input, width: 'auto', padding: '6px 14px', cursor: 'pointer', borderColor: sort === s ? CYAN : 'rgba(34,211,238,0.3)', color: sort === s ? CYAN : '#94a3b8', fontWeight: sort === s ? 700 : 400 }}>{s}</button>
            ))}
          </div>
        )}

        <button onClick={getQuote} disabled={loading}
          style={{ marginTop: 16, width: '100%', padding: '12px', borderRadius: 10, border: 'none', cursor: loading ? 'wait' : 'pointer',
            background: `linear-gradient(135deg, ${CYAN}, #0ea5b7)`, color: '#04222a', fontWeight: 800, fontSize: 15, display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 8 }}>
          {loading ? <Loader2 size={18} className="animate-spin" /> : <ArrowRight size={18} />}
          {crossChain ? 'Get bridge routes' : 'Get swap price'}
        </button>
      </div>

      {error && (
        <div style={{ ...panel, borderColor: 'rgba(248,113,113,0.5)', display: 'flex', gap: 10, alignItems: 'flex-start', marginBottom: 14 }}>
          <AlertTriangle size={18} color="#f87171" style={{ flexShrink: 0, marginTop: 2 }} />
          <div style={{ fontSize: 13, color: '#fca5a5' }}>{error}</div>
        </div>
      )}

      {result && !crossChain && result.buyAmount && (
        <div style={{ ...panel, marginBottom: 14 }}>
          <div style={{ fontSize: 12, color: '#94a3b8' }}>You would receive ~</div>
          <div style={{ fontSize: 28, fontWeight: 800, color: CYAN }}>
            {fromBaseUnits(String(result.buyAmount), buyMeta?.dec ?? 18)} <span style={{ fontSize: 16, color: '#cbd5e1' }}>{buyMeta?.label}</span>
          </div>
          <div style={{ fontSize: 11, color: result.liquidityAvailable ? '#34d399' : '#f87171' }}>
            {result.liquidityAvailable ? '● liquidity available' : '○ no liquidity for this pair/size'}
          </div>
        </div>
      )}

      {result && crossChain && Array.isArray(result.quotes) && (
        <div style={{ ...panel, marginBottom: 14 }}>
          <div style={{ fontSize: 13, color: CYAN, fontWeight: 700, marginBottom: 8 }}>{result.quotes.length} bridge route{result.quotes.length === 1 ? '' : 's'} found</div>
          {result.quotes.slice(0, 3).map((q: any, i: number) => {
            const out = q.buyAmount || q.minBuyAmount || q.toAmount;
            const eta = q.estimatedFillTimeSec || q.estimatedTime || q.eta;
            const bridge = q.bridge || q.provider || (q.steps && q.steps[0] && q.steps[0].bridge) || 'route';
            return (
              <div key={i} style={{ display: 'flex', justifyContent: 'space-between', padding: '8px 0', borderTop: i ? '1px solid rgba(34,211,238,0.15)' : 'none' }}>
                <div style={{ fontSize: 13 }}>{i === 0 ? '⭐ ' : ''}{bridge}</div>
                <div style={{ fontSize: 13, textAlign: 'right' }}>
                  {out ? <span style={{ color: CYAN, fontWeight: 700 }}>{fromBaseUnits(String(out), buyMeta?.dec ?? 6)} {buyMeta?.label}</span> : '—'}
                  {eta ? <span style={{ color: '#94a3b8' }}> · ~{eta}s</span> : ''}
                </div>
              </div>
            );
          })}
        </div>
      )}

      <div style={{ ...panel, display: 'flex', gap: 10, alignItems: 'flex-start' }}>
        <Info size={16} color={CYAN} style={{ flexShrink: 0, marginTop: 2 }} />
        <div style={{ fontSize: 12, color: '#94a3b8' }}>
          This is a live <b style={{ color: '#cbd5e1' }}>quote preview</b>. Signing &amp; broadcasting the swap is the gated next phase.
          <span style={{ display: 'block', marginTop: 4 }}><ShieldCheck size={12} color="#34d399" style={{ verticalAlign: 'middle' }} /> Native <b style={{ color: CYAN }}>SIGIL</b> stays your home coin — this is the window out to EVM &amp; Solana.</span>
        </div>
      </div>
    </div>
  );
}
