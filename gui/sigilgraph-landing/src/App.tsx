import { useState } from 'react';
import { deriveFromMnemonic, generatePhrase, type DerivedWallet } from './wallet';
import {
  IconSigil, IconCopy, IconCheck, IconSpark, IconLock, IconWarn,
  IconChain, IconRoots, IconSwap, IconCoin, IconBridge, IconSeal,
  IconDownload, IconChevron, IconGithub,
} from './icons';

// sigil-top release channel — same URLs the live signed manifest
// (sigilgraph.fluxapp.xyz/downloads/sigil-top-latest.json) resolves to,
// via the stable (unversioned) alias so this never goes stale on a release.
const DOWNLOADS = [
  { label: 'Linux x64', href: 'https://sigilgraph.fluxapp.xyz/downloads/sigil-top-linux-x64' },
  { label: 'Windows x64', href: 'https://sigilgraph.fluxapp.xyz/downloads/sigil-top-windows-x64.exe' },
];
const GITHUB_URL = 'https://github.com/deme-plata/sigilgraph';

// The real, live SIGIL wallet MCP installer for AI agents (Claude Code,
// Cursor, Codex, Grok…) — it's the shared Flux setup script under the hood
// (same binary, same install path as every other Flux tool), but what it
// actually hands the agent is the flux_sigil_wallet_*/flux_sigil_dex_* tool
// surface this page cares about. 2026-08-20: labeled below as the SIGIL
// wallet MCP install, not a generic "install Flux" pitch — a dedicated
// sigilgraph.org-only script was considered and deliberately skipped (this
// one is real and already works; a second one would just be two paths to
// maintain for the same tools).
const INSTALL_CMD = 'curl -fsSL https://sigilgraph.org/setup-sigil.sh | bash';

// 2026-08-23: the standalone sigilgraph.quillon.xyz/sigil-wallet/ app was
// pulled from this page (operator: "faulty wallet") — the in-page create/
// authenticate widget below (WalletCard) is real key derivation + a live
// balance lookup against this node, it just no longer links out anywhere.

function CopyInstallPill() {
  const [copied, setCopied] = useState(false);
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(INSTALL_CMD);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch {
      /* clipboard API unavailable — the pill still shows the command to copy by hand */
    }
  };
  return (
    <div className={`copy-pill${copied ? ' copied' : ''}`} onClick={onCopy} title="Copy the SIGIL wallet MCP install command — gives your AI agent flux_sigil_wallet_* tools">
      <span className="copy-pill-label">SIGIL wallet MCP</span>
      <code>{INSTALL_CMD}</code>
      <span className="copy-btn">{copied ? <IconCheck /> : <IconCopy />}</span>
    </div>
  );
}

function DownloadMenu() {
  const [open, setOpen] = useState(false);
  return (
    <div
      className={`download-menu${open ? ' open' : ''}`}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        className="btn btn-primary btn-download"
        onClick={() => setOpen(o => !o)}
        aria-haspopup="true"
        aria-expanded={open}
      >
        <IconDownload /> Download sigil-top <IconChevron className="chev" />
      </button>
      <div className="download-dropdown" role="menu">
        {DOWNLOADS.map(d => (
          <a key={d.label} href={d.href} role="menuitem">
            <span>{d.label}</span>
            <IconDownload width={13} height={13} />
          </a>
        ))}
        <a
          className="download-dropdown-sub"
          href="https://sigilgraph.fluxapp.xyz/downloads/sigil-top-latest.json"
          role="menuitem"
        >
          Signed release manifest ↗
        </a>
      </div>
    </div>
  );
}

function Topbar() {
  return (
    <header className="topbar">
      <div className="brand">
        <span className="mark"><IconSigil width={17} height={17} /></span>
        SIGIL<span style={{ color: 'var(--dim)', fontWeight: 600 }}>Graph</span>
      </div>
      <nav>
        <a href="#stack">Protocol</a>
        <a href="#honesty">Status</a>
      </nav>
      <div className="topbar-actions">
        <CopyInstallPill />
        <DownloadMenu />
        <a className="btn btn-github" href={GITHUB_URL} target="_blank" rel="noreferrer" title="deme-plata/sigilgraph on GitHub">
          <IconGithub />
        </a>
      </div>
    </header>
  );
}

type Status = { kind: 'idle' | 'pending' | 'ok' | 'err'; msg: string };

function WalletCard() {
  const [phrase, setPhrase] = useState('');
  const [revealed, setRevealed] = useState(false);
  const [wallet, setWallet] = useState<DerivedWallet | null>(null);
  const [balance, setBalance] = useState<string | null>(null);
  const [status, setStatus] = useState<Status>({ kind: 'idle', msg: '' });

  const doCreate = () => {
    const p = generatePhrase();
    setPhrase(p);
    setRevealed(true);
    const w = deriveFromMnemonic(p);
    setWallet(w);
    setBalance(null);
    void checkBalance(w);
  };

  const checkBalance = async (w: DerivedWallet) => {
    setStatus({ kind: 'pending', msg: 'Checking real balance on sigil-g0…' });
    try {
      const r = await fetch(`/v1/balance?wallet=${w.address}`, { cache: 'no-store' });
      const j = await r.json();
      if (j && j.ok && j.data) {
        const raw = BigInt(j.data.balance || '0');
        setBalance((Number(raw) / 1e8).toLocaleString(undefined, { maximumFractionDigits: 6 }));
        setStatus({ kind: 'ok', msg: 'Live balance confirmed against the real sigil-api node.' });
      } else {
        setStatus({ kind: 'err', msg: (j && j.error) || 'Node did not return a balance.' });
      }
    } catch {
      setStatus({ kind: 'err', msg: 'Could not reach the node — try again in a moment.' });
    }
  };

  // sigilgraph.org/sigil-wallet-tron-embedded.html is same-origin as this
  // 2026-08-23 fix: sigil-wallet-tron-embedded.html's own boot script gates
  // on localStorage['sigil-wallet-address'] (or a `?addr=` query param) — NOT
  // the mnemonic. Without an address it bounces to 'enter-sigil.html', which
  // doesn't exist in this static root, so the server's SPA fallback silently
  // re-served THIS landing page — the "authenticate just reopens sigilgraph.org"
  // bug. Passing `?addr=` satisfies that gate directly (no missing-page bounce
  // at all); the mnemonic still goes to localStorage so send/swap/bridge
  // signing works once inside (that part reads localStorage on demand, same
  // key the old comment described, and is same-origin/never-networked either way).
  const doAuthenticate = () => {
    if (!wallet) return;
    try { localStorage.setItem('sigil-wallet-mnemonic', phrase); } catch { /* private-mode storage block — signing will just prompt for the phrase instead */ }
    window.open(`/sigil-wallet-tron-embedded.html?addr=${wallet.address}`, '_blank', 'noreferrer');
  };

  return (
    <div className="wallet-card">
      <h3>Try SIGIL, right here</h3>
      <p className="sub">Real key derivation, real address, real testnet balance lookup — nothing simulated.</p>

      {!revealed ? (
        <button className="btn btn-primary" style={{ width: '100%', justifyContent: 'center' }} onClick={doCreate}>
          <IconSpark /> Generate a new wallet
        </button>
      ) : (
        <>
          <div className="field-label">Your recovery phrase — write it down, never share it</div>
          <div className="phrase-box">{phrase}</div>
          <div className="warn-line"><IconWarn /> This phrase alone controls the wallet. It never left your browser and this page never stores it.</div>
          <button className="btn btn-primary" style={{ width: '100%', justifyContent: 'center', marginTop: 12 }} onClick={doAuthenticate}>
            <IconLock /> Authenticate ↗
          </button>
        </>
      )}

      {wallet && (
        <div className="wallet-result">
          <div className="kv"><span className="k">Address</span><span className="v">{wallet.address}</span></div>
          {balance !== null && <div className="kv"><span className="k">Balance</span><span className="v">{balance} SIGIL (testnet)</span></div>}
        </div>
      )}
      {status.msg && <div className={`status-line ${status.kind === 'err' ? 'err' : status.kind === 'ok' ? 'ok' : 'pending'}`}>{status.msg}</div>}
    </div>
  );
}

function Hero() {
  return (
    <section className="hero">
      <div>
        <h1>A chain that <span className="grad">proves</span>, not asserts.</h1>
        <p className="lede">
          SIGIL Graph is DagKnight consensus with four state roots committed to every block —
          wallets, DEX pools, the event log, and contract storage — so divergence between nodes
          is provable, not something you have to trust an operator about. Native DEX. A
          fixed-buffer stablecoin. A live bridge to Polygon. All of it built the same way: read
          the real code first, verify from source, ship what's actually true.
        </p>
        <div className="badges">
          <span className="badge">DagKnight consensus</span>
          <span className="badge">4 committed state roots</span>
          <span className="badge">Post-quantum signing</span>
          <span className="badge">21M cap, code-enforced</span>
          <span className="badge">sigil-g0 · testnet</span>
        </div>
        <div className="hero-ctas">
          <a className="btn btn-ghost" href="#stack">See the protocol</a>
        </div>
      </div>
      <WalletCard />
    </section>
  );
}

function StatsStripe() {
  return (
    <div className="stripe">
      <div className="row">
        <div className="stat"><div className="num">21,000,000</div><div className="lbl">Hard SIGIL cap, compiled in</div></div>
        <div className="stat"><div className="num">4</div><div className="lbl">State roots committed per block</div></div>
        <div className="stat"><div className="num">105%</div><div className="lbl">USDS collateral buffer, always</div></div>
        <div className="stat"><div className="num">±20%</div><div className="lbl">Oracle sanity band per push</div></div>
        <div className="stat"><div className="num">0.30%</div><div className="lbl">Protocol fee, shared by DEX + USDS</div></div>
      </div>
    </div>
  );
}

function Stack() {
  const items = [
    { Ic: IconChain, h: 'DagKnight consensus', p: 'Blocks reference multiple parents; a real DAG order, not a single-chain race — with a settled spine that finalizes deterministically.' },
    { Ic: IconRoots, h: 'Four state roots, every block', p: 'Wallet balances, DEX pools, the event log, and contract storage each get their own committed Merkle root — every node can prove, not assert, that it agrees.' },
    { Ic: IconSwap, h: 'A real native DEX', p: 'Constant-product pools, real swaps, real LP shares — authenticated wallet-signed swaps queued straight into the block producer, no side ledger.' },
    { Ic: IconCoin, h: 'USDS — a fixed-buffer stablecoin', p: '105% collateral, always, by construction — no liquidation engine to get wrong. A direct response to a real oracle bug found on a sister chain (see Status below).' },
    { Ic: IconBridge, h: 'Live Polygon bridges', p: 'Wrapped SIGIL and wrapped USDS on Polygon, mint-on-verified-lock, zero pre-mint, isolated vaults so one bridge can never touch the other\'s funds.' },
    { Ic: IconSeal, h: 'Provenance-signed binaries', p: 'Every release binary carries a signed proof binding the artifact hash, source hash, and the agent that built it — verifiable, not asserted.' },
  ];
  return (
    <section className="section" id="stack">
      <div className="kicker">The protocol</div>
      <h2>Built to be checked, not just believed</h2>
      <p className="desc">Every piece below exists because something on a sister chain broke in a way that was preventable — each is a specific, verifiable answer to a specific, real failure.</p>
      <div className="grid3">
        {items.map((it) => (
          <div className="card" key={it.h}>
            <span className="ic"><it.Ic /></span>
            <h4>{it.h}</h4>
            <p>{it.p}</p>
          </div>
        ))}
      </div>
    </section>
  );
}

function Honesty() {
  return (
    <section className="section" id="honesty">
      <div className="honesty">
        <div className="kicker">Status — stated plainly, not buried</div>
        <p style={{ margin: '4px 0 0', color: 'var(--text)', fontWeight: 600 }}>
          SIGIL Graph runs today as <code className="mono">sigil-g0</code>, a public testnet. Nothing on this chain represents real monetary value yet.
        </p>
        <ul>
          <li>Single-authority price oracle — one wallet pushes SIGIL's USD price; a ±20% sanity band bounds one bad push, but the authority itself is still a trust assumption, not yet decentralized.</li>
          <li>USDS carries no liquidation engine by design — a fixed 105% buffer instead, a deliberate trade explained in the USDS technical review.</li>
          <li>Circulating USDS supply isn't indexed yet — the live status endpoint reports price and vault collateral, both real, not yet total supply.</li>
          <li>A real SIGIL mainnet does not exist yet. When it does, bridged tokens get redeployed fresh against it rather than migrated — clean books, not a fee event.</li>
        </ul>
      </div>
    </section>
  );
}

function Footer() {
  return (
    <footer>
      <div>
        <a href="https://quillon.xyz/" target="_blank" rel="noreferrer">Quillon Graph</a>
      </div>
      <div style={{ marginTop: 10 }}>SIGIL Graph — verify, don't trust.</div>
    </footer>
  );
}

export default function App() {
  return (
    <>
      <Topbar />
      <Hero />
      <StatsStripe />
      <Stack />
      <Honesty />
      <Footer />
    </>
  );
}
