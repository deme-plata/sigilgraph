global.window = {};
const src = require('fs').readFileSync('/tmp/claude-0/-home-storage-claude-code/e4b37e21-5a56-49a6-8ca7-a73433160e1c/scratchpad/extracted.js','utf8');
(0, eval)(src);
const S = global.window.__sigilShieldScan;
if (!S) { console.log('✗ inlined bundle did not expose the API'); process.exit(1); }
const V = JSON.parse(process.argv[2]);
const hex2b = h => Uint8Array.from(h.match(/../g).map(x => parseInt(x,16)));
const id = S.encIdentityFromSeed(hex2b(V.seed_hex));
const n = S.decodeNote(S.openEnvelope(V.ciphertext, id));
const ok = n && n.value === BigInt(V.expect_value) && n.blinding === BigInt(V.expect_blinding_str);
console.log(`  API exposed from the PAGE : yes`);
console.log(`  opens the Rust note       : ${ok ? 'MATCH' : 'MISMATCH'}`);
console.log(ok ? '\n  ✅ the scanner works as inlined in the wallet page' : '\n  ❌ inlining broke it');
process.exit(ok ? 0 : 1);
