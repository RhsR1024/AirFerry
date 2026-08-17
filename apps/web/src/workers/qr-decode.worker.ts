/**
 * QR decode worker — decodes QR codes from a captured video frame.
 *
 * Single backend: FAST — the self-compiled ZXing-C++ → WASM module
 * (`fastzxing/airferry_zxing.js`, built by scripts/build-fastzxing.sh with
 * Emscripten 3.1.64, -O3 + SIMD). It reads a raw Y (luminance) plane — no
 * RGBA conversion, ~4× less data across the postMessage boundary. The former
 * `zxing-wasm` compat fallback was removed (FAST-only policy): the build now
 * FAILS when the fast artifacts are missing instead of silently shipping a
 * slow path (see apps/web/scripts/prepare-wasm.cjs).
 *
 * ## Protocol
 * - main → `{type:"init"}`: load the FAST module
 * - main → `{type:"decode", width, height, yPlane, format:"Y", jobId}`
 * - worker → `{type:"ready"}` / `{type:"decoded", payloads, jobId}` /
 *   `{type:"error", message}`
 *
 * One frame in flight per worker (the pool keeps N frames across cores).
 */

/// <reference lib="webworker" />

let fastMod: {
  _airferry_wasm_decode_multi_y(
    p: number,
    len: number,
    w: number,
    h: number,
    stride: number,
    outLen: number
  ): number
  _airferry_wasm_free(p: number): void
  _airferry_wasm_abi_version(): number
  _malloc(n: number): number
  _free(p: number): void
  HEAPU8: Uint8Array
  HEAPU32: Uint32Array
} | null = null

/** Load the self-compiled ZXing-C++ WASM (the only backend). */
async function loadFastBackend(): Promise<void> {
  // The fast backend is a generated artifact (build-fastzxing.sh,
  // git-ignored). Its runtime location differs per host build:
  //  - web receiver PROD copies it to the site root (public/ via
  //    prepare-wasm.cjs); the worker lives at assets/<hash>.js, so
  //    "../airferry_zxing.js" resolves to the site root.
  //  - web DEV serves this worker from its source path via /@fs/, where the
  //    artifact sits in the sibling ../fastzxing/ output dir of build-fastzxing.sh.
  // Vite dev answers unknown /@fs/ paths with the SPA index (HTTP 200, HTML
  // body), so a wrong candidate fails at import() time — try each in order.
  // Keep @vite-ignore so Vite doesn't try to bundle it (public runtime asset).
  const candidates = ["../airferry_zxing.js", "../fastzxing/airferry_zxing.js"]
  let mod: { default: unknown } | undefined
  let lastErr: unknown
  for (const rel of candidates) {
    const moduleUrl = new URL(rel, self.location.href).href
    try {
      mod = (await import(/* @vite-ignore */ moduleUrl)) as { default: unknown }
      break
    } catch (e) {
      lastErr = e
    }
  }
  if (!mod) {
    throw lastErr ?? new Error("FAST ZXing 模块未找到")
  }
  const inst = await (mod.default as () => Promise<unknown>)()
  const m = inst as typeof fastMod
  if (!m || m._airferry_wasm_abi_version() !== 1) {
    throw new Error("FAST ZXing ABI 版本不匹配（期望 1）")
  }
  fastMod = m
}

/** Decode all QR codes in a Y (luminance) plane. */
function decodeFastY(
  yPlane: Uint8Array,
  w: number,
  h: number,
  rowStride: number
): Uint8Array[] {
  const payloads: Uint8Array[] = []
  if (!fastMod) return payloads
  const srcPtr = fastMod._malloc(yPlane.length)
  const lenPtr = fastMod._malloc(8)
  // _malloc returns 0 on failure (emscripten) — writing through HEAPU8/HEAPU32
  // with a 0 base would silently corrupt the WASM heap head.
  if (srcPtr === 0 || lenPtr === 0) {
    if (srcPtr !== 0) fastMod._free(srcPtr)
    if (lenPtr !== 0) fastMod._free(lenPtr)
    return payloads
  }
  // Wrap the decode + parse in try/finally so the two input allocations are
  // released even if _airferry_wasm_decode_multi_y traps (a WASM trap on a
  // malformed/corrupt luminance plane is rare but possible). The WASM heap only
  // grows, never shrinks, so leaking w*h+8 bytes per trap would accumulate
  // unbounded over a long scan session.
  try {
    fastMod.HEAPU8.set(yPlane, srcPtr)
    fastMod.HEAPU32[lenPtr >> 2] = 0
    fastMod.HEAPU32[(lenPtr >> 2) + 1] = 0
    const outPtr = fastMod._airferry_wasm_decode_multi_y(
      srcPtr,
      yPlane.length,
      w,
      h,
      rowStride,
      lenPtr
    )
    const outLen = fastMod.HEAPU32[lenPtr >> 2]
    if (outPtr !== 0 && outLen > 0) {
      const packed = fastMod.HEAPU8.subarray(outPtr, outPtr + outLen)
      const count = packed[0] | (packed[1] << 8) | (packed[2] << 16) | (packed[3] << 24)
      let off = 4
      for (let i = 0; i < count; i++) {
        // A truncated record (length header or payload running past the
        // packed buffer) means a corrupt write from the native side — stop
        // parsing instead of emitting silently truncated payloads.
        if (off + 4 > packed.length) break
        const len =
          packed[off] | (packed[1 + off] << 8) | (packed[2 + off] << 16) | (packed[3 + off] << 24)
        off += 4
        if (off + len + 16 > packed.length) break
        // AF2 wire frame minimum size is 30 B (Header 26 B + Frame CRC 4 B)
        if (len >= 30) payloads.push(packed.slice(off, off + len))
        off += len + 16 // payload + 4×s32 bbox
      }
      fastMod._airferry_wasm_free(outPtr)
    }
  } finally {
    fastMod._free(srcPtr)
    fastMod._free(lenPtr)
  }
  return payloads
}

function post(msg: unknown): void {
  ;(postMessage as (m: unknown) => void)(msg)
}

self.addEventListener("message", async (e: MessageEvent) => {
  const data = e.data
  if (!data || typeof data !== "object") return

  if (data.type === "init") {
    try {
      await loadFastBackend()
      post({ type: "ready" })
    } catch (err) {
      post({ type: "error", message: `解码器加载失败: ${String(err)}` })
    }
    return
  }

  if (data.type === "decode") {
    const { width, height, yPlane, jobId } = data as {
      width: number
      height: number
      yPlane?: Uint8Array
      jobId: number
    }
    // Always answer: the main thread marks this pool slot busy on dispatch,
    // and a silent return would leave it busy forever (pool shrinks to 0
    // with no error surfaced).
    if (!fastMod || !yPlane) {
      post({ type: "decoded", payloads: [], jobId })
      return
    }
    try {
      // Feed the raw Y (luminance) plane — no RGBA conversion.
      const payloads = decodeFastY(yPlane, width, height, width)
      post({ type: "decoded", payloads, jobId })
    } catch (err) {
      post({
        type: "error",
        message: `解码失败: ${String(err)}`,
        jobId,
      })
    }
    return
  }
})
