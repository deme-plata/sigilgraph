/**
 * headerCanon.ts — byte-exact JS port of SigilBlockHeaderV0's canonical
 * serialization (Rust: crates/sigil-header/src/lib.rs, `hash()` /
 * `signing_bytes()`).
 *
 * WHY THIS FILE HAS TO BE THIS PICKY: the block hash and the producer's
 * signature are both computed over `BLAKE3(serde_json::to_vec(header))` on
 * the Rust side. If this port emits the fields in a different order, with a
 * different number format, or handles the `topology_commitment: null` case
 * differently, every hash it computes will silently disagree with the real
 * chain — there is no partial credit, a one-byte difference changes the
 * whole hash. So the JSON here is NOT built via `JSON.stringify(obj)` (which
 * would depend on JS object key insertion order matching Rust struct field
 * order exactly, with no compiler to catch a slip) — it's built by
 * explicit, ordered string concatenation, one field at a time, in the exact
 * order `SigilBlockHeaderV0`'s fields are declared in Rust.
 *
 * PROVEN, not assumed: `headerCanon.test.ts` checks this module's output
 * against a real Rust-computed fixture (`sigil-header`'s
 * `js_port_conformance_fixture` test) — both a byte-for-byte string
 * comparison of the canonical JSON AND a hash comparison, for both the
 * `topology_commitment: Some` and `: None` cases. Don't trust this file's
 * correctness from reading it — trust the test.
 */

import { blake3 } from '@noble/hashes/blake3'

/** Mirrors Rust's `WesolowskiProof` (sigil-header/src/lib.rs). */
export interface WesolowskiProof {
  y: number[]
  pi: number[]
  t: number
}

/** Mirrors Rust's `StarkProof`. */
export interface StarkProof {
  bytes: number[]
  /** 32 bytes. */
  public_inputs_hash: number[]
}

/** Mirrors Rust's `ProofBundle`. */
export interface ProofBundle {
  /** 32 bytes. */
  artifact_blake3: number[]
  sqisign_sig: number[]
  sqisign_pubkey: number[]
  /** 32 bytes, or null. */
  settle_tx: number[] | null
}

/** Mirrors Rust's `SigScheme` enum — serializes as its exact variant name. */
export type SigScheme = 'SqiSign5' | 'Dilithium5' | 'Ed25519Hot' | 'HybridSqiEd25519'

/**
 * Mirrors Rust's `SigilBlockHeaderV0` exactly, field for field, in
 * declaration order. All fixed-length byte arrays are plain `number[]` (0-255
 * each) — that's how `serde_json` serializes `[u8; N]` and `Vec<u8>`, as a
 * JSON array of integers, not hex/base64.
 *
 * NOTE on `height`/`timestamp_ms`/`difficulty`/`vdf_proof.t`: these are `u64`
 * in Rust but plain JS `number` here. Safe today (block heights and
 * millisecond timestamps are far below `Number.MAX_SAFE_INTEGER`, 2^53-1),
 * but NOT permanently safe — if any of these fields is ever expected to
 * exceed 2^53, this module needs `bigint` handling before it can be trusted
 * again. Flagged here so a future reader doesn't have to rediscover it.
 */
export interface SigilBlockHeaderV0 {
  version: number
  /** 8 bytes. */
  network_id: number[]
  height: number
  /** 32 bytes. */
  parent_hash: number[]
  /** array of 32-byte arrays. */
  merge_parents: number[][]
  timestamp_ms: number
  /** 292 bytes (SQISIGN_L5_LEN). */
  nonce_sqisign: number[]
  /** 32 bytes. */
  vdf_input: number[]
  vdf_proof: WesolowskiProof
  difficulty: number
  /** 32 bytes. */
  wallet_state_root: number[]
  /** 32 bytes. */
  dex_state_root: number[]
  /** 32 bytes. */
  event_log_root: number[]
  /** 32 bytes. */
  contract_state_root: number[]
  state_transition_proof: StarkProof
  /** 32 bytes. */
  txs_merkle_root: number[]
  tx_count: number
  fluxc_artifact_proof: ProofBundle
  sig_scheme: SigScheme
  /** 32 bytes. */
  producer: number[]
  /** variable length, per `sig_scheme.expected_sig_len()`. */
  producer_sig: number[]
  /** 32 bytes, or null. */
  topology_commitment: number[] | null
}

function u8arr(a: number[]): string {
  return '[' + a.join(',') + ']'
}

function optU8arr(a: number[] | null): string {
  return a === null ? 'null' : u8arr(a)
}

/**
 * The canonical JSON bytes for a header, matching Rust's
 * `serde_json::to_vec(&header)` EXCEPT that a `null` `topology_commitment`
 * is omitted entirely rather than emitted as `"topology_commitment":null` —
 * this is intentionally equivalent to, not merely similar to, what Rust's
 * `strip_null_topology_commitment_for_hashing` produces: since
 * `topology_commitment` is the struct's LAST field, removing the literal
 * substring `,"topology_commitment":null` from the end of the JSON leaves
 * exactly the same bytes as never having emitted the field — there is no
 * edge case where these two approaches diverge.
 *
 * This function is what `headerHash` and `headerSigningBytes` both build on;
 * it is exported separately so a caller can also use it for its own
 * diagnostics (e.g. logging what was actually hashed).
 */
export function headerCanonicalJson(h: SigilBlockHeaderV0): string {
  const parts: string[] = [
    `"version":${h.version}`,
    `"network_id":${u8arr(h.network_id)}`,
    `"height":${h.height}`,
    `"parent_hash":${u8arr(h.parent_hash)}`,
    `"merge_parents":[${h.merge_parents.map(u8arr).join(',')}]`,
    `"timestamp_ms":${h.timestamp_ms}`,
    `"nonce_sqisign":${u8arr(h.nonce_sqisign)}`,
    `"vdf_input":${u8arr(h.vdf_input)}`,
    `"vdf_proof":{"y":${u8arr(h.vdf_proof.y)},"pi":${u8arr(h.vdf_proof.pi)},"t":${h.vdf_proof.t}}`,
    `"difficulty":${h.difficulty}`,
    `"wallet_state_root":${u8arr(h.wallet_state_root)}`,
    `"dex_state_root":${u8arr(h.dex_state_root)}`,
    `"event_log_root":${u8arr(h.event_log_root)}`,
    `"contract_state_root":${u8arr(h.contract_state_root)}`,
    `"state_transition_proof":{"bytes":${u8arr(h.state_transition_proof.bytes)},"public_inputs_hash":${u8arr(h.state_transition_proof.public_inputs_hash)}}`,
    `"txs_merkle_root":${u8arr(h.txs_merkle_root)}`,
    `"tx_count":${h.tx_count}`,
    `"fluxc_artifact_proof":{"artifact_blake3":${u8arr(h.fluxc_artifact_proof.artifact_blake3)},"sqisign_sig":${u8arr(h.fluxc_artifact_proof.sqisign_sig)},"sqisign_pubkey":${u8arr(h.fluxc_artifact_proof.sqisign_pubkey)},"settle_tx":${optU8arr(h.fluxc_artifact_proof.settle_tx)}}`,
    `"sig_scheme":${JSON.stringify(h.sig_scheme)}`,
    `"producer":${u8arr(h.producer)}`,
    `"producer_sig":${u8arr(h.producer_sig)}`,
  ]
  if (h.topology_commitment !== null) {
    parts.push(`"topology_commitment":${u8arr(h.topology_commitment)}`)
  }
  return '{' + parts.join(',') + '}'
}

/** BLAKE3 of the UTF-8 bytes of a string, as a lowercase hex string. */
function blake3HexOf(s: string): string {
  const bytes = blake3(new TextEncoder().encode(s))
  return Array.from(bytes).map((b) => b.toString(16).padStart(2, '0')).join('')
}

/**
 * The block's canonical hash — matches Rust's `SigilBlockHeaderV0::hash()`.
 * Returns lowercase hex (32 bytes / 64 hex chars).
 */
export function headerHash(h: SigilBlockHeaderV0): string {
  return blake3HexOf(headerCanonicalJson(h))
}

/**
 * The exact bytes the producer's signature is computed over — matches
 * Rust's `SigilBlockHeaderV0::signing_bytes()`: the header with
 * `producer_sig` zeroed out (an empty array, NOT zero-filled — Rust zeroes
 * it by replacing with `SignatureBytes(Vec::new())`, an empty vec, which
 * serializes as `[]`).
 */
export function headerSigningBytes(h: SigilBlockHeaderV0): string {
  return headerCanonicalJson({ ...h, producer_sig: [] })
}

/** BLAKE3 hex hash of `headerSigningBytes(h)` — used by [[headerHash]]'s
 * sibling checks and by producer-signature verification (Phase 2). */
export function headerSigningBytesHash(h: SigilBlockHeaderV0): string {
  return blake3HexOf(headerSigningBytes(h))
}
