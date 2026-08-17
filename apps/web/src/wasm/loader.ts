/**
 * Loader + thin wrapper around the Rust `transfer_engine` WASM module.
 *
 * `npm run wasm` produces the Rust package. The `@airferry-wasm` alias points
 * to the scalar WASM package in `wasm-pkg/`. It exposes `SenderBuilderWasm`
 * and `encode_qr`; initialization is lazy.
 *
 * Standalone (single-file) build: under `file://`, `fetch()` of the `.wasm`
 * asset is blocked, so the standalone build inlines the WASM as a base64
 * constant on `globalThis.__WASM_TRANSFER_ENGINE__`. When present we decode it
 * and pass the buffer directly to `init(buffer)` — the wasm-bindgen glue routes
 * any non-string/URL/Request input straight to `WebAssembly.instantiate(buffer)`,
 * bypassing fetch entirely.
 */
import init, {
  SenderBuilderWasm,
  SenderSessionWasm,
  ReceiverSessionWasm,
  encode_qr,
  encode_chunk_balanced,
  plan_chunks,
} from "@airferry-wasm/transfer_engine.js"
import { base64ToBuffer } from "./base64"

let initPromise: Promise<void> | null = null

/** Initialize the WASM module exactly once. */
export function ensureWasm(): Promise<void> {
  if (!initPromise) {
    // Standalone build inlines the wasm as base64 (file:// can't fetch it).
    // When absent (extension / normal web), pass nothing → the glue derives the
    // URL from import.meta.url and fetches normally.
    const standaloneB64 =
      (globalThis as { __WASM_TRANSFER_ENGINE__?: string }).__WASM_TRANSFER_ENGINE__
    const pending = init(base64ToBuffer(standaloneB64)).then(() => undefined)
    let retryable: Promise<void>
    retryable = pending.catch((error: unknown) => {
      // A transient fetch/instantiation failure must not poison every later
      // retry for the lifetime of the page.
      if (initPromise === retryable) initPromise = null
      throw error
    })
    initPromise = retryable
  }
  return initPromise
}

export {
  SenderBuilderWasm,
  SenderSessionWasm,
  ReceiverSessionWasm,
  encode_qr,
  encode_chunk_balanced,
  plan_chunks,
}
