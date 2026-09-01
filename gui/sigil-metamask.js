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

  var state = { provider: null, account: null, chainId: null, label: null };
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

  /** Connect (prompts MetaMask). Returns the new state. */
  async function connect() {
    var sel = pick();
    if (!sel) {
      var err = new Error('No browser wallet detected. Install MetaMask, then reload this page.');
      err.code = 'NO_PROVIDER';
      throw err;
    }
    state.provider = sel.provider;
    state.label = sel.info.name || 'Injected wallet';
    bind(state.provider);
    var accts = await req('eth_requestAccounts');
    if (!accts || !accts.length) throw new Error('No account was authorized.');
    state.account = accts[0].toLowerCase();
    try { state.chainId = await req('eth_chainId'); } catch (e) {}
    try { localStorage.setItem('sigil-mm-connected', '1'); } catch (e) {}
    emit('change', getState());
    return getState();
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
    formatWei: formatWei,
    parseAmount8: parseAmount8,
    getState: getState,
    on: on,
    explorerTx: function (h) { return 'https://polygonscan.com/tx/' + h; },
    explorerToken: function () { return 'https://polygonscan.com/token/' + TOKEN.address; }
  };
})(window);
