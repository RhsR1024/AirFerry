/**
 * Y-plane extraction worker (OffscreenCanvas).
 *
 * Draws the transferred ImageBitmap snapshot of a video frame and converts
 * RGBA → a tightly-packed Y plane (rowStride == width, what the FAST ZXing
 * Y-plane decoder expects). Previously this ran on the MAIN thread inside the
 * rVFC capture tick: getImageData reads back ~8 MB and the RGBA→Y loop walks
 * ~2M pixels per frame, and the per-frame allocations fed periodic major-GC
 * hitches that starved the capture/decode pipeline (visible as periodic
 * receive-rate dips).
 *
 * One message per frame; the host keeps at most one frame in flight (drop
 * otherwise), so no queue builds up here. ALWAYS replies with `yplane` or
 * `yerror` — the host reserved a decode-pool slot for this frame and only
 * the reply frees it.
 */

/// <reference lib="webworker" />

let canvas: OffscreenCanvas | null = null
let ctx: OffscreenCanvasRenderingContext2D | null = null
let canvasW = 0
let canvasH = 0

function post(msg: unknown, transfer: Transferable[] = []): void {
  ;(postMessage as unknown as (m: unknown, t?: Transferable[]) => void)(msg, transfer)
}

self.addEventListener("message", (e: MessageEvent) => {
  const d = e.data
  if (!d || d.type !== "convert") return
  const bitmap = d.bitmap as ImageBitmap | null | undefined
  const jobId = d.jobId as number
  const qrSlot = d.qrSlot as number
  try {
    if (!bitmap || bitmap.width <= 0 || bitmap.height <= 0) {
      post({ type: "yerror", message: "invalid bitmap", jobId, qrSlot })
      return
    }
    const w = bitmap.width
    const h = bitmap.height
    if (!canvas || canvasW !== w || canvasH !== h) {
      canvas = new OffscreenCanvas(w, h)
      ctx = canvas.getContext("2d", { willReadFrequently: true })
      canvasW = w
      canvasH = h
    }
    const c = ctx
    if (!c) {
      post({ type: "yerror", message: "no 2d context", jobId, qrSlot })
      return
    }
    c.drawImage(bitmap, 0, 0, w, h)
    const img = c.getImageData(0, 0, w, h)
    const rgba = img.data
    const y = new Uint8Array(w * h)
    // BT.601 integer luma (matches the previous main-thread implementation —
    // the receivers' white-balance/contrast behaviour depends on it).
    for (let i = 0; i < w * h; i++) {
      const o = i * 4
      y[i] = (rgba[o] * 77 + rgba[o + 1] * 150 + rgba[o + 2] * 29 + 128) >> 8
    }
    post({ type: "yplane", yPlane: y, width: w, height: h, jobId, qrSlot }, [
      y.buffer as ArrayBuffer,
    ])
  } catch (err) {
    post({ type: "yerror", message: err instanceof Error ? err.message : String(err), jobId, qrSlot })
  } finally {
    try {
      bitmap?.close()
    } catch {
      /* already closed */
    }
  }
})
