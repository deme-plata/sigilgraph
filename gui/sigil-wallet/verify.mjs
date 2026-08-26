import { encIdentityFromSeed, openEnvelope, decodeNote, hex2b, b2hex } from './scan.mjs';
const V = JSON.parse(process.argv[2]);
const seed = hex2b(V.seed_hex);
const id = encIdentityFromSeed(seed);
let ok = true;
const pkOk = b2hex(id.pk) === V.pk_enc_hex;
console.log(`  pk_enc derived in JS  : ${b2hex(id.pk)}`);
console.log(`  pk_enc from Rust      : ${V.pk_enc_hex}`);
console.log(`  -> ${pkOk ? 'MATCH' : 'MISMATCH'}`); ok &&= pkOk;
const note = decodeNote(openEnvelope(V.ciphertext, id));
if (!note) { console.log('  ✗ FAILED to open the Rust-sealed envelope'); process.exit(1); }
const vOk = note.value === BigInt(V.expect_value);
const bOk = note.blinding === BigInt(V.expect_blinding_str);
console.log(`  value    JS=${note.value} Rust=${V.expect_value} -> ${vOk ? 'MATCH' : 'MISMATCH'}`);
console.log(`  blinding JS=${note.blinding} Rust=${V.expect_blinding_str} -> ${bOk ? 'MATCH' : 'MISMATCH'}`);
ok &&= vOk && bOk;
// a stranger must NOT open it
const stranger = encIdentityFromSeed(new Uint8Array(32).fill(0x11));
const leaked = decodeNote(openEnvelope(V.ciphertext, stranger));
console.log(`  stranger opens it?    : ${leaked ? 'YES — PRIVACY BROKEN' : 'no (correct)'}`);
ok &&= !leaked;
console.log(ok ? '\n  ✅ JS scanner byte-matches the Rust sealed box' : '\n  ❌ MISMATCH');
if (!ok) process.exit(1);

// ── netting check: a note whose nullifier is in the spent set must be excluded ──
import { scanBalance } from './scan.mjs';
const fakeMimc = {
  deriveField: () => 12345n,
  compress2: (k, pos) => (k * 1000n + pos),      // stand-in; only identity matters here
  toWireHex: v => v.toString(16).padStart(64, '0'),
};
const cts = [V.ciphertext];
const grossRun = scanBalance(cts, hex2b(V.seed_hex), {});
const nfHex = fakeMimc.toWireHex(fakeMimc.compress2(12345n, 0n)).toLowerCase();
const nettedRun = scanBalance(cts, hex2b(V.seed_hex), { spent: new Set([nfHex]), mimc: fakeMimc });
console.log(`\n  gross (no spent set)  : ${grossRun.balance}  netted=${grossRun.netted}`);
console.log(`  after netting a spend : ${nettedRun.balance}  netted=${nettedRun.netted}  spentValue=${nettedRun.spentValue}`);
const nettingOk = grossRun.balance === BigInt(V.expect_value)
  && grossRun.netted === false
  && nettedRun.balance === 0n
  && nettedRun.spentValue === BigInt(V.expect_value);
console.log(nettingOk ? '  ✅ spends are netted out, and an un-nettable scan says so'
                      : '  ❌ netting is wrong');
process.exit(nettingOk ? 0 : 1);
