/* ──────────────────────────────────────────────────────────────────────────
   sigil-metamask.js — SIGILGRAPH ⇄ MetaMask / Polygon integration.
   2026-08-26. Plain classic script (no bundler, no ESM, no CDN): defines
   window.SigilMM. Loaded by enter-sigil.html (create-wallet-with-MetaMask)
   and sigil-wallet-tron.html (connect button + cross-chain bridge modal).

   ── Everything here was verified against the LIVE deployment, not assumed ──

   Wrapped-SIGIL ERC-20, Polygon mainnet (chain 137):
     0xc224602C32F5c7f68d3Ef002aE4C99e4C7Df25B7
   Verified live 2026-08-26 by eth_call against polygon-bor-rpc.publicnode.com:
     symbol()   -> "SIGIL"
     decimals() -> 18        (0x12)

   The RETURN leg (Polygon -> native SIGIL) calls, on that contract:
     burn(uint256 amount, bytes32 destSigilAddress)   selector 0xbcf64e05
   Confirmed by fetching the deployed runtime bytecode via eth_getCode and
   matching function selectors: `burn(uint256,bytes32)` (0xbcf64e05) IS
   present; the reversed-argument `burn(bytes32,uint256)` (0x7a408454) and
   `burn(uint256,address)` (0xfcd3533c) are BOTH absent — so the argument
   order below is not a guess. The BurnedTo topic hash
   (0x95d8284568...c9b79785) is likewise present in the bytecode, i.e. the
   function really does emit the event the relayer watches for. The keccak
   used to derive those selectors was self-tested against the canonical
   ERC-20 Transfer topic hash first.

   That event is consumed by crates/sigil-relayer (systemd:
   sigil-bridge-relayer.service, confirmed ACTIVE 2026-08-26), which decodes
     event BurnedTo(address indexed from, uint256 amount, bytes32 indexed destSigilAddress)
   and submits a relayer-signed POST /v1/bridge/unlock to sigil-api, releasing
   native SIGIL from the bridge vault. Burns are deduped by tx hash in the
   node's `processed_burns` set, so the same burn can never unlock twice.

   Decimals: native SIGIL is 8dp, wrapped SIGIL is 18dp, so the bridge's one
   conversion factor is 10^10 (sigil-relayer::DECIMAL_SHIFT). The relayer
   FLOORS any burn that is not a clean multiple of 10^10 and that dust is
   unrecoverable — so every amount this file sends is forced to a clean
   multiple by capping input at 8 decimal places. Never relax that.

   Polygon's native gas token is POL (renamed from MATIC); wallet_addEthereumChain
   below uses POL deliberately.
   ────────────────────────────────────────────────────────────────────────── */
(function (root) {
  'use strict';

  var POLYGON_PARAMS = {
    chainId: '0x89',                       // 137
    chainName: 'Polygon Mainnet',
    nativeCurrency: { name: 'POL', symbol: 'POL', decimals: 18 },
    rpcUrls: ['https://polygon-rpc.com', 'https://polygon-bor-rpc.publicnode.com'],
    blockExplorerUrls: ['https://polygonscan.com']
  };

  var TOKEN = {
    address: '0xc224602C32F5c7f68d3Ef002aE4C99e4C7Df25B7',
    symbol: 'SIGIL',
    decimals: 18
  };

  // Read-only fallbacks, same order/endpoints as sigil-api's Chain::Polygon
  // public_fallbacks() — used when no injected wallet is present so the
  // balance card still works for a logged-out visitor.
  var PUBLIC_RPCS = ['https://polygon-bor-rpc.publicnode.com', 'https://1rpc.io/matic'];

  var SEL_BURN      = '0xbcf64e05'; // burn(uint256,bytes32)
  // Set to true ONLY when sigil-bridge-relayer is live and watching BurnedTo events.
  // See the long note in burnToSigil() for why this exists.
  var BRIDGE_BURN_ENABLED = false;
  var SEL_BALANCEOF = '0x70a08231'; // balanceOf(address)

  var DECIMAL_SHIFT = 10000000000n; // 10^10 — native 8dp -> wrapped 18dp

  // ── provider discovery: EIP-6963 first (MetaMask's own recommendation),
  // legacy window.ethereum as fallback. Pre-6963 the LAST extension to inject
  // won window.ethereum, so with several wallets installed you could silently
  // drive the wrong one; 6963 makes each announce itself instead.
  var providers = [];   // [{info:{uuid,name,rdns,icon}, provider}]
  var announced = {};

  function onAnnounce(ev) {
    try {
      var d = ev.detail; if (!d || !d.provider || !d.info) return;
      var key = d.info.uuid || d.info.rdns || d.info.name;
      if (announced[key]) return;
      announced[key] = true;
      providers.push({ info: d.info, provider: d.provider });
    } catch (e) { /* a malformed announce must never break the page */ }
  }

  if (root.addEventListener) {
    root.addEventListener('eip6963:announceProvider', onAnnounce);
    try { root.dispatchEvent(new Event('eip6963:requestProvider')); } catch (e) {}
  }

  function legacyProviders() {
    var out = [], eth = root.ethereum;
    if (!eth) return out;
    if (Array.isArray(eth.providers)) {
      eth.providers.forEach(function (p) {
        out.push({ info: { name: p.isMetaMask ? 'MetaMask' : 'Injected wallet', rdns: 'legacy', uuid: 'legacy-' + out.length }, provider: p });
      });
    } else {
      out.push({ info: { name: eth.isMetaMask ? 'MetaMask' : 'Injected wallet', rdns: 'legacy', uuid: 'legacy-0' }, provider: eth });
    }
    return out;
  }

  function list() {
    // Re-request each time: extensions can announce late (slow startup, or the
    // user installs/enables one while the page is open).
    try { root.dispatchEvent(new Event('eip6963:requestProvider')); } catch (e) {}
    if (providers.length) return providers.slice();
    return legacyProviders();
  }

  function pick() {
    var all = list();
    if (!all.length) return null;
    for (var i = 0; i < all.length; i++) {
      var n = (all[i].info.rdns || '') + ' ' + (all[i].info.name || '');
      if (/metamask/i.test(n)) return all[i];        // prefer MetaMask when present
    }
    return all[0];
  }

  var state = { provider: null, account: null, chainId: null, label: null, pending: null };
  var listeners = {};

  function emit(name, payload) {
    (listeners[name] || []).forEach(function (fn) { try { fn(payload); } catch (e) {} });
  }

  function on(name, fn) { (listeners[name] = listeners[name] || []).push(fn); }

  function available() { return !!pick(); }

  function req(method, params) {
    if (!state.provider) throw new Error('No wallet connected');
    return state.provider.request({ method: method, params: params || [] });
  }

  function bind(p) {
    if (!p || p.__sigilBound) return;
    p.__sigilBound = true;
    try {
      p.on('accountsChanged', function (accts) {
        state.account = (accts && accts[0]) ? accts[0].toLowerCase() : null;
        if (!state.account) { state.provider = null; try { localStorage.removeItem('sigil-mm-connected'); } catch (e) {} }
        emit('change', getState());
      });
      p.on('chainChanged', function (cid) {
        state.chainId = cid;
        emit('change', getState());
      });
    } catch (e) { /* some injected providers expose no .on — non-fatal */ }
  }

  function getState() {
    return {
      connected: !!state.account,
      account: state.account,
      chainId: state.chainId,
      onPolygon: state.chainId === POLYGON_PARAMS.chainId,
      label: state.label
    };
  }

  /* ── The "already pending" trap ────────────────────────────────────────
     MetaMask keeps at most ONE permission request per origin, and that
     request OUTLIVES the page. Dismissing the popup — clicking away, closing
     the notification window, or having MetaMask locked when it fires —
     neither approves nor rejects it; it just hides. Any later
     eth_requestAccounts then rejects with

       code -32002  "Request of type 'wallet_requestPermissions' already
                     pending for origin https://sigilgraph.org. Please wait."

     which is a true statement the user cannot act on as written: nothing on
     the page is pending, the popup they must answer is hidden behind the
     extension icon. Three guards, in order of how much they help:

       1. eth_accounts FIRST — never prompts, so a returning user who already
          authorised this site never opens a request at all.
       2. in-flight dedup — this page cannot queue a second request over its
          own first one.
       3. on -32002, poll eth_accounts until the popup that is ALREADY open
          gets approved, so the page recovers by itself instead of dead-ending
          on an error string.
     ─────────────────────────────────────────────────────────────────────── */
  var PENDING_MSG =
    'MetaMask already has a connection request open for this site. Click the ' +
    'MetaMask fox in your browser toolbar and approve it — this page then ' +
    'continues on its own. (If no popup appears, unlock MetaMask first.)';

  function isPendingErr(e) {
    var c = e && (e.code != null ? e.code
              : (e.data && e.data.originalError && e.data.originalError.code));
    if (c === -32002) return true;
    return /already pending/i.test(String((e && e.message) || ''));
  }

  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }

  /** Wait for a popup the user already has open. Resolves to accounts, or []. */
  function awaitPendingApproval(onWait, timeoutMs) {
    if (onWait) { try { onWait(PENDING_MSG); } catch (e) {} }
    var deadline = Date.now() + (timeoutMs || 180000);
    return (function poll() {
      if (Date.now() >= deadline) return Promise.resolve([]);
      return sleep(1000).then(function () {
        return req('eth_accounts').then(function (a) {
          return (a && a.length) ? a : poll();
        }, poll);
      });
    })();
  }

  /**
   * Connect (prompts MetaMask). Returns the new state.
   * @param onWait optional callback(msg) used to tell the user that a popup
   *               they already have open is what is being waited on.
   */
  function connect(onWait) {
    if (state.pending) return state.pending;          // one request per origin
    state.pending = (async function () {
      var sel = pick();
      if (!sel) {
        var err = new Error('No browser wallet detected. Install MetaMask, then reload this page.');
        err.code = 'NO_PROVIDER';
        throw err;
      }
      state.provider = sel.provider;
      state.label = sel.info.name || 'Injected wallet';
      bind(state.provider);

      var accts = null;
      // (1) silent path — already authorised, no popup, no pending request.
      try {
        var have = await req('eth_accounts');
        if (have && have.length) accts = have;
      } catch (e) { /* not fatal: fall through to the prompting path */ }

      if (!accts) {
        try {
          accts = await req('eth_requestAccounts');
        } catch (e) {
          if (!isPendingErr(e)) throw e;
          // (3) a popup is already open — wait for the user to answer it.
          accts = await awaitPendingApproval(onWait);
          if (!accts.length) {
            var pe = new Error(PENDING_MSG);
            pe.code = -32002;
            throw pe;
          }
        }
      }

      if (!accts || !accts.length) throw new Error('No account was authorized.');
      state.account = accts[0].toLowerCase();
      try { state.chainId = await req('eth_chainId'); } catch (e) {}
      try { localStorage.setItem('sigil-mm-connected', '1'); } catch (e) {}
      emit('change', getState());
      return getState();
    })();
    // (2) clear the dedup slot once it settles, either way.
    state.pending.then(function () { state.pending = null; },
                       function () { state.pending = null; });
    return state.pending;
  }

  /**
   * Reconnect silently if the user already authorized this site in a previous
   * visit — eth_accounts never prompts, so this is safe to call on page load.
   */
  async function resume() {
    try { if (localStorage.getItem('sigil-mm-connected') !== '1') return getState(); } catch (e) { return getState(); }
    var sel = pick(); if (!sel) return getState();
    state.provider = sel.provider;
    state.label = sel.info.name || 'Injected wallet';
    bind(state.provider);
    try {
      var accts = await req('eth_accounts');
      if (accts && accts.length) {
        state.account = accts[0].toLowerCase();
        try { state.chainId = await req('eth_chainId'); } catch (e) {}
        emit('change', getState());
      } else {
        state.provider = null; state.account = null;
      }
    } catch (e) { state.provider = null; state.account = null; }
    return getState();
  }

  function disconnect() {
    // EIP-1193 has no revoke; this forgets the site's own connection state.
    state.provider = null; state.account = null; state.chainId = null; state.label = null;
    try { localStorage.removeItem('sigil-mm-connected'); } catch (e) {}
    emit('change', getState());
  }

  /**
   * Make sure MetaMask is on Polygon mainnet, adding the network if the user
   * doesn't have it yet. 4902 = "chain not added"; MetaMask also surfaces that
   * as -32603 wrapping 4902 in some versions, so both are handled.
   */
  async function ensurePolygon() {
    if (!state.provider) await connect();
    try { state.chainId = await req('eth_chainId'); } catch (e) {}
    if (state.chainId === POLYGON_PARAMS.chainId) return true;
    try {
      await req('wallet_switchEthereumChain', [{ chainId: POLYGON_PARAMS.chainId }]);
    } catch (e) {
      var code = e && (e.code != null ? e.code : (e.data && e.data.originalError && e.data.originalError.code));
      if (code === 4902 || code === -32603) {
        await req('wallet_addEthereumChain', [POLYGON_PARAMS]);   // adds AND switches
      } else {
        throw e;
      }
    }
    try { state.chainId = await req('eth_chainId'); } catch (e) {}
    emit('change', getState());
    return state.chainId === POLYGON_PARAMS.chainId;
  }

  /**
   * Ask MetaMask to display wrapped SIGIL in its asset list. Never fatal:
   * the user may simply decline, and the bridge works either way.
   */
  async function watchToken() {
    if (!state.provider) return false;
    try {
      return !!(await req('wallet_watchAsset', {
        type: 'ERC20',
        options: { address: TOKEN.address, symbol: TOKEN.symbol, decimals: TOKEN.decimals }
      }));
    } catch (e) { return false; }
  }

  /** connect -> Polygon -> offer the token, in one call. */
  async function connectFull() {
    var st = await connect();
    try { await ensurePolygon(); } catch (e) { /* user may decline the switch */ }
    try { await watchToken(); } catch (e) {}
    return getState();
  }

  async function signMessage(message) {
    if (!state.account) await connect();
    return await req('personal_sign', [message, state.account]);
  }

  // ── ABI helpers (hand-rolled: two static call shapes, no encoder needed) ──
  function pad32(hexNo0x) {
    var h = String(hexNo0x).replace(/^0x/, '').toLowerCase();
    if (h.length > 64) throw new Error('value too wide for uint256/bytes32');
    return '0'.repeat(64 - h.length) + h;
  }

  /** eth_getLogs against the user's provider, falling back to the public RPCs.
      Read-only and key-free — used to show Polygon-side activity (Uniswap trades,
      token transfers) that never passes through this origin. */
  async function getLogs(filter) {
    var body = { jsonrpc: '2.0', id: 1, method: 'eth_getLogs', params: [filter] };
    if (state.provider) {
      try { return await req('eth_getLogs', [filter]); } catch (e) { /* fall through */ }
    }
    var lastErr = null;
    for (var i = 0; i < PUBLIC_RPCS.length; i++) {
      try {
        var r = await fetch(PUBLIC_RPCS[i], {
          method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body)
        });
        var j = await r.json();
        if (j && j.result) return j.result;
        lastErr = new Error((j && j.error && j.error.message) || 'RPC returned no result');
      } catch (e) { lastErr = e; }
    }
    throw lastErr || new Error('every Polygon RPC endpoint failed');
  }

  /* ── DIRECT UNISWAP V2 (2026-09-06) ────────────────────────────────────────
     Why this exists rather than "just use app.uniswap.org".

     Reported 2026-09-06: the Uniswap interface answered "Token approval failed —
     A network or connection issue likely caused your approval to fail." Checked
     on chain afterwards: the account's allowance to Permit2, to the V2 router and
     to both Universal Routers was ZERO — no approval transaction was ever mined,
     so the message was literally true and nothing was wrong with the token. The
     same swap, sent straight to the canonical V2 router from a script, went
     through and returned EXACTLY the quoted amount.

     The modern Uniswap app routes through Permit2 + the Universal Router and
     leans on its own backend for routing and for watching the approval land.
     That is three moving parts more than a V2 swap needs, and each of them can
     fail for a brand-new unlisted pair with two-tenths of a cent in it. So this
     talks to the V2 router directly: approve exactly the amount, then
     swapExactTokensForTokens. No Permit2, no Universal Router, no routing API.

     Hand-encoded because it is two static call shapes and pulling in an ABI coder
     for them would be the larger dependency. Both verified against the live
     router before shipping.                                                     */
  var UNI_V2_ROUTER = '0xedf6066a2b290C185783862C7F4776A2C8077AD1';
  var SEL_ALLOWANCE = '0xdd62ed3e'; // allowance(address,address)
  var SEL_APPROVE   = '0x095ea7b3'; // approve(address,uint256)
  var SEL_AMTS_OUT  = '0xd06ca61f'; // getAmountsOut(uint256,address[])
  var SEL_SWAP_EXACT= '0x38ed1739'; // swapExactTokensForTokens(uint256,uint256,address[],address,uint256)
  function n32(v) { return pad32(BigInt(v).toString(16)); }
  function a32(a) { return pad32(String(a).replace(/^0x/, '').toLowerCase()); }

  /** How much `path[last]` comes out for `amountIn` of `path[0]`. Read-only. */
  async function uniQuote(amountIn, path) {
    var data = SEL_AMTS_OUT + n32(amountIn) + n32(64) + n32(path.length) +
               path.map(a32).join('');
    var res = await rawCall(UNI_V2_ROUTER, data);
    var d = String(res).replace(/^0x/, '');
    var len = parseInt(d.slice(64, 128), 16);
    var outs = [];
    for (var i = 0; i < len; i++) outs.push(BigInt('0x' + d.slice(128 + i * 64, 192 + i * 64)));
    return outs;
  }

  /**
   * Approve (only if the current allowance is short) and swap, through MetaMask.
   * `slippageBps` guards the minimum out; on a pool this thin the price moves a lot
   * with your own trade, which is not slippage — the quote already accounts for it.
   * Returns { approveTx, swapTx, expectedOut, minOut }.
   *
   * SPENDS REAL TOKENS. The caller confirms first.
   */
  async function uniSwap(amountIn, path, slippageBps, onStep) {
    if (!state.account) await connect();
    if (!(await ensurePolygon())) throw new Error('MetaMask must be on Polygon mainnet.');
    var amt = BigInt(amountIn);
    if (amt <= 0n) throw new Error('Amount must be greater than zero.');

    var outs = await uniQuote(amt, path);
    var expected = outs[outs.length - 1];
    if (expected <= 0n) throw new Error('The pool cannot fill that trade.');
    var minOut = expected * BigInt(10000 - (slippageBps || 100)) / 10000n;

    var approveTx = null;
    var cur = BigInt(await rawCall(path[0], SEL_ALLOWANCE + a32(state.account) + a32(UNI_V2_ROUTER)));
    if (cur < amt) {
      if (onStep) onStep('approve');
      // Exact amount, not MAX: a brand-new token with no live bridge behind it has
      // not earned an unlimited standing allowance.
      approveTx = await req('eth_sendTransaction', [{
        from: state.account, to: path[0],
        data: SEL_APPROVE + a32(UNI_V2_ROUTER) + n32(amt)
      }]);
      if (onStep) onStep('approve-sent', approveTx);
      // The swap must not be signed until the approval is actually mined, or it
      // reverts on TRANSFER_FROM_FAILED — which is exactly the confusing failure
      // this whole function exists to avoid.
      await waitMined(approveTx);
    }

    if (onStep) onStep('swap');
    var deadline = Math.floor(Date.now() / 1000) + 900;
    var data = SEL_SWAP_EXACT + n32(amt) + n32(minOut) + n32(160) +
               a32(state.account) + n32(deadline) +
               n32(path.length) + path.map(a32).join('');
    var swapTx = await req('eth_sendTransaction', [{
      from: state.account, to: UNI_V2_ROUTER, data: data
    }]);
    if (onStep) onStep('swap-sent', swapTx);
    return { approveTx: approveTx, swapTx: swapTx, expectedOut: expected, minOut: minOut };
  }

  /** Poll until a tx has a receipt. Polygon blocks are ~2 s; 3 minutes is generous. */
  async function waitMined(hash, timeoutMs) {
    var until = Date.now() + (timeoutMs || 180000);
    while (Date.now() < until) {
      try {
        var r = await req('eth_getTransactionReceipt', [hash]);
        if (r && r.blockNumber) {
          if (r.status && BigInt(r.status) === 0n) throw new Error('Transaction reverted on chain: ' + hash);
          return r;
        }
      } catch (e) { if (String(e && e.message).indexOf('reverted') >= 0) throw e; }
      await new Promise(function (r2) { setTimeout(r2, 2500); });
    }
    throw new Error('Timed out waiting for ' + hash + ' — it may still land; check Polygonscan.');
  }

  async function rawCall(to, data) {
    var body = { jsonrpc: '2.0', id: 1, method: 'eth_call', params: [{ to: to, data: data }, 'latest'] };
    if (state.provider) {
      try { return await req('eth_call', [{ to: to, data: data }, 'latest']); } catch (e) { /* fall through to public RPC */ }
    }
    var lastErr = null;
    for (var i = 0; i < PUBLIC_RPCS.length; i++) {
      try {
        var r = await fetch(PUBLIC_RPCS[i], {
          method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body)
        });
        var j = await r.json();
        if (j && j.result) return j.result;
        lastErr = new Error((j && j.error && j.error.message) || 'RPC returned no result');
      } catch (e) { lastErr = e; }
    }
    throw lastErr || new Error('every Polygon RPC endpoint failed');
  }

  /** Wrapped-SIGIL balance of a Polygon address, as an 18dp wei BigInt. */
  async function balanceOfWei(addr) {
    var a = String(addr || '').toLowerCase().replace(/^0x/, '');
    if (!/^[0-9a-f]{40}$/.test(a)) throw new Error('not a valid Polygon address');
    var res = await rawCall(TOKEN.address, SEL_BALANCEOF + pad32(a));
    return BigInt(res || '0x0');
  }

  /**
   * Burn wrapped SIGIL on Polygon and instruct the bridge to release the same
   * amount of native SIGIL to `sigilAddr64hex`.
   *
   * @param amountBase8      BigInt, native SIGIL base units (8dp) — NOT wei.
   * @param sigilAddr64hex   the 64-hex SIGIL wallet address (bytes32).
   * @returns the Polygon transaction hash.
   *
   * THIS SPENDS REAL TOKENS AND IS NOT REVERSIBLE. Callers must confirm first.
   */
  async function burnToSigil(amountBase8, sigilAddr64hex) {
    // ── BURNING IS DISABLED (2026-09-06) ────────────────────────────────────
    //
    // The button above this call promises: "the relayer sees the burn and releases
    // the same amount of native SIGIL". That sentence is currently FALSE. The
    // sigil-bridge-relayer is deliberately masked (DO-NOT-START-README.md, an
    // unbacked-mint hazard), so nothing is watching for burn events. A burn today
    // destroys wrapped SIGIL on Polygon — irreversibly, by design, that is what burn
    // MEANS — and releases nothing on the SIGIL side. There is no undo and no
    // support desk.
    //
    // The token this helper points at (0xc224602C…25B7) is also the OLD wSIGIL, whose
    // backing did not survive the g2 chain reset and whose pool is ~99% drained. Its
    // replacement wSIGIL3 (0x3FCED760…C2e9, deployed 2026-09-06) is tradeable on
    // Uniswap but likewise has no live bridge behind it yet.
    //
    // So the guard is here, in the ONE function every caller funnels through, rather
    // than in a button handler that a second caller could bypass. Lift it in the same
    // commit that unmasks the relayer — not before, and not "temporarily".
    if (!BRIDGE_BURN_ENABLED) {
      var e = new Error(
        'Burning is disabled: the bridge relayer is offline, so a burn would destroy ' +
        'your wrapped SIGIL on Polygon and release nothing on the SIGIL side. ' +
        'Your tokens are safe where they are — sell or hold them on Uniswap instead.');
      e.code = 'BRIDGE_DISABLED';
      throw e;
    }
    if (!state.account) await connect();
    var ok = await ensurePolygon();
    if (!ok) throw new Error('MetaMask must be on Polygon mainnet to burn wrapped SIGIL.');

    var dest = String(sigilAddr64hex || '').toLowerCase().replace(/^0x/, '');
    if (!/^[0-9a-f]{64}$/.test(dest)) throw new Error('Destination must be a 64-hex SIGIL address.');

    var amt = BigInt(amountBase8);
    if (amt <= 0n) throw new Error('Amount must be greater than zero.');

    // 8dp -> 18dp. Because the caller is capped at 8 decimals this is always a
    // clean multiple of DECIMAL_SHIFT, so the relayer's flooring never bites.
    var wei = amt * DECIMAL_SHIFT;

    var bal = await balanceOfWei(state.account);
    if (bal < wei) throw new Error('Not enough wrapped SIGIL on Polygon — you hold ' + formatWei(bal) + '.');

    var data = SEL_BURN + pad32(wei.toString(16)) + pad32(dest);
    return await req('eth_sendTransaction', [{ from: state.account, to: TOKEN.address, data: data }]);
  }

  /** 18dp wei BigInt -> display string with 8dp (the native SIGIL precision). */
  function formatWei(wei) {
    var base = BigInt(wei) / DECIMAL_SHIFT;              // -> 8dp base units
    var whole = base / 100000000n;
    var frac = (base % 100000000n).toString().padStart(8, '0');
    return whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',') + '.' + frac;
  }

  /** "1.5" -> 150000000n (8dp base units). Rejects >8dp instead of rounding. */
  function parseAmount8(str) {
    var s = String(str || '').trim();
    if (!/^\d+(\.\d{1,8})?$/.test(s)) throw new Error('Amount must be a number with at most 8 decimals.');
    var p = s.split('.');
    return BigInt(p[0] || '0') * 100000000n + BigInt(((p[1] || '') + '00000000').slice(0, 8));
  }

  root.SigilMM = {
    PENDING_MSG: PENDING_MSG,
    isPendingErr: isPendingErr,
    POLYGON_PARAMS: POLYGON_PARAMS,
    TOKEN: TOKEN,
    DECIMAL_SHIFT: DECIMAL_SHIFT,
    available: available,
    list: list,
    connect: connect,
    connectFull: connectFull,
    resume: resume,
    disconnect: disconnect,
    ensurePolygon: ensurePolygon,
    watchToken: watchToken,
    signMessage: signMessage,
    balanceOfWei: balanceOfWei,
    burnToSigil: burnToSigil,
    burnEnabled: function () { return BRIDGE_BURN_ENABLED; },
    // Read any Polygon ERC-20 (falls back to public RPCs when MetaMask is absent),
    // so the wallet's asset list can show the Polygon side without a second RPC client.
    rawCall: rawCall,
    getLogs: getLogs,
    pad32: pad32,
    uniQuote: uniQuote,
    uniSwap: uniSwap,
    waitMined: waitMined,
    UNI_V2_ROUTER: UNI_V2_ROUTER,
    formatWei: formatWei,
    parseAmount8: parseAmount8,
    getState: getState,
    on: on,
    explorerTx: function (h) { return 'https://polygonscan.com/tx/' + h; },
    explorerToken: function () { return 'https://polygonscan.com/token/' + TOKEN.address; }
  };
})(window);
