// Exercise the BUNDLED artifact, not the source modules — a bundler can drop or
// mangle things the source test never sees.
// The bundle is an IIFE meant for a <script> tag; run it that way rather than
// require()-ing it, so what is tested is exactly the artifact the page will load.
global.window = {};
const src = require('fs').readFileSync(__dirname + '/shieldscan.bundle.js', 'utf8');
(0, eval)(src);
const S = global.window.__sigilShieldScan;
if (!S) { console.log('✗ bundle did not expose __sigilShieldScan'); process.exit(1); }
const V = JSON.parse(process.argv[2]);
const hex2b = h => Uint8Array.from(h.match(/../g).map(x => parseInt(x, 16)));
const b2hex = b => [...b].map(x => x.toString(16).padStart(2, '0')).join('');
const seed = hex2b(V.seed_hex);
const id = S.encIdentityFromSeed(seed);
const pkOk = b2hex(id.pk) === V.pk_enc_hex;
const note = S.decodeNote(S.openEnvelope(V.ciphertext, id));
const vOk = note && note.value === BigInt(V.expect_value);
const bOk = note && note.blinding === BigInt(V.expect_blinding_str);
const stranger = S.encIdentityFromSeed(new Uint8Array(32).fill(0x11));
const leak = S.decodeNote(S.openEnvelope(V.ciphertext, stranger));
const scan = S.scanBalance([V.ciphertext], seed, {});
console.log(`  bundle exposes API    : yes`);
console.log(`  pk_enc                : ${pkOk ? 'MATCH' : 'MISMATCH'}`);
console.log(`  value/blinding        : ${vOk && bOk ? 'MATCH' : 'MISMATCH'}`);
console.log(`  stranger can open     : ${leak ? 'YES — BROKEN' : 'no (correct)'}`);
console.log(`  scanBalance gross     : ${scan.balance} (netted=${scan.netted})`);
const ok = pkOk && vOk && bOk && !leak && scan.balance === BigInt(V.expect_value);
console.log(ok ? '\n  ✅ the BUNDLED scanner is correct' : '\n  ❌ bundle is broken');
process.exit(ok ? 0 : 1);
