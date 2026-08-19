/**
 * Play-time chunk staging bridge for streamed AF2 senders.
 *
 * The WASM sender holds no content (bounded memory): each chunk must be
 * staged via `stage_chunk` before the playlist reaches it. This class glues
 * the synchronous rAF render loop to the async preparation worker:
 *
 *  - `tick()` runs before every render: it prefetches the chunk AFTER the
 *    current one — WITH wraparound, so the epoch restart back to chunk 0 is
 *    also covered (for single-chunk transfers every window boundary is an
 *    epoch wrap) — and reports whether a needed stage is still in flight
 *    (render must skip those ticks).
 *  - `handleNotStaged()` recognizes the `AF2_CHUNK_NOT_STAGED:<i>` marker
 *    from `next_qr_scratch` and requests the stage; the failed call had no
 *    side effects (transactional sender), so the next tick continues the
 *    exact frame sequence.
 *
 * The `armed` set prevents re-staging a chunk that is already staged for its
 * next need: without it every tick would re-issue the previous stage request
 * (an 8 MiB read + encode + main-thread validation per round-trip — a
 * continuous IO hammer and periodic main-thread hitches). A chunk re-arms
 * only after its window consumed the staged bytes or a not-staged marker
 * proved them gone.
 *
 * Worker responses call `session.stage_chunk` directly; `isLive` guards
 * against a stale stager outliving its session/worker.
 */

import type { SenderSessionWasm } from "@/wasm/loader"

const NOT_STAGED_RE = /^AF2_CHUNK_NOT_STAGED:(\d+)$/

export interface ChunkStagerOptions {
  worker: Worker
  session: SenderSessionWasm
  jobId: number
  chunkCount: number
  /** False once the owning playback session is no longer current. */
  isLive: () => boolean
  /** Called when a stage failed terminally (read error / bad hash). */
  onFatal?: (message: string) => void
}

export interface ChunkStager {
  /** Returns false when a required stage is still in flight (skip render). */
  tick(session: SenderSessionWasm): boolean
  /** True when the error was the not-staged marker and was handled. */
  handleNotStaged(err: unknown): boolean
  dispose(): void
}

export function createChunkStager(opts: ChunkStagerOptions): ChunkStager {
  const inflight = new Set<number>()
  /** Staged into the session and not yet consumed by a window. */
  const armed = new Set<number>()
  let disposed = false

  const onMessage = (e: MessageEvent) => {
    const d = e.data
    if (!d || typeof d !== "object" || d.jobId !== opts.jobId) return
    if (d.type === "staged") {
      inflight.delete(d.index as number)
      if (disposed || !opts.isLive()) return
      try {
        opts.session.stage_chunk(d.index as number, d.codec as number, d.data as Uint8Array)
        armed.add(d.index as number)
      } catch (err) {
        opts.onFatal?.(err instanceof Error ? err.message : String(err))
      }
    } else if (d.type === "stageError") {
      inflight.delete(d.index as number)
      if (disposed || !opts.isLive()) return
      opts.onFatal?.(`分块 ${Number(d.index) + 1} 读取失败: ${String(d.message)}`)
    }
  }
  opts.worker.addEventListener("message", onMessage)

  const requestStage = (index: number): void => {
    if (
      disposed ||
      index < 0 ||
      index >= opts.chunkCount ||
      inflight.has(index) ||
      armed.has(index)
    ) {
      return
    }
    inflight.add(index)
    opts.worker.postMessage({ type: "stage", jobId: opts.jobId, index })
  }

  // (epoch, chunk) uniquely identifies the active window — a single-chunk
  // transfer keeps chunk index 0 across every epoch wrap.
  //
  // INVARIANT behind armed.delete(cur) on a key change: Af2Sender consumes
  // the staged bytes (ensure_chunk_encoder) BEFORE writing the new
  // PlaylistState — both in advance_past_chunk's mid-list branch and its
  // epoch-wrap branch (the failure case leaves the state untouched). The
  // instant current_chunk_index()/epoch() report a NEW key, the chunk's
  // staged bytes are therefore already consumed and the armed entry is
  // stale by definition; deleting it re-arms the slot for the next epoch.
  let lastWindowKey = -1

  return {
    tick(session: SenderSessionWasm): boolean {
      if (disposed || !opts.isLive()) return true
      const cur = session.current_chunk_index()
      if (cur >= 0) {
        const key = session.epoch() * 0x1_0000_0000 + cur
        if (key !== lastWindowKey) {
          // A new window started: its META emission consumed the staged
          // bytes — re-arm the slot so the next epoch prefetches again.
          lastWindowKey = key
          armed.delete(cur)
        }
        // Prefetch the next window's chunk, wrapping to 0 at the epoch
        // boundary (covers the epoch restart back to chunk 0, and the
        // single-chunk case where next == current).
        requestStage((cur + 1) % opts.chunkCount)
        if (inflight.has(cur)) return false
      } else if (!armed.size && !inflight.size && opts.chunkCount > 0) {
        // Bootstrap: prefetch chunks 0/1 once, before the first window
        // starts (later needs always surface as handled not-staged markers).
        requestStage(0)
        requestStage(1)
      }
      return true
    },
    handleNotStaged(err: unknown): boolean {
      const msg = err instanceof Error ? err.message : typeof err === "string" ? err : ""
      const m = NOT_STAGED_RE.exec(msg)
      if (!m) return false
      const index = Number(m[1])
      armed.delete(index)
      requestStage(index)
      return true
    },
    dispose(): void {
      disposed = true
      opts.worker.removeEventListener("message", onMessage)
      inflight.clear()
      armed.clear()
    },
  }
}
