/* tslint:disable */
/* eslint-disable */

/**
 * Build and PROVE a real shielded-to-shielded send: spend one self-created note,
 * paying `amount` to `recipient_pk_shield_hex`/`recipient_pk_enc_hex` (the recipient's
 * published shielded address — see `GET /v1/shielded/address`) with the rest returned
 * as a change note back to this account. Returns a JSON string with exactly the fields
 * `sigil_api::shielded::ShieldedSendRequest` expects, ready for
 * `POST /v1/shielded_send` with `Content-Type: application/json`.
 *
 * # Parameters
 * - `seed_hex`: this wallet's 32-byte seed (same seed `doShield()` derives the spend
 *   key and blinding from — `sha3_256(mnemonic)` on this page).
 * - `note_index` / `note_value_str`: the derivation index and value of the SELF-CREATED
 *   note being spent (from this wallet's local note-index bookkeeping — see
 *   `noteIndexNext`/`noteRecord` on this page).
 * - `note_position`: that note's leaf position in the pool, found by matching
 *   [`shield_note_commitment`]'s output against `GET /v1/shielded/leaves`'s `leaves`.
 * - `unpadded_leaves_json`: the JSON array of hex commitment strings from that SAME
 *   response's `leaves` field — the real, unpadded leaves only.
 * - `capacity`: that response's `capacity` field. Padding up to it is done HERE (via
 *   the real [`padding_leaf_wire`], not a JS port) rather than shipped over the wire —
 *   the server deliberately omits padding to avoid sending ~1MB of derivable zeros.
 * - `recipient_pk_shield_hex` / `recipient_pk_enc_hex`: the recipient's published
 *   shielded address (`GET /v1/shielded/address?wallet=...` returns both).
 * - `amount_str`: how much to pay the recipient, in atomic units.
 *
 * Errors surface as a JS exception carrying the underlying reason (non-conserving
 * amount, note not found at the claimed position, bad recipient key, etc.) — never a
 * silently wrong proof.
 */
export function buildPrivateSend(seed_hex: string, note_index: number, note_value_str: string, note_position: number, unpadded_leaves_json: string, capacity: number, recipient_pk_shield_hex: string, recipient_pk_enc_hex: string, amount_str: string): string;

/**
 * [`buildPrivateSendReceivedWithMemo`] without a memo.
 */
export function buildPrivateSendReceived(seed_hex: string, note_blinding_hex: string, note_value_str: string, note_position: number, unpadded_leaves_json: string, capacity: number, recipient_pk_shield_hex: string, recipient_pk_enc_hex: string, amount_str: string): string;

/**
 * Spend a note this wallet **received** rather than created — a private payment from
 * someone else, or a mining reward the chain minted straight into the shielded pool.
 *
 * ## Why this had to exist
 *
 * [`buildPrivateSend`] addresses the note being spent by its DERIVATION INDEX, and
 * re-derives the blinding as `blinding(seed, index)`. That works only for notes this
 * wallet minted itself, because only then does an index exist on this side. A received
 * note's blinding was chosen by whoever sealed it; the only place it exists is inside
 * the ciphertext, which [`openNoteCiphertext`] already returns as `blinding_hex`.
 *
 * So until now the browser could SEE a received or mined note — the balance scan
 * trial-decrypts the whole pool and adds it up correctly — and had no way to SPEND it.
 * Reported 2026-09-06 as a wallet showing a real mined balance whose Send button could
 * never find a note: the balance came from the chain, the spend candidates came from a
 * localStorage list of self-created notes, and for a wallet that mined on a phone that
 * list is empty. The native Android wallet never had the gap because it calls
 * `wallet::build_spend` directly, and `OwnedNote::index` is `Option<u64>` precisely so
 * a received note can be held with `None`.
 *
 * Nothing about the PROOF differs: `build_spend` uses the note's blinding and leaf
 * position and never looks at the index. This is the same circuit, the same fee, the
 * same output sealing — only the way the input note is addressed changes.
 *
 * - `note_blinding_hex`: the note's blinding, wire-encoded — exactly the `blinding_hex`
 *   field [`openNoteCiphertext`] returned for this ciphertext.
 * - `note_position`: its leaf position in the pool, i.e. its index in `GET
 *   /v1/shielded/leaves`'s `leaves` array.
 */
export function buildPrivateSendReceivedWithMemo(seed_hex: string, note_blinding_hex: string, note_value_str: string, note_position: number, unpadded_leaves_json: string, capacity: number, recipient_pk_shield_hex: string, recipient_pk_enc_hex: string, amount_str: string, memo: string): string;

/**
 * [`buildPrivateSend`] plus a private memo (UTF-8, at most `note_cipher::MEMO_LEN` = 512
 * bytes) sealed to the recipient alongside the note. A separate export rather than an
 * extra parameter so pages built against the memo-less signature keep working unchanged.
 */
export function buildPrivateSendWithMemo(seed_hex: string, note_index: number, note_value_str: string, note_position: number, unpadded_leaves_json: string, capacity: number, recipient_pk_shield_hex: string, recipient_pk_enc_hex: string, amount_str: string, memo: string): string;

/**
 * The nullifier this wallet's note at pool leaf `position` publishes when spent —
 * `compress2(spend_key, position)`, byte-identical to `note_v1::nullifier` and
 * `wallet::nullifier_at`, wire-encoded like `/v1/shielded/nullifiers` lists them.
 *
 * Exported 2026-09-08 because the page's JS port of this derivation disagreed with the
 * crate (leaf 2438: JS `43cc93…`, chain `1f2a01…`), so the browser never recognised its
 * own spent notes: balances netted nothing and Send kept offering spent notes, which the
 * node refused as "nullifier already spent". One derivation, this one.
 */
export function noteNullifier(seed_hex: string, position: number): string;

/**
 * Trial-open ONE published note ciphertext with this seed's encryption key.
 *
 * The receiving half of private payments, in the browser: a page walks every ciphertext
 * on chain, calls this, and each success is a note that is ours — value, blinding (so the
 * note can be spent) and the memo the sender wrote. Throws for any ciphertext not sealed
 * to us, which is the common case and carries no information.
 */
export function openNoteCiphertext(seed_hex: string, ciphertext_json: string): string;

/**
 * The X25519 note-delivery key ciphertexts are sealed to — the OTHER half of this
 * wallet's shielded address. `pk_shield` alone makes a wallet payable but not
 * notifiable: without publishing this key too, nothing can tell the wallet a payment
 * landed. The wallet page calls this during auto-registration.
 *
 * 2026-08-29: this export existed only in an UNCOMMITTED working tree the 08-26 site
 * build was cut from — the page called it, the committed crate lacked it, and every
 * fresh WASM build silently broke registration. Re-added from the committed
 * `ShieldedAccount::address` derivation, the same one the node and MCP use.
 */
export function shieldEncryptPublicKey(seed_hex: string): string;

/**
 * The commitment this account would publish for a SELF-CREATED note at `index` holding
 * `value` — deterministic from the seed alone. A wallet shields a deposit by computing
 * this locally (already done in JS on this page for `doShield()`'s fixed-denomination
 * path) and later needs the SAME value here to find that note's leaf position by
 * matching it against `GET /v1/shielded/leaves`'s `leaves` array — this function exists
 * so that lookup uses the real prover's own math rather than a second hand-ported copy.
 */
export function shieldNoteCommitment(seed_hex: string, index: number, value_str: string): string;

/**
 * This account's wire-hex `pk_shield` — the circuit key a payer binds an output note
 * to. Publish this (together with an X25519 encryption key) via
 * `POST /v1/shielded/register` so others can pay you privately.
 */
export function shieldPublicKey(seed_hex: string): string;

/**
 * Sanity/self-test entry point for the standalone WASM-loading smoke-test page: no
 * secrets, no proving, just confirms the module loaded and its exports are callable.
 */
export function wasmApiVersion(): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly buildPrivateSend: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number) => [number, number, number, number];
    readonly buildPrivateSendReceived: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number, p: number) => [number, number, number, number];
    readonly buildPrivateSendReceivedWithMemo: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number, p: number, q: number, r: number) => [number, number, number, number];
    readonly buildPrivateSendWithMemo: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number, p: number, q: number) => [number, number, number, number];
    readonly noteNullifier: (a: number, b: number, c: number) => [number, number, number, number];
    readonly openNoteCiphertext: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly shieldEncryptPublicKey: (a: number, b: number) => [number, number, number, number];
    readonly shieldNoteCommitment: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
    readonly shieldPublicKey: (a: number, b: number) => [number, number, number, number];
    readonly wasmApiVersion: () => [number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
