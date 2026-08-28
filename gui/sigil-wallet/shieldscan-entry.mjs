// Bundle entry: expose the shielded-note scanner + the primitives it needs on window,
// matching how this page already vendors @noble inline (CDN imports are blocked on some
// operator networks — the wallet must work fully offline).
import { scanBalance, encIdentityFromSeed, openEnvelope, decodeNote, fetchShieldedBalance } from './scan.mjs';
window.__sigilShieldScan = { scanBalance, encIdentityFromSeed, openEnvelope, decodeNote, fetchShieldedBalance };
