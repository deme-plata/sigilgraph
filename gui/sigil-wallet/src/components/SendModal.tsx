// SendModal.tsx — production mobile-first Send modal for the SIGIL wallet.
//
// Wires the REAL send path (not the main.tsx preview):
//   signTransactionForP2P({from,to,amount,memo})  →  useLibP2P().submitTransaction(signed)
//   with an HTTP fallback via qnkAPI.sendTransaction when P2P has no peers.
//
// Mobile: slides up as a bottom sheet. Desktop: centered card. Cyan Flux theme
// (--theme-accent #22d3ee). Matches the repo's modal conventions: framer-motion +
// lucide-react + createPortal.
import { useState, useEffect, useCallback, useRef } from 'react';
import { createPortal } from 'react-dom';
import { motion, AnimatePresence } from 'framer-motion';
import { X, ArrowUp, Loader2, CheckCircle2, AlertCircle, Wifi, WifiOff } from 'lucide-react';
import { useLibP2P } from '../contexts/LibP2PContext';
import { signTransactionForP2P } from '../services/walletAuth';
import { qnkAPI } from '../services/api';
import type { SignedTransaction } from '../libp2p/types';
import './SendModal.css';

const FEE_SGL = 0.001;

export interface SendModalProps {
  isOpen: boolean;
  onClose: () => void;
  /** Live spendable balance in SGL. */
  balance: number;
  /** Sender address (sgl1… / 64-hex). Falls back to localStorage. */
  walletAddress?: string;
  /** Called after a confirmed send so the parent can refresh balance/activity. */
  onSent?: (info: { txHash: string; amount: number; to: string; peers: number }) => void;
}

type Phase = 'idle' | 'signing' | 'submitting' | 'success' | 'error';

const short = (a: string) => (a.length > 18 ? `${a.slice(0, 10)}…${a.slice(-6)}` : a);

export default function SendModal({ isOpen, onClose, balance, walletAddress, onSent }: SendModalProps) {
  const { isReady: p2pReady, peerCount, submitTransaction } = useLibP2P();
  const from = walletAddress || localStorage.getItem('walletAddress') || '';

  const [to, setTo] = useState('');
  const [amount, setAmount] = useState('');
  const [memo, setMemo] = useState('');
  const [phase, setPhase] = useState<Phase>('idle');
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ txHash: string; peers: number } | null>(null);
  const addrRef = useRef<HTMLInputElement>(null);

  // reset + autofocus on open
  useEffect(() => {
    if (isOpen) {
      setTo(''); setAmount(''); setMemo(''); setPhase('idle'); setError(null); setResult(null);
      const t = setTimeout(() => addrRef.current?.focus(), 140);
      return () => clearTimeout(t);
    }
  }, [isOpen]);

  // Esc to close (only when not mid-flight)
  useEffect(() => {
    if (!isOpen) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape' && phase !== 'signing' && phase !== 'submitting') onClose(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [isOpen, phase, onClose]);

  const busy = phase === 'signing' || phase === 'submitting';
  const amt = parseFloat(amount || '0');
  const total = (isFinite(amt) ? amt : 0) + FEE_SGL;

  const validate = useCallback((): string | null => {
    const a = to.trim();
    if (!a || a.length < 12) return 'Enter a valid recipient address.';
    if (!/^(sgl1|[0-9a-fA-F]{64})/.test(a)) return 'Address must start with sgl1 or be 64-hex.';
    if (!isFinite(amt) || amt <= 0) return 'Enter a positive amount.';
    if (total > balance) return `Insufficient balance — need ${total.toFixed(4)} SGL (incl. ${FEE_SGL} fee), have ${balance.toFixed(4)}.`;
    return null;
  }, [to, amt, total, balance]);

  const setMax = () => setAmount(Math.max(0, +(balance - FEE_SGL).toFixed(4)).toString());

  const handleSend = async () => {
    const v = validate();
    if (v) { setError(v); return; }
    setError(null);
    const recipient = to.trim();
    try {
      // 1) sign locally (Ed25519 + optional Dilithium5)
      setPhase('signing');
      const signed = await signTransactionForP2P({
        from,
        to: recipient,
        amount: amt,
        memo: memo.trim() || undefined,
      });
      if (!signed.success || !signed.transaction) {
        throw new Error(signed.error || 'Signing failed — please re-unlock your wallet.');
      }

      // 2) submit — P2P gossip first when peers exist, else HTTP fallback
      setPhase('submitting');
      let txHash = ''; let peers = 0;
      if (p2pReady && peerCount > 0) {
        const r = await submitTransaction(signed.transaction as SignedTransaction);
        if (r?.success) { txHash = r.txHash || ''; peers = r.peerCount || 0; }
        else if (!canHttp()) throw new Error(r?.error || 'P2P broadcast reached 0 peers.');
      }
      if (!txHash && canHttp()) {
        const h = await httpFallback(from, recipient, amt, memo.trim());
        if (!h.ok) throw new Error(h.error || 'Transaction was not accepted by the network.');
        txHash = h.txHash;
      }
      if (!txHash) throw new Error('No P2P peers and HTTP fallback unavailable — try again in a moment.');

      setResult({ txHash, peers });
      setPhase('success');
      onSent?.({ txHash, amount: amt, to: recipient, peers });
      setTimeout(() => { if (isOpen) onClose(); }, 1600);
    } catch (e: any) {
      setError(e?.message || 'Send failed.');
      setPhase('error');
    }
  };

  if (!isOpen) return null;

  return createPortal(
    <AnimatePresence>
      <motion.div
        className="sm-backdrop"
        initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
        onClick={() => !busy && onClose()}
      >
        <motion.div
          className="sm-sheet"
          role="dialog" aria-modal="true" aria-label="Send SGL"
          initial={{ y: 60, opacity: 0, scale: 0.98 }}
          animate={{ y: 0, opacity: 1, scale: 1 }}
          exit={{ y: 60, opacity: 0 }}
          transition={{ type: 'spring', stiffness: 380, damping: 32 }}
          onClick={(e) => e.stopPropagation()}
        >
          <div className="sm-head">
            <span className="sm-title"><ArrowUp size={16} /> Send SGL</span>
            <span className={`sm-net ${p2pReady && peerCount > 0 ? 'on' : 'off'}`}>
              {p2pReady && peerCount > 0 ? <Wifi size={13} /> : <WifiOff size={13} />}
              {p2pReady && peerCount > 0 ? `${peerCount} peer${peerCount === 1 ? '' : 's'}` : 'HTTP'}
            </span>
            <button className="sm-x" onClick={onClose} disabled={busy} aria-label="Close"><X size={18} /></button>
          </div>

          {phase === 'success' && result ? (
            <div className="sm-success">
              <CheckCircle2 size={44} />
              <div className="sm-success-t">Sent {amt} SGL</div>
              <div className="sm-success-s">
                to {short(to.trim())}{result.peers > 0 ? ` · ${result.peers} peer${result.peers === 1 ? '' : 's'}` : ''}
              </div>
              {result.txHash && <code className="sm-hash">{short(result.txHash)}</code>}
            </div>
          ) : (
            <>
              <label className="sm-label">Recipient</label>
              <input
                ref={addrRef} className="sm-input" inputMode="text" autoComplete="off" spellCheck={false}
                placeholder="sgl1…" value={to} disabled={busy}
                onChange={(e) => { setTo(e.target.value); setError(null); }}
              />

              <label className="sm-label">Amount</label>
              <div className="sm-amt-wrap">
                <input
                  className="sm-input" type="number" min="0" step="0.0001" inputMode="decimal"
                  placeholder="0.00" value={amount} disabled={busy}
                  onChange={(e) => { setAmount(e.target.value); setError(null); }}
                />
                <button className="sm-max" onClick={setMax} disabled={busy}>MAX</button>
              </div>
              <div className="sm-balrow">
                <span>balance <b>{balance.toFixed(4)}</b> SGL · fee ≈ {FEE_SGL}</span>
                {amt > 0 && <span className="sm-total">total {total.toFixed(4)}</span>}
              </div>

              <label className="sm-label">Memo <span className="sm-opt">(optional)</span></label>
              <input
                className="sm-input" maxLength={120} placeholder="thanks for the slice"
                value={memo} disabled={busy} onChange={(e) => setMemo(e.target.value)}
              />

              {error && <div className="sm-err"><AlertCircle size={14} /> {error}</div>}

              <div className="sm-actions">
                <button className="sm-ghost" onClick={onClose} disabled={busy}>Cancel</button>
                <button className="sm-prim" onClick={handleSend} disabled={busy}>
                  {phase === 'signing' && <><Loader2 size={15} className="sm-spin" /> Signing…</>}
                  {phase === 'submitting' && <><Loader2 size={15} className="sm-spin" /> Broadcasting…</>}
                  {(phase === 'idle' || phase === 'error') && <>Sign &amp; Send</>}
                </button>
              </div>
              <div className="sm-hint">Ed25519 signed locally · {p2pReady && peerCount > 0 ? 'libp2p gossip' : 'HTTP submit'} · ~12s finality</div>
            </>
          )}
        </motion.div>
      </motion.div>
    </AnimatePresence>,
    document.body
  );
}

// HTTP fallback helpers — guarded so the component compiles even if the API
// surface differs; degrades to a clear error instead of a hard crash.
function canHttp(): boolean {
  return typeof (qnkAPI as any)?.sendTransaction === 'function';
}
async function httpFallback(from: string, to: string, amount: number, memo: string): Promise<{ ok: boolean; txHash: string; error?: string }> {
  try {
    const r: any = await (qnkAPI as any).sendTransaction(from, to, amount, memo || undefined, 'SGL');
    if (r?.success && r?.data) {
      return { ok: true, txHash: r.data.transaction_id || r.data.transaction_hash || r.data.tx_hash || '' };
    }
    return { ok: false, txHash: '', error: r?.error || 'HTTP submit rejected.' };
  } catch (e: any) {
    return { ok: false, txHash: '', error: e?.message || 'HTTP submit error.' };
  }
}
