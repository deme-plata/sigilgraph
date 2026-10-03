/* tslint:disable */
/* eslint-disable */

export class Veil {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * End of a streamed answer: release anything held back.
     */
    finish(): string;
    /**
     * Gate text the USER (or a tool) wrote. JSON:
     * `{"ok":true,"text":"…","hidden":["QNK",…]}` or `{"ok":false,"refused":["MNEMONIC"]}`.
     */
    gate(text: string): string;
    /**
     * How many distinct values this conversation has hidden so far.
     */
    hidden_count(): number;
    /**
     * One egress-ledger JSON line for what was actually sent (the page keeps
     * these in its own storage — "what did my AI send out?").
     * `now_ms` = JavaScript `Date.now()` — wasm has no clock of its own.
     */
    ledger_line(destination: string, sent: string, now_ms: number): string;
    constructor();
    /**
     * Text the user did not write (system prompt, page context): hidden, never refused.
     */
    pseudonymize(text: string): string;
    /**
     * Streaming answer: feed each chunk, show what comes back.
     */
    push(chunk: string): string;
    /**
     * Restore placeholders in a complete answer.
     */
    rehydrate(text: string): string;
}

/**
 * True when the URL's host is this device (localhost, 127.x, ::1) — then the
 * page needs no gate at all: nothing leaves.
 */
export function veil_is_loopback(url: string): boolean;

/**
 * Which installed model rewrites (models = newline-separated names).
 */
export function veil_pick_rewriter(models: string): string;

/**
 * The rewriter's user turn. `history` = lines "role<TAB>content" (oldest first);
 * only user/assistant lines are used, the last 6, each capped at 600 chars.
 */
export function veil_rewrite_prompt(history: string, user: string): string;

/**
 * The rewriter's system prompt.
 */
export function veil_rewrite_system(): string;

/**
 * Vet a rewrite: the cleaned, name-scrubbed question, or "" = do not use it.
 */
export function veil_vet_rewrite(raw: string, original: string): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_veil_free: (a: number, b: number) => void;
    readonly veil_finish: (a: number) => [number, number];
    readonly veil_gate: (a: number, b: number, c: number) => [number, number];
    readonly veil_hidden_count: (a: number) => number;
    readonly veil_is_loopback: (a: number, b: number) => number;
    readonly veil_ledger_line: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number];
    readonly veil_new: () => number;
    readonly veil_pick_rewriter: (a: number, b: number) => [number, number];
    readonly veil_pseudonymize: (a: number, b: number, c: number) => [number, number];
    readonly veil_push: (a: number, b: number, c: number) => [number, number];
    readonly veil_rehydrate: (a: number, b: number, c: number) => [number, number];
    readonly veil_rewrite_prompt: (a: number, b: number, c: number, d: number) => [number, number];
    readonly veil_rewrite_system: () => [number, number];
    readonly veil_vet_rewrite: (a: number, b: number, c: number, d: number) => [number, number];
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
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
