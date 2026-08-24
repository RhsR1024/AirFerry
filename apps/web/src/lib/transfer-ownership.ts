/**
 * Ownership helpers for values crossing JS/WASM and Worker boundaries.
 * A consumed wasm-bindgen handle and a transferred ArrayBuffer must never be
 * reused by the sender pipeline.
 */

export interface ConsumingEncodedChunk {
  readonly codec_id: number
  into_data(): Uint8Array
}

/** Read the codec before consuming the handle, then consume it exactly once. */
export function consumeEncodedChunk(
  encoded: ConsumingEncodedChunk
): { codec: number; data: Uint8Array } {
  const codec = encoded.codec_id
  const data = encoded.into_data()
  return { codec, data }
}

/** Retain bytes beyond a postMessage transfer of the caller's buffer. */
export function retainBytes(bytes: Uint8Array): Uint8Array {
  return bytes.slice()
}

/** A transfer list may contain a given ArrayBuffer at most once. */
export function uniqueTransferBuffers(
  views: readonly Uint8Array[]
): ArrayBuffer[] {
  const unique = new Set<ArrayBuffer>()
  for (const view of views) unique.add(view.buffer as ArrayBuffer)
  return [...unique]
}
