/**
 * Play-time chunk staging bridge for streamed AF2 senders.
 *
 * The WASM sender holds no content (bounded memory): each chunk must be
 * staged via `stage_chunk` before the playlist reaches it. This class glues
 * the synchronous rAF render loop to the async preparation worker:
 *
 *  - `tick()` runs before every render and (a) RECONCILES the armed set
 *    against the core's `is_staged()` — armed entries are dropped exactly
 *    when the core has consumed the staged bytes, never on window-key
 *    changes (consumption timing differs between mid-list advances and
 *    single-chunk epoch wraps; see `Af2Sender::is_staged`), and (b) prefetches
 *    the WRAPPED next chunk so the epoch restart back to chunk 0 is covered.
 *    It does NOT gate rendering: a genuinely unstaged current chunk surfaces
 *    as the `AF2_CHUNK_NOT_STAGED:<i>` marker from `next_qr_scratch`, which
 *    the transactional sender makes side-effect-free.
 *  - `handleNotStaged()` recognizes that marker, drops the armed entry and
 *    requests the stage; the next tick continues the exact frame sequence.
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
  let bootstrapped = false

  const onMessage = (e: MessageEvent) => {
    const d = e.data
    if (!d || typeof d !== "object" || d.jobId !== opts.jobId) return
    if (d.type === "staged") {
      inflight.delete(d.index as number)
      if (disposed || !opts.isLive()) return
      try {
        opts.session.stage_chunk(
          d.index as number,
          d.codec as number,
          d.data as Uint8Array,
          (d.rawHash as Uint8Array | undefined) ?? new Uint8Array(0)
        )
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

  return {
    tick(session: SenderSessionWasm): boolean {
      if (disposed || !opts.isLive()) return true
      const cur = session.current_chunk_index()
      if (cur >= 0) {
        const next = (cur + 1) % opts.chunkCount
        // Reconcile FIRST: an armed entry whose core slot was consumed (the
        // window's META emission) is stale and re-arms for the next epoch;
        // one that is still staged must NOT be re-requested (that was the
        // per-epoch redundant round-trip / stall bug at single-chunk wraps).
        if (armed.has(cur) && !session.is_staged(cur)) armed.delete(cur)
        if (armed.has(next) && !session.is_staged(next)) armed.delete(next)
        // Only the actual next window is prefetched. For one-chunk transfers
        // next === cur, so this still stages the next epoch; for multi-chunk
        // transfers it avoids retaining every already-visited chunk until the
        // following epoch (which would grow staged memory with file size).
        requestStage(next)
      } else if (!bootstrapped && opts.chunkCount > 0) {
        // Bootstrap: prefetch chunks 0/1 once, before the first window
        // starts (later needs always surface as handled not-staged markers).
        bootstrapped = true
        requestStage(0)
        requestStage(1)
      }
      // Never block rendering: the live window's encoder is authoritative,
      // and re-arming the NEXT epoch's copy must not freeze the current one.
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
