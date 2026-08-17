/**
 * AF2 Receive worker.
 *
 * Ingests AF2 frame byte arrays into `ReceiverSessionWasm`, handles manifest
 * readiness and chunk delivery, and posts structured results back to main.
 */

/// <reference lib="webworker" />

import { ReceiverSessionWasm, ensureWasm } from "@/wasm/loader"

export const KIND_FILE = 1
export const KIND_UTF8_TEXT = 2
export const KIND_DIRECTORY = 3

export interface ManifestEntryDto {
  kind: number
  path: string
  /** §7.2 save-time sanitized name (equals path when nothing needed fixing). */
  save_path?: string
  offset: number
  size: number
}

export interface MetaInfo {
  transferIdHex: string
  contentIdHex: string
  totalRawSize: number
  entryCount: number
  chunkCount: number
  chunkRawSize: number
  /** Wire symbol size T observed by the Rust receiver (0 before lock). */
  symbolSize: number
  metaConfirmed: boolean
  /** v1-magic frames rejected so far; > 0 ⇒ the peer runs protocol 1. */
  legacyPeerFrames: number
  entries: ManifestEntryDto[]
}

export interface RecoveredText {
  kind: "text"
  text: string
  validUtf8: boolean
  name?: string
}

export interface RecoveredFile {
  kind: "file"
  name: string
  data: Uint8Array
}

export interface RecoveredBundle {
  kind: "bundle"
  entries: RecoveredFile[]
}

export type Recovered = RecoveredText | RecoveredFile | RecoveredBundle

let session: ReceiverSessionWasm | null = null
let activeJobId = -1
let lastMetaSent = false
let totalAcceptedSymbols = 0
let receivedChunks: Map<number, Uint8Array> = new Map()

function post(msg: unknown, transfer: Transferable[] = []): void {
  ;(postMessage as (m: unknown, transfer?: Transferable[]) => void)(msg, transfer)
}

function dropSession(): void {
  if (session) {
    try {
      session.free()
    } catch (_) {}
    session = null
  }
  lastMetaSent = false
  totalAcceptedSymbols = 0
  receivedChunks.clear()
}

function readMeta(s: ReceiverSessionWasm): MetaInfo {
  const snap = JSON.parse(s.snapshot_json()) as {
    schema_version: number
    meta_confirmed: boolean
    transfer_id_hex: string
    content_id_hex: string
    total_raw_size: number
    entry_count: number
    chunk_count: number
    chunk_raw_size: number
    symbol_size?: number
    legacy_peer_frames?: number
    entries?: ManifestEntryDto[]
  }
  return {
    transferIdHex: snap.transfer_id_hex || "",
    contentIdHex: snap.content_id_hex || "",
    totalRawSize: Number(snap.total_raw_size || 0),
    entryCount: snap.entry_count || 0,
    chunkCount: snap.chunk_count || 0,
    chunkRawSize: snap.chunk_raw_size || 0,
    symbolSize: Number(snap.symbol_size || 0),
    metaConfirmed: snap.meta_confirmed === true,
    legacyPeerFrames: Number(snap.legacy_peer_frames || 0),
    entries: Array.isArray(snap.entries) ? snap.entries : [],
  }
}

function ingestBatch(frames: Uint8Array[], jobId: number): {
  complete: boolean
  acceptedCount: number
  snapshot: Record<string, unknown>
} {
  if (!session) {
    session = new ReceiverSessionWasm()
  }

  let acceptedCount = 0
  for (const frame of frames) {
    const rawWord = session.ingest(frame)
    // Unpack 64-bit status word (BigInt in JS from wasm64)
    const word = typeof rawWord === "bigint" ? rawWord : BigInt(rawWord)
    const ERROR_RECEIVED = 0xFFFFFFFFn
    if (((word >> 32n) & 0xFFFFFFFFn) === ERROR_RECEIVED) {
      continue
    }
    const accepted = ((word >> 1n) & 1n) !== 0n
    const manifestReady = ((word >> 2n) & 1n) !== 0n
    const chunkReady = ((word >> 3n) & 1n) !== 0n
    const receivedSymbols = Number((word >> 32n) & 0xFFFFFFFFn)

    if (accepted) {
      acceptedCount++
      totalAcceptedSymbols++
    }
    if (accepted && receivedSymbols === 0) {
      // Relocked in native AF2:
      receivedChunks.clear()
      lastMetaSent = false
      totalAcceptedSymbols = 0
      post({ type: "relock", jobId })
    }
    if (manifestReady) {
      maybePostMeta(jobId)
    }
    if (chunkReady) {
      const idx = session.last_chunk_index()
      const bytes = new Uint8Array(session.assemble_chunk(idx))
      if (bytes.length > 0) {
        receivedChunks.set(idx, bytes)
        session.forget_chunk(idx)
      }
      maybePostMeta(jobId)
    }
  }

  const meta = readMeta(session)
  const isComplete =
    meta.metaConfirmed &&
    meta.chunkCount > 0 &&
    receivedChunks.size >= meta.chunkCount

  // Symbol totals are estimates derived from the observed wire T (exact
  // per-chunk K lives inside the Rust decoder and chunk compression shrinks
  // the encoded size, so raw-size/T is the honest upper bound). T is 0 only
  // against a stale WASM build that predates the symbol_size snapshot field —
  // fall back to the legacy 1024 guess there so progress keeps rendering.
  const t = meta.symbolSize > 0 ? meta.symbolSize : 1024
  const totalSymbols =
    meta.totalRawSize > 0 ? Math.ceil(meta.totalRawSize / t) : meta.chunkCount * 1024
  const decodedSymbols = Math.min(
    receivedChunks.size * Math.ceil(meta.chunkRawSize / t),
    totalSymbols
  )

  const snapshot = {
    totalSymbols,
    decodedSymbols,
    receivedSymbols: totalAcceptedSymbols,
    decodedBlocks: receivedChunks.size,
    totalBlocks: meta.chunkCount,
    decodedFraction: meta.chunkCount > 0 ? receivedChunks.size / meta.chunkCount : 0,
    framesSeen: 0,
    framesDuplicate: 0,
    framesCorrupt: 0,
    metaConfirmed: meta.metaConfirmed,
    symbolSize: meta.symbolSize,
    legacyPeerFrames: meta.legacyPeerFrames,
    complete: isComplete,
  }

  return { complete: isComplete, acceptedCount, snapshot }
}

function maybePostMeta(jobId: number): void {
  if (!session || lastMetaSent) return
  const meta = readMeta(session)
  if (!meta.metaConfirmed) return

  const nonDirEntries = meta.entries.filter((e) => e.kind !== KIND_DIRECTORY)
  let fileName = "文件传输"
  if (nonDirEntries.length === 1) {
    fileName = nonDirEntries[0].path
  } else if (nonDirEntries.length > 1) {
    fileName = `多文件传输包 (${nonDirEntries.length} 项)`
  } else if (meta.entryCount > 1) {
    fileName = `多文件传输包 (${meta.entryCount} 项)`
  }

  post({
    type: "meta",
    meta: {
      ...meta,
      fileName,
      originalSize: meta.totalRawSize,
      compressedSize: meta.totalRawSize,
      compressedSizeKnown: false,
      segmented: false,
      rootId: meta.transferIdHex,
    },
    jobId,
  })
  lastMetaSent = true
}

self.addEventListener("message", async (e: MessageEvent) => {
  const data = e.data
  if (!data || typeof data !== "object") return

  if (data.type === "init") {
    try {
      await ensureWasm()
      dropSession()
      activeJobId = data.jobId ?? 0
      post({ type: "ready" })
    } catch (err) {
      post({ type: "error", message: `WASM 初始化失败: ${String(err)}` })
    }
    return
  }

  if (data.type === "reset") {
    dropSession()
    activeJobId = data.jobId ?? activeJobId
    post({ type: "reset-ack", jobId: activeJobId })
    return
  }

  if (data.type === "frames") {
    const { frames, jobId } = data as { frames: Uint8Array[]; jobId: number }
    if (jobId !== undefined && jobId !== activeJobId) return
    try {
      const res = ingestBatch(frames, activeJobId)
      post({
        type: "status",
        complete: res.complete,
        acceptedCount: res.acceptedCount,
        snapshot: res.snapshot,
        nowMs: Date.now(),
        jobId: activeJobId,
      })
    } catch (err) {
      // Without this the failure becomes an unhandled rejection inside the
      // worker and the UI stays on "扫描中" forever with no error shown.
      post({
        type: "error",
        message: `帧处理失败: ${err instanceof Error ? err.message : String(err)}`,
        jobId: activeJobId,
      })
    }
    return
  }

  if (data.type === "assemble") {
    if (!session) return
    try {
      const meta = readMeta(session)
      // Every chunk index must be present. A size-only check would silently
      // assemble a short stream (shifting every entry slice) if the ledger
      // somehow had a hole with extra out-of-range indices.
      for (let i = 0; i < meta.chunkCount; i++) {
        if (!receivedChunks.has(i)) {
          post({
            type: "error",
            message: `分块 ${i + 1}/${meta.chunkCount} 缺失，无法组装`,
            jobId: activeJobId,
          })
          return
        }
      }

      // 1. Assemble Canonical Content Stream from chunks
      let totalLen = 0
      for (let i = 0; i < meta.chunkCount; i++) {
        totalLen += receivedChunks.get(i)!.byteLength
      }
      // Canonical chunk lengths sum to exactly total_raw_size; anything else
      // means the ledger is inconsistent and entry slicing would be garbage.
      if (meta.totalRawSize > 0 && totalLen !== meta.totalRawSize) {
        post({
          type: "error",
          message: `分块总长 ${totalLen} 与清单声明 ${meta.totalRawSize} 不一致`,
          jobId: activeJobId,
        })
        return
      }
      const stream = new Uint8Array(totalLen)
      let offset = 0
      for (let i = 0; i < meta.chunkCount; i++) {
        const chunk = receivedChunks.get(i)!
        stream.set(chunk, offset)
        offset += chunk.byteLength
      }

      // 1b. Final integrity gate (§13 ⑧⑨): verify full stream through Rust core
      if (!session.verify_final_stream(stream)) {
        post({
          type: "error",
          message: "传输终验失败：条目哈希、UTF-8 或 Content ID 校验未通过",
          jobId: activeJobId,
        })
        return
      }

      // 2. Materialize entries from the Manifest entry table
      const entries = meta.entries.filter((e) => e.kind !== KIND_DIRECTORY)
      let recovered: Recovered

      if (entries.length === 1 && entries[0].kind === KIND_UTF8_TEXT) {
        const e0 = entries[0]
        const slice = stream.subarray(e0.offset, e0.offset + e0.size)
        let text = ""
        let validUtf8 = true
        try {
          text = new TextDecoder("utf-8", { fatal: true }).decode(slice)
        } catch {
          text = new TextDecoder("utf-8").decode(slice)
          validUtf8 = false
        }
        recovered = {
          kind: "text",
          text,
          validUtf8,
          name: e0.path,
        }
      } else if (entries.length === 1) {
        const e0 = entries[0]
        const slice = stream.subarray(e0.offset, e0.offset + e0.size)
        recovered = {
          kind: "file",
          name: e0.path,
          data: slice,
        }
      } else {
        const bundleEntries = entries.map((e) => ({
          kind: "file" as const,
          name: e.path,
          data: stream.subarray(e.offset, e.offset + e.size),
        }))
        recovered = {
          kind: "bundle",
          entries: bundleEntries,
        }
      }

      // All result payloads are subarray views of the single `stream` buffer,
      // so there is exactly one transferable — the shared ArrayBuffer. Listing
      // it once per bundle entry would duplicate the transferable, which
      // postMessage rejects with DataCloneError (killing the result silently
      // inside this async handler).
      const transferables: Transferable[] =
        recovered.kind === "text" ? [] : [stream.buffer]

      dropSession()
      post(
        {
          type: "result",
          recovered,
          meta,
          jobId: activeJobId,
        },
        transferables
      )
    } catch (err) {
      // Covers snapshot JSON.parse failures, allocation failures and decoder
      // exceptions — without this the UI stays on "恢复中" forever.
      post({
        type: "error",
        message: `组装失败: ${err instanceof Error ? err.message : String(err)}`,
        jobId: activeJobId,
      })
    }
  }
})
