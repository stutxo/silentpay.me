/* tslint:disable */
/* eslint-disable */

export class SilentpayApp {
    free(): void;
    [Symbol.dispose](): void;
    currentPayment(): any;
    exportSnapshot(): any;
    static fromSnapshot(snapshot: any): SilentpayApp;
    constructor();
    paymentQrSvg(): string;
    pollOnce(): Promise<any>;
    recoveryWif(): string;
    reset(): any;
    retryBroadcast(): Promise<any>;
    setRecipient(request: any): any;
}

export function recoveryWifFromSnapshot(snapshot: any): string;

export function start(): void;

export function version(): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_silentpayapp_free: (a: number, b: number) => void;
    readonly recoveryWifFromSnapshot: (a: number, b: number) => void;
    readonly silentpayapp_currentPayment: (a: number, b: number) => void;
    readonly silentpayapp_exportSnapshot: (a: number, b: number) => void;
    readonly silentpayapp_fromSnapshot: (a: number, b: number) => void;
    readonly silentpayapp_new: () => number;
    readonly silentpayapp_paymentQrSvg: (a: number, b: number) => void;
    readonly silentpayapp_pollOnce: (a: number) => number;
    readonly silentpayapp_recoveryWif: (a: number, b: number) => void;
    readonly silentpayapp_reset: (a: number, b: number) => void;
    readonly silentpayapp_retryBroadcast: (a: number) => number;
    readonly silentpayapp_setRecipient: (a: number, b: number, c: number) => void;
    readonly start: () => void;
    readonly version: (a: number) => void;
    readonly rustsecp256k1_v0_10_0_context_create: (a: number) => number;
    readonly rustsecp256k1_v0_10_0_context_destroy: (a: number) => void;
    readonly rustsecp256k1_v0_10_0_default_error_callback_fn: (a: number, b: number) => void;
    readonly rustsecp256k1_v0_10_0_default_illegal_callback_fn: (a: number, b: number) => void;
    readonly __wasm_bindgen_func_elem_1159: (a: number, b: number, c: number, d: number) => void;
    readonly __wasm_bindgen_func_elem_1161: (a: number, b: number, c: number, d: number) => void;
    readonly __wbindgen_export: (a: number, b: number) => number;
    readonly __wbindgen_export2: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_export3: (a: number) => void;
    readonly __wbindgen_export4: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export5: (a: number, b: number) => void;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
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
