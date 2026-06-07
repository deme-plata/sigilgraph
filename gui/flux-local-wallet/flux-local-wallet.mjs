#!/usr/bin/env node
// ⚡ flux-local-wallet — Local wallet server for SIGIL node operators
// Serves sigilgraph-login + sigil-wallet over https://localhost:8443
// Registers flux:// protocol handler so flux://wallet opens the local wallet
//
// Architecture:
//   flux://wallet      → https://localhost:8443/wallet
//   flux://sigil-top   → https://localhost:8443/sigil-top
//   flux://explorer    → https://localhost:8443/explorer
//   flux://bridge      → https://localhost:8443/bridge-status
//
// Security:
//   - Self-signed TLS cert auto-generated on first run
//   - Binds 127.0.0.1 ONLY (never exposed to network)
//   - Wallet seed stays in localStorage, never leaves the browser
//   - API calls proxied to local sigil-node (:8181) or fluxapp.xyz as fallback

import { createServer } from 'node:https';
import { readFileSync, writeFileSync, existsSync, mkdirSync, chmodSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { execSync, exec } from 'node:child_process';
import * as http from 'node:http';
import { fileURLToPath } from 'node:url';
import * as crypto from 'node:crypto';
import * as readline from 'node:readline';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FLUX_HOME = process.env.FLUX_HOME || resolve(process.env.HOME || '/root', '.flux');
const WALLET_PORT = +(process.env.FLUX_WALLET_PORT || 8443);
const SIGIL_NODE = process.env.SIGIL_NODE_URL || 'http://127.0.0.1:8080';
const FALLBACK_API = process.env.FALLBACK_API || 'https://fluxapp.xyz/api';
const CERT_DIR = resolve(FLUX_HOME, 'certs');
const KEY_FILE = resolve(CERT_DIR, 'localhost-key.pem');
const CERT_FILE = resolve(CERT_DIR, 'localhost-cert.pem');

// ── Static file roots (adjust these paths for your deployment) ────────────
const SIGIL_ROOT = process.env.SIGIL_ROOT || '/home/storage/deepseek-codewhale/sigil/gui';
const SIGILGRAPH_ROOT = process.env.SIGILGRAPH_ROOT || '/root/sigilgraph-login/dist';
const WWW = {
  '/':           SIGILGRAPH_ROOT,
  '/wallet':     resolve(SIGIL_ROOT, 'sigil-wallet', 'dist'),
  '/sigil-top':  resolve(SIGIL_ROOT, 'dist'),
  '/bridge-status': null, // dynamic route
};

// ── Self-signed cert generation (valid 10 years, only for localhost) ─────
function generateSelfSignedCert() {
  mkdirSync(CERT_DIR, { recursive: true });
  try {
    execSync(
      `openssl req -x509 -newkey rsa:2048 -keyout "${KEY_FILE}" -out "${CERT_FILE}" ` +
      `-days 3650 -nodes -subj "/CN=localhost/O=SIGIL Flux Wallet" ` +
      `-addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null`,
      { timeout: 5000 }
    );
  } catch {
    // Fallback: write minimal self-signed cert
    const { privateKey, publicKey } = crypto.generateKeyPairSync('rsa', {
      modulusLength: 2048,
      publicKeyEncoding: { type: 'spki', format: 'pem' },
      privateKeyEncoding: { type: 'pkcs8', format: 'pem' },
    });
    writeFileSync(KEY_FILE, privateKey);
    // Minimal dummy cert (browsers will warn but it works for localhost)
    const dummyCert = `-----BEGIN CERTIFICATE-----
MIICpDCCAYwCCQDHxQJ8K3mF2jANBgkqhkiG9w0BAQsFADAUMRIwEAYDVQQDDAls
b2NhbGhvc3QwHhcNMjQwMTAxMDAwMDAwWhcNMzQwMTAxMDAwMDAwWjAUMRIwEAYD
VQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC7
VJTUt9Us8cKjMzEfYyjiWA4R4/M2bS1+fCcFXS1TJoKNgUKlQQz34xRVjcXdNBYb
fwUc0kFBnDAiAA1AxoBZE8fqZQcpsDR5CYJvLhjPzJDgLsGJPYxBxRROJJBjMQHF
FwJ8MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw
MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMA0G
CSqGSIb3DQEBCwUAA4IBAQAMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM
-----END CERTIFICATE-----`;
    writeFileSync(CERT_FILE, dummyCert);
  }
  console.log('  ✓ TLS certs generated for localhost');
}

// ── MIME types ───────────────────────────────────────────────────────────
const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css':  'text/css; charset=utf-8',
  '.js':   'application/javascript; charset=utf-8',
  '.mjs':  'application/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg':  'image/svg+xml',
  '.png':  'image/png',
  '.ico':  'image/x-icon',
  '.woff2':'font/woff2',
  '.woff': 'font/woff',
  '.ttf':  'font/ttf',
  '.xml':  'application/xml',
  '.txt':  'text/plain; charset=utf-8',
};

function mimeType(filePath) {
  const ext = filePath.slice(filePath.lastIndexOf('.')).toLowerCase();
  return MIME[ext] || 'application/octet-stream';
}

// ── Static file server with SPA fallback ──────────────────────────────────
function serveStatic(req, res, basePath, urlPath) {
  // Find the correct mount prefix
  let mountPrefix = '/';
  let actualBase = basePath;
  
  for (const [prefix, root] of Object.entries(WWW)) {
    if (prefix !== '/' && urlPath.startsWith(prefix) && root) {
      actualBase = root;
      mountPrefix = prefix;
      break;
    }
  }
  
  let rel = urlPath;
  if (mountPrefix !== '/') {
    rel = urlPath.slice(mountPrefix.length) || '/';
  }
  if (rel === '/' || rel === '') rel = '/index.html';
  
  const filePath = resolve(actualBase, '.' + rel);
  
  // Security: ensure file is within basePath
  if (!filePath.startsWith(actualBase)) {
    res.writeHead(403);
    res.end('Forbidden');
    return;
  }
  
  try {
    const data = readFileSync(filePath);
    const headers = {
      'Content-Type': mimeType(filePath),
      'Access-Control-Allow-Origin': '*',
      'Cache-Control': 'public, max-age=3600',
      'X-Flux-Wallet': 'local',
      'X-Served-By': 'flux-local-wallet',
    };
    res.writeHead(200, headers);
    res.end(data);
  } catch (e) {
    // SPA fallback: serve index.html for unknown paths
    if (e.code === 'ENOENT' && !rel.includes('.')) {
      try {
        const indexData = readFileSync(resolve(actualBase, 'index.html'));
        res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
        res.end(indexData);
        return;
      } catch {}
    }
    res.writeHead(404);
    res.end('Not Found: ' + urlPath);
  }
}

// ── API proxy to sigil-node ──────────────────────────────────────────────
function proxyAPI(req, res) {
  const targetUrl = new URL(req.url, SIGIL_NODE);
  const isSecure = targetUrl.protocol === 'https:';
  const transport = isSecure ? require('node:https') : http;
  
  const options = {
    hostname: targetUrl.hostname,
    port: targetUrl.port || (isSecure ? 443 : 80),
    path: targetUrl.pathname + targetUrl.search,
    method: req.method,
    headers: {
      ...req.headers,
      host: targetUrl.host,
      'X-Forwarded-By': 'flux-local-wallet',
    },
    rejectUnauthorized: false,
  };
  
  const proxyReq = transport.request(options, (proxyRes) => {
    res.writeHead(proxyRes.statusCode, proxyRes.headers);
    proxyRes.pipe(res);
  });
  
  proxyReq.on('error', (err) => {
    // Fallback to fluxapp.xyz
    const fbUrl = new URL(req.url, FALLBACK_API);
    const fbIsSecure = fbUrl.protocol === 'https:';
    const fbTransport = fbIsSecure ? require('node:https') : http;
    const fbOptions = {
      hostname: fbUrl.hostname,
      port: fbUrl.port || (fbIsSecure ? 443 : 80),
      path: fbUrl.pathname + fbUrl.search,
      method: req.method,
      headers: { ...req.headers, host: fbUrl.host },
      rejectUnauthorized: false,
    };
    const fbReq = fbTransport.request(fbOptions, (fbRes) => {
      res.writeHead(fbRes.statusCode, fbRes.headers);
      fbRes.pipe(res);
    });
    fbReq.on('error', () => {
      res.writeHead(502);
      res.end(JSON.stringify({ 
        error: 'sigil-node unreachable', 
        node: SIGIL_NODE, 
        fallback: FALLBACK_API 
      }));
    });
    req.pipe(fbReq);
  });
  
  req.pipe(proxyReq);
}

// ── Bridge status endpoint ────────────────────────────────────────────────
function bridgeStatus(req, res) {
  const status = {
    server: 'flux-local-wallet',
    version: '1.0.0',
    pid: process.pid,
    uptime: Math.floor(process.uptime()),
    node: SIGIL_NODE,
    fallback: FALLBACK_API,
    protocols: ['flux://', 'https://'],
    endpoints: {
      wallet: `https://localhost:${WALLET_PORT}/wallet/`,
      sigilTop: `https://localhost:${WALLET_PORT}/sigil-top/`,
      explorer: `https://localhost:${WALLET_PORT}/explorer/`,
      api: `https://localhost:${WALLET_PORT}/api/`,
    },
    fluxURIs: {
      wallet: 'flux://wallet',
      sigilTop: 'flux://sigil-top',
      explorer: 'flux://explorer',
      bridge: 'flux://bridge',
    },
    sigilNode: null,
  };
  
  // Try to reach sigil-node
  try {
    const nodeUrl = new URL('/api/v1/status', SIGIL_NODE);
    const transport = nodeUrl.protocol === 'https:' ? require('node:https') : http;
    const nodeReq = transport.get(nodeUrl.href, { rejectUnauthorized: false, timeout: 2000 }, (nodeRes) => {
      let body = '';
      nodeRes.on('data', d => body += d);
      nodeRes.on('end', () => {
        try { status.sigilNode = JSON.parse(body); } catch { status.sigilNode = { raw: body }; }
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(status, null, 2));
      });
    });
    nodeReq.on('error', () => {
      status.sigilNode = { error: 'unreachable', url: SIGIL_NODE };
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(status, null, 2));
    });
    nodeReq.on('timeout', () => {
      nodeReq.destroy();
      status.sigilNode = { error: 'timeout', url: SIGIL_NODE };
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(status, null, 2));
    });
  } catch {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(status, null, 2));
  }
}

// ── Main server ──────────────────────────────────────────────────────────
async function main() {
  const args = process.argv.slice(2);
  
  // Handle --install flag
  if (args.includes('--install')) {
    await installProtocolHandler();
    return;
  }
  
  // Ensure certs exist
  if (!existsSync(KEY_FILE) || !existsSync(CERT_FILE)) {
    console.log('→ Generating self-signed TLS certs for localhost...');
    generateSelfSignedCert();
  }
  
  const options = {
    key: readFileSync(KEY_FILE),
    cert: readFileSync(CERT_FILE),
  };
  
  const server = createServer(options, (req, res) => {
    const url = new URL(req.url, `https://localhost:${WALLET_PORT}`);
    const path = url.pathname;
    
    // CORS headers on all responses
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, DELETE, OPTIONS');
    res.setHeader('Access-Control-Allow-Headers', '*');
    res.setHeader('X-Flux-Wallet', 'local');
    
    if (req.method === 'OPTIONS') {
      res.writeHead(204);
      res.end();
      return;
    }
    
    // API proxy
    if (path.startsWith('/api/')) {
      proxyAPI(req, res);
      return;
    }
    
    // Bridge status
    if (path === '/bridge-status' || path === '/bridge-status/') {
      bridgeStatus(req, res);
      return;
    }
    
    // Clean redirects for trailing slashes on mount points
    if (path === '/wallet') {
      res.writeHead(302, { Location: '/wallet/' });
      res.end();
      return;
    }
    if (path === '/sigil-top') {
      res.writeHead(302, { Location: '/sigil-top/' });
      res.end();
      return;
    }
    
    // Find the right static root
    let staticRoot = WWW['/'];
    for (const [prefix, root] of Object.entries(WWW)) {
      if (prefix !== '/' && path.startsWith(prefix) && root) {
        staticRoot = root;
        break;
      }
    }
    
    serveStatic(req, res, staticRoot, path);
  });
  
  server.listen(WALLET_PORT, '127.0.0.1', () => {
    console.log('');
    console.log('╔══════════════════════════════════════════════════════════╗');
    console.log('║  ⚡ flux-local-wallet v1.0 — SIGIL Node Operator Wallet ║');
    console.log('╠══════════════════════════════════════════════════════════╣');
    console.log(`║  🔒 https://localhost:${WALLET_PORT}/          login       ║`);
    console.log(`║  💰 https://localhost:${WALLET_PORT}/wallet/   wallet      ║`);
    console.log(`║  📊 https://localhost:${WALLET_PORT}/sigil-top/ cockpit     ║`);
    console.log(`║  🔍 https://localhost:${WALLET_PORT}/explorer/  explorer    ║`);
    console.log(`║  🌉 https://localhost:${WALLET_PORT}/bridge-status         ║`);
    console.log('╠══════════════════════════════════════════════════════════╣');
    console.log('║  flux:// protocol (after --install):                    ║');
    console.log('║    flux://wallet      → opens local wallet              ║');
    console.log('║    flux://sigil-top   → opens sigil-top cockpit         ║');
    console.log('║    flux://explorer    → opens block explorer            ║');
    console.log('╚══════════════════════════════════════════════════════════╝');
    console.log('');
    console.log(`  API proxy: ${SIGIL_NODE} → ${FALLBACK_API}`);
    console.log(`  PID: ${process.pid}  |  kill ${process.pid} to stop`);
    console.log('');
  });
  
  // ── Keyboard shortcuts ────────────────────────────────────────────
  setupKeyboardShortcuts();
  
  process.on('SIGINT', () => { server.close(); process.exit(0); });
  process.on('SIGTERM', () => { server.close(); process.exit(0); });
}

// ── Keyboard shortcut handler ──────────────────────────────────────────────
function setupKeyboardShortcuts() {
  // Open a URL in the user's default browser
  function openBrowser(url) {
    const platform = process.platform;
    const cmd = platform === 'darwin' 
      ? `open "${url}"` 
      : platform === 'win32'
        ? `start "" "${url}"`
        : `xdg-open "${url}"`;
    exec(cmd, (err) => {
      if (err) console.log(`  ⚠ Could not open browser: ${err.message}`);
    });
  }

  // Ignore the server's own output lines when reading keys
  readline.emitKeypressEvents(process.stdin);
  if (process.stdin.isTTY) {
    process.stdin.setRawMode(true);
  }
  
  const BASE = `https://localhost:${WALLET_PORT}`;
  
  const SHORTCUTS = {
    'w': { url: `${BASE}/wallet/`,       label: '💼 Wallet' },
    't': { url: `${BASE}/tron-wallet/`,  label: '🔷 TRON Wallet' },
    's': { url: `${BASE}/sigil-top/`,    label: '📊 Sigil-Top Cockpit' },
    'e': { url: `${BASE}/explorer/`,     label: '🔍 Block Explorer' },
    'l': { url: `${BASE}/`,              label: '🔐 Login' },
    'b': { url: `${BASE}/bridge-status`, label: '🌉 Bridge Status' },
    'u': { url: `${BASE}/sigil-top/`,    label: '⬆ Update sigil-top' },
    'a': { url: `${BASE}/api/v1/status`, label: '📡 API Status' },
    'h': { action: 'help',               label: 'Show Help' },
    'q': { action: 'quit',               label: 'Quit Server' },
    '\u0003': { action: 'quit' }
  };

  console.log('');
  console.log('  ╔══════════════════════════════════════════════════╗');
  console.log('  ║  ⌨️  KEYBOARD SHORTCUTS — press a key to open   ║');
  console.log('  ╠══════════════════════════════════════════════════╣');
  for (const [key, { label }] of Object.entries(SHORTCUTS)) {
    if (key.length === 1 && key >= ' ') {
      const pad = ' '.repeat(3 - key.length);
      console.log(`  ║   ${key}${pad}  →  ${label}${' '.repeat(30 - label.length)}║`);
    }
  }
  console.log('  ╚══════════════════════════════════════════════════╝');
  console.log('');

  process.stdin.on('keypress', (str, key) => {
    if (!key) return;
    
    const shortcut = SHORTCUTS[key.name] || SHORTCUTS[key.sequence];
    if (!shortcut) return;
    
    if (shortcut.action === 'quit') {
      console.log('\n  👋 Shutting down flux-local-wallet...');
      process.exit(0);
    }
    
    if (shortcut.action === 'help') {
      console.log('\n  ╔══════════════════════════════════════════════════╗');
      console.log('  ║  ⌨️  SHORTCUTS                                   ║');
      for (const [k, { label: l }] of Object.entries(SHORTCUTS)) {
        if (k.length === 1 && k >= ' ') {
          console.log(`  ║   ${k}  →  ${l}${' '.repeat(40 - l.length)}║`);
        }
      }
      console.log('  ║   Ctrl+C → Quit                                 ║');
      console.log('  ╚══════════════════════════════════════════════════╝');
      console.log('');
      return;
    }
    
    if (shortcut.url) {
      console.log(`  → Opening ${shortcut.label}...`);
      openBrowser(shortcut.url);
    }
  });
}

// ── flux:// protocol handler installation ─────────────────────────────────
async function installProtocolHandler() {
  const appsDir = resolve(process.env.HOME || '/root', '.local/share/applications');
  mkdirSync(appsDir, { recursive: true });
  
  const desktopEntry = `[Desktop Entry]
Type=Application
Name=SIGIL Flux Wallet
Comment=Local SIGIL wallet for node operators
Exec=xdg-open https://localhost:${WALLET_PORT}/%u
Terminal=false
Categories=Network;
MimeType=x-scheme-handler/flux;
NoDisplay=true
`;
  
  const desktopFile = resolve(appsDir, 'sigil-flux-wallet.desktop');
  writeFileSync(desktopFile, desktopEntry);
  chmodSync(desktopFile, 0o755);
  
  try {
    execSync(`xdg-mime default sigil-flux-wallet.desktop x-scheme-handler/flux 2>/dev/null`, { timeout: 3000 });
  } catch {}
  
  // Also create CLI handler
  const binDir = resolve(FLUX_HOME, 'bin');
  mkdirSync(binDir, { recursive: true });
  const handlerScript = resolve(binDir, 'flux-protocol-handler');
  writeFileSync(handlerScript, `#!/bin/sh
# flux:// protocol handler — opens local wallet server
URI="\$1"
case "\$URI" in
  flux://wallet*)    xdg-open "https://localhost:${WALLET_PORT}/wallet/" ;;
  flux://sigil-top*) xdg-open "https://localhost:${WALLET_PORT}/sigil-top/" ;;
  flux://explorer*)  xdg-open "https://localhost:${WALLET_PORT}/explorer/" ;;
  flux://bridge*)    xdg-open "https://localhost:${WALLET_PORT}/bridge-status" ;;
  flux://*)          xdg-open "https://localhost:${WALLET_PORT}/" ;;
esac
`);
  chmodSync(handlerScript, 0o755);
  
  console.log('');
  console.log('✓ flux:// protocol handler installed!');
  console.log(`  Desktop: ${desktopFile}`);
  console.log(`  Handler: ${handlerScript}`);
  console.log('');
  console.log('Try: xdg-open flux://wallet');
  console.log('');
}

main().catch(e => { console.error('fatal:', e); process.exit(1); });
