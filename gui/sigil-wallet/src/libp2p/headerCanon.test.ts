/**
 * Cross-language conformance test for headerCanon.ts.
 *
 * The expected JSON strings and hashes below are NOT hand-computed — they
 * are copied verbatim from the printed output of
 * `crates/sigil-header/src/lib.rs`'s `js_port_conformance_fixture` test
 * (`fluxc test -p sigil-header js_port_conformance_fixture -- --nocapture`,
 * run 2026-08-23). If this test ever needs updating, regenerate it from that
 * Rust test — never hand-edit the expected values.
 */
import { describe, it, expect } from 'vitest'
import { headerCanonicalJson, headerHash, headerSigningBytesHash, type SigilBlockHeaderV0 } from './headerCanon'

// Deterministic byte-pattern generators matching the Rust fixture exactly.
function parentPattern(): number[] {
  return Array.from({ length: 32 }, (_, i) => i + 1)
}
function mergePattern(): number[] {
  return Array.from({ length: 32 }, (_, i) => 200 + i)
}
function noncePattern(): number[] {
  return Array.from({ length: 292 }, (_, i) => (i * 7 + 3) % 256)
}
function fill(byte: number): number[] {
  return Array.from({ length: 32 }, () => byte)
}

function buildHeader(topologyCommitment: number[] | null): SigilBlockHeaderV0 {
  return {
    version: 0,
    network_id: Array.from(new TextEncoder().encode('sigil-g0')),
    height: 2003042,
    parent_hash: parentPattern(),
    merge_parents: [mergePattern()],
    timestamp_ms: 1787500000123,
    nonce_sqisign: noncePattern(),
    vdf_input: [
      146, 96, 22, 61, 174, 233, 226, 167, 255, 86, 139, 211, 135, 63, 22, 33,
      235, 169, 50, 74, 107, 203, 115, 212, 52, 123, 193, 86, 196, 164, 217, 80,
    ],
    vdf_proof: { y: [1, 2, 3, 4], pi: [5, 6, 7], t: 12345 },
    difficulty: 987654,
    wallet_state_root: fill(11),
    dex_state_root: fill(22),
    event_log_root: fill(33),
    contract_state_root: fill(44),
    state_transition_proof: { bytes: [9, 8, 7], public_inputs_hash: fill(55) },
    txs_merkle_root: fill(66),
    tx_count: 17,
    fluxc_artifact_proof: {
      artifact_blake3: fill(77),
      sqisign_sig: [1, 1, 2, 3, 5],
      sqisign_pubkey: [8, 13, 21],
      settle_tx: fill(88),
    },
    sig_scheme: 'Ed25519Hot',
    producer: fill(99),
    producer_sig: Array.from({ length: 64 }, () => 0xab),
    topology_commitment: topologyCommitment,
  }
}

// Verbatim from the Rust test's CANONICAL_JSON= output (topology_commitment: Some).
const EXPECTED_JSON_SOME =
  '{"version":0,"network_id":[115,105,103,105,108,45,103,48],"height":2003042,"parent_hash":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"merge_parents":[[200,201,202,203,204,205,206,207,208,209,210,211,212,213,214,215,216,217,218,219,220,221,222,223,224,225,226,227,228,229,230,231]],"timestamp_ms":1787500000123,"nonce_sqisign":[3,10,17,24,31,38,45,52,59,66,73,80,87,94,101,108,115,122,129,136,143,150,157,164,171,178,185,192,199,206,213,220,227,234,241,248,255,6,13,20,27,34,41,48,55,62,69,76,83,90,97,104,111,118,125,132,139,146,153,160,167,174,181,188,195,202,209,216,223,230,237,244,251,2,9,16,23,30,37,44,51,58,65,72,79,86,93,100,107,114,121,128,135,142,149,156,163,170,177,184,191,198,205,212,219,226,233,240,247,254,5,12,19,26,33,40,47,54,61,68,75,82,89,96,103,110,117,124,131,138,145,152,159,166,173,180,187,194,201,208,215,222,229,236,243,250,1,8,15,22,29,36,43,50,57,64,71,78,85,92,99,106,113,120,127,134,141,148,155,162,169,176,183,190,197,204,211,218,225,232,239,246,253,4,11,18,25,32,39,46,53,60,67,74,81,88,95,102,109,116,123,130,137,144,151,158,165,172,179,186,193,200,207,214,221,228,235,242,249,0,7,14,21,28,35,42,49,56,63,70,77,84,91,98,105,112,119,126,133,140,147,154,161,168,175,182,189,196,203,210,217,224,231,238,245,252,3,10,17,24,31,38,45,52,59,66,73,80,87,94,101,108,115,122,129,136,143,150,157,164,171,178,185,192,199,206,213,220,227,234,241,248],"vdf_input":[146,96,22,61,174,233,226,167,255,86,139,211,135,63,22,33,235,169,50,74,107,203,115,212,52,123,193,86,196,164,217,80],"vdf_proof":{"y":[1,2,3,4],"pi":[5,6,7],"t":12345},"difficulty":987654,"wallet_state_root":[11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11],"dex_state_root":[22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22,22],"event_log_root":[33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33,33],"contract_state_root":[44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44,44],"state_transition_proof":{"bytes":[9,8,7],"public_inputs_hash":[55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55,55]},"txs_merkle_root":[66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66,66],"tx_count":17,"fluxc_artifact_proof":{"artifact_blake3":[77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77,77],"sqisign_sig":[1,1,2,3,5],"sqisign_pubkey":[8,13,21],"settle_tx":[88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88,88]},"sig_scheme":"Ed25519Hot","producer":[99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99],"producer_sig":[171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171],"topology_commitment":[111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111,111]}'

const EXPECTED_HASH_SOME = '307a8f9201b73fe1cd33b126aa8acae62d1c671181df6872d67b7d5176790908'
const EXPECTED_SIGNING_BYTES_HASH_SOME = '24aeb9cb39b986e504af25847bcb9820e57c34f6915d1cc88e3c6a999032c911'
const EXPECTED_HASH_NONE = '4bd711fcecd323797234e04c6cb92745918fe993ed184741b62fe5491d3f9ead'

describe('headerCanon — cross-language conformance vs. real Rust output', () => {
  it('produces byte-identical canonical JSON to serde_json (topology_commitment: Some)', () => {
    const h = buildHeader(fill(111))
    expect(headerCanonicalJson(h)).toBe(EXPECTED_JSON_SOME)
  })

  it('hash() matches the real BLAKE3 hash Rust computed for the same header', () => {
    const h = buildHeader(fill(111))
    expect(headerHash(h)).toBe(EXPECTED_HASH_SOME)
  })

  it('signing_bytes() hash matches (producer_sig zeroed before hashing)', () => {
    const h = buildHeader(fill(111))
    expect(headerSigningBytesHash(h)).toBe(EXPECTED_SIGNING_BYTES_HASH_SOME)
  })

  it('hash() matches when topology_commitment is None (null-strip path)', () => {
    const h = buildHeader(null)
    expect(headerHash(h)).toBe(EXPECTED_HASH_NONE)
  })

  it('Some and None fixtures hash differently (the field is load-bearing, not silently dropped)', () => {
    expect(headerHash(buildHeader(fill(111)))).not.toBe(headerHash(buildHeader(null)))
  })
})
