/**
 * AF2 Receive worker (with §12 OPFS bounded-memory spill + crash-safe resume).
 *
 * Ingests AF2 frame byte arrays into `ReceiverSessionWasm`, spills completed
 * chunks to Origin Private File System (`af2-<tid>.partial`) via synchronous
 * access handles, journals completed bits to `af2-<tid>.ledger.jsonl`, and
 * materializes entries at completion without holding the full canonical stream
 * in memory. Falls back to an in-memory Map when OPFS is unavailable.
 */

/// <reference lib="webworker" />

import { ReceiverSessionWasm, ensureWasm } from "@/wasm/loader"

export const KIND_FILE = 1
export const KIND_UTF8_TEXT = 2
export const KIND_DIRECTORY = 3

const FINAL_VERIFY_MEM_LIMIT = 64 * 1024 * 1024

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
  /** Canonical ROOT frame bytes re-encoded for the §12 resume ledger. */
  rootFrameHex: string
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

// ---------------------------------------------------------------------------
// OPFS / in-memory chunk storage abstraction
// ---------------------------------------------------------------------------

interface SyncHandleLike {
  read(buf: ArrayBufferView, options?: { at?: number }): number
  write(buf: ArrayBufferView, options?: { at?: number }): number
  flush(): void
  close(): void
  getSize(): number
}

class ChunkStore {
  private memory = new Map<number, Uint8Array>()
  private opfsDir: FileSystemDirectoryHandle | null = null
  private opfsFile: FileSystemFileHandle | null = null
  private syncHandle: SyncHandleLike | null = null
  private transferId = ""
  private completedIndices = new Set<number>()

  async init(dir: FileSystemDirectoryHandle | null, transferIdHex: string): Promise<void> {
    this.opfsDir = dir
    this.transferId = transferIdHex
    if (!dir || !transferIdHex) return
    try {
      const fileName = `af2-${transferIdHex}.partial`
      this.opfsFile = await dir.getFileHandle(fileName, { create: true })
      // createSyncAccessHandle is synchronous once acquired (Worker-only API).
      if (typeof (this.opfsFile as any).createSyncAccessHandle === "function") {
        this.syncHandle = await (this.opfsFile as any).createSyncAccessHandle()
      }
    } catch {
      this.syncHandle = null
    }
  }

  writeChunk(index: number, chunkRawSize: number, bytes: Uint8Array): void {
    this.completedIndices.add(index)
    if (this.syncHandle && chunkRawSize > 0) {
      try {
        const at = index * chunkRawSize
        this.syncHandle.write(bytes, { at })
        this.syncHandle.flush()
        return
      } catch {
        // Fall back to memory on write error
      }
    }
    this.memory.set(index, bytes)
  }

  readRange(offset: number, size: number, totalRawSize: number, chunkRawSize: number): Uint8Array | null {
    if (size <= 0 || offset < 0) return new Uint8Array(0)
    if (this.syncHandle) {
      try {
        const out = new Uint8Array(size)
        const read = this.syncHandle.read(out, { at: offset })
        if (read === size) return out
      } catch {
        // fall back to memory
      }
    }
    // In-memory slicing fallback
    if (this.memory.size === 0) return null
    const out = new Uint8Array(size)
    let copied = 0
    while (copied < size) {
      const currentOffset = offset + copied
      const chunkIdx = Math.floor(currentOffset / chunkRawSize)
      const chunkOffset = currentOffset % chunkRawSize
      const chunk = this.memory.get(chunkIdx)
      if (!chunk) return null
      const toCopy = Math.min(size - copied, chunk.byteLength - chunkOffset)
      out.set(chunk.subarray(chunkOffset, chunkOffset + toCopy), copied)
      copied += toCopy
    }
    return out
  }

  readChunk(index: number, chunkRawSize: number, totalRawSize: number): Uint8Array | null {
    const off = index * chunkRawSize
    const len = Math.min(chunkRawSize, Math.max(0, totalRawSize - off))
    return this.readRange(off, len, totalRawSize, chunkRawSize)
  }

  has(index: number): boolean {
    return this.completedIndices.has(index)
  }

  get completedCount(): number {
    return this.completedIndices.size
  }

  get completedList(): number[] {
    return Array.from(this.completedIndices).sort((a, b) => a - b)
  }

  markResumed(indices: number[]): void {
    for (const i of indices) this.completedIndices.add(i)
  }

  invalidate(index: number): void {
    this.completedIndices.delete(index)
    this.memory.delete(index)
  }

  async discard(): Promise<void> {
    this.memory.clear()
    this.completedIndices.clear()
    if (this.syncHandle) {
      try {
        this.syncHandle.close()
      } catch {}
      this.syncHandle = null
    }
    if (this.opfsDir && this.transferId) {
      try {
        await this.opfsDir.removeEntry(`af2-${this.transferId}.partial`)
      } catch {}
    }
  }
}

// ---------------------------------------------------------------------------
// §12 OPFS Journal
// ---------------------------------------------------------------------------

class OpfsJournal {
  private opfsDir: FileSystemDirectoryHandle | null = null
  private transferId = ""
  private journalFile: FileSystemFileHandle | null = null

  async init(dir: FileSystemDirectoryHandle | null, transferIdHex: string, crs: number, rootHex: string): Promise<void> {
    this.opfsDir = dir
    this.transferId = transferIdHex
    if (!dir || !transferIdHex) return
    try {
      const fileName = `af2-${transferIdHex}.ledger.jsonl`
      this.journalFile = await dir.getFileHandle(fileName, { create: true })
      // Header: line 1
      const header = JSON.stringify({ v: 1, tid: transferIdHex, crs, root: rootHex }) + "\n"
      const w = await (this.journalFile as any).createWritable({ keepExistingData: false })
      await w.write(header)
      await w.close()
    } catch {
      this.journalFile = null
    }
  }

  async commit(index: number): Promise<void> {
    if (!this.journalFile) return
    try {
      const w = await (this.journalFile as any).createWritable({ keepExistingData: true })
      const size = (await this.journalFile.getFile()).size
      await w.seek(size)
      await w.write(JSON.stringify({ c: index }) + "\n")
      await w.close()
    } catch {}
  }

  async invalidate(index: number): Promise<void> {
    if (!this.journalFile) return
    try {
      const w = await (this.journalFile as any).createWritable({ keepExistingData: true })
      const size = (await this.journalFile.getFile()).size
      await w.seek(size)
      await w.write(JSON.stringify({ i: index }) + "\n")
      await w.close()
    } catch {}
  }

  async discard(): Promise<void> {
    if (this.opfsDir && this.transferId) {
      try {
        await this.opfsDir.removeEntry(`af2-${this.transferId}.ledger.jsonl`)
      } catch {}
    }
  }

  static async loadMostRecent(dir: FileSystemDirectoryHandle | null): Promise<{
    transferIdHex: string
    rootFrameBytes: Uint8Array
    chunkRawSize: number
    completed: number[]
  } | null> {
    if (!dir) return null
    try {
      let latestFile: FileSystemFileHandle | null = null
      let latestMtime = 0
      for await (const [name, handle] of (dir as any).entries()) {
        if (typeof name === "string" && name.endsWith(".ledger.jsonl") && handle.kind === "file") {
          const file = await handle.getFile()
          if (file.lastModified > latestMtime) {
            latestMtime = file.lastModified
            latestFile = handle
          }
        }
      }
      if (!latestFile) return null
      const text = await (await latestFile.getFile()).text()
      const lines = text.split("\n").filter((l) => l.trim().length > 0)
      if (lines.length === 0) return null
      const header = JSON.parse(lines[0])
      if (!header.tid || !header.root) return null
      const completed = new Set<number>()
      for (let i = 1; i < lines.length; i++) {
        try {
          const o = JSON.parse(lines[i])
          if (typeof o.c === "number") completed.add(o.c)
          if (typeof o.i === "number") completed.delete(o.i)
        } catch {
          // skip torn line
        }
      }
      return {
        transferIdHex: header.tid,
        rootFrameBytes: hexToBytes(header.root),
        chunkRawSize: header.crs || 8 * 1024 * 1024,
        completed: Array.from(completed).sort((a, b) => a - b),
      }
    } catch {
      return null
    }
  }
}

function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) return new Uint8Array(0)
  const bytes = new Uint8Array(hex.length / 2)
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.substring(i * 2, i * 2 + 2), 16)
  }
  return bytes
}

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

let session: ReceiverSessionWasm | null = null
let activeJobId = -1
let lastMetaSent = false
let totalAcceptedSymbols = 0
let opfsDirHandle: FileSystemDirectoryHandle | null = null
let chunkStore = new ChunkStore()
let journal = new OpfsJournal()
let pendingReverify: Set<number> | null = null
let resumeChecked = false

async function getOpfsDir(): Promise<FileSystemDirectoryHandle | null> {
  if (opfsDirHandle) return opfsDirHandle
  try {
    if (typeof navigator !== "undefined" && navigator.storage && typeof navigator.storage.getDirectory === "function") {
      opfsDirHandle = await navigator.storage.getDirectory()
      return opfsDirHandle
    }
  } catch {}
  return null
}

function post(msg: unknown, transfer: Transferable[] = []): void {
  ;(postMessage as (m: unknown, transfer?: Transferable[]) => void)(msg, transfer)
}

async function dropSession(): Promise<void> {
  if (session) {
    try {
      session.free()
    } catch (_) {}
    session = null
  }
  lastMetaSent = false
  totalAcceptedSymbols = 0
  pendingReverify = null
  await chunkStore.discard()
  await journal.discard()
}

function readMeta(s: ReceiverSessionWasm): MetaInfo {
  const snap = JSON.parse(s.snapshot_json()) as {
    schema_version: number
    meta_confirmed: boolean
    transfer_id_hex: string
    content_id_hex: string
    root_frame_hex?: string
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
    rootFrameHex: snap.root_frame_hex || "",
    entries: Array.isArray(snap.entries) ? snap.entries : [],
  }
}

async function tryResume(): Promise<void> {
  if (resumeChecked || (session && session.is_complete())) return
  resumeChecked = true
  const dir = await getOpfsDir()
  const latest = await OpfsJournal.loadMostRecent(dir)
  if (!latest) return
  if (!session) session = new ReceiverSessionWasm()
  const ok = session.resume(latest.rootFrameBytes, new Uint32Array(latest.completed))
  if (ok) {
    await chunkStore.init(dir, latest.transferIdHex)
    chunkStore.markResumed(latest.completed)
    pendingReverify = new Set(latest.completed)
    await journal.init(dir, latest.transferIdHex, latest.chunkRawSize, "")
  }
}

async function reverifyResumedChunks(meta: MetaInfo): Promise<void> {
  if (!pendingReverify || pendingReverify.size === 0 || !session || !meta.metaConfirmed) return
  for (const idx of Array.from(pendingReverify)) {
    const chunkBytes = chunkStore.readChunk(idx, meta.chunkRawSize, meta.totalRawSize)
    if (!chunkBytes) continue
    pendingReverify.delete(idx)
    if (!session.verify_chunk(idx, chunkBytes)) {
      session.invalidate_chunk(idx)
      chunkStore.invalidate(idx)
      await journal.invalidate(idx)
    }
  }
  if (pendingReverify.size === 0) pendingReverify = null
}

async function ingestBatch(frames: Uint8Array[], jobId: number): Promise<{
  complete: boolean
  acceptedCount: number
  snapshot: Record<string, unknown>
}> {
  if (!session) {
    session = new ReceiverSessionWasm()
  }
  if (!resumeChecked) {
    await tryResume()
  }

  let acceptedCount = 0
  for (const frame of frames) {
    const rawWord = session.ingest(frame)
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
      // Relocked in native AF2: discard old storage and journal
      await chunkStore.discard()
      await journal.discard()
      lastMetaSent = false
      lastPostedFileName = ""
      lastPostedMetaTid = ""
      totalAcceptedSymbols = 0
      pendingReverify = null
      post({ type: "relock", jobId })
    }
    // Also post initial meta when ROOT locks (entry count + total size available)
    if (accepted && !lastMetaSent) {
      maybePostMeta(jobId)
    }
    if (manifestReady) {
      const m = readMeta(session)
      await reverifyResumedChunks(m)
      maybePostMeta(jobId)
    }
    if (chunkReady) {
      const idx = session.last_chunk_index()
      const bytes = new Uint8Array(session.assemble_chunk(idx))
      if (bytes.length > 0) {
        const snap = readMeta(session)
        if (!chunkStore.has(idx)) {
          const dir = await getOpfsDir()
          await chunkStore.init(dir, snap.transferIdHex)
          chunkStore.writeChunk(idx, snap.chunkRawSize, bytes)
          if (snap.rootFrameHex) {
            await journal.init(dir, snap.transferIdHex, snap.chunkRawSize, snap.rootFrameHex)
          }
          await journal.commit(idx)
        }
        session.forget_chunk(idx)
      }
      maybePostMeta(jobId)
    }
  }

  const meta = readMeta(session)
  // Completion requires the decoded Manifest (entries non-empty): the core may
  // report all chunks done BEFORE the Manifest object is recovered. Staging
  // without the entry table would fail the final gate (or emit an empty
  // bundle) — keep ingesting instead; the manifest interleave delivers it and
  // every later batch re-announces complete=true.
  const isComplete =
    meta.metaConfirmed &&
    meta.chunkCount > 0 &&
    meta.entries.length > 0 &&
    chunkStore.completedCount >= meta.chunkCount

  const t = meta.symbolSize > 0 ? meta.symbolSize : 1024
  const totalSymbols =
    meta.totalRawSize > 0 ? Math.ceil(meta.totalRawSize / t) : meta.chunkCount * 1024
  const decodedSymbols = Math.min(
    chunkStore.completedCount * Math.ceil(meta.chunkRawSize / t),
    totalSymbols
  )

  const nonDirEntries = meta.entries.filter((e) => e.kind !== KIND_DIRECTORY)
  let currentFileName = ""
  if (nonDirEntries.length === 1) {
    currentFileName = nonDirEntries[0].save_path || nonDirEntries[0].path || "文件传输"
  } else if (nonDirEntries.length > 1) {
    currentFileName = `多文件传输包 (${nonDirEntries.length} 项)`
  } else if (meta.entryCount > 1) {
    currentFileName = `多文件传输包 (${meta.entryCount} 项)`
  } else if (meta.totalRawSize > 0) {
    currentFileName = "文件传输"
  }

  const snapshot = {
    totalSymbols,
    decodedSymbols,
    receivedSymbols: totalAcceptedSymbols,
    decodedBlocks: chunkStore.completedCount,
    totalBlocks: meta.chunkCount,
    decodedFraction: meta.chunkCount > 0 ? chunkStore.completedCount / meta.chunkCount : 0,
    framesSeen: 0,
    framesDuplicate: 0,
    framesCorrupt: 0,
    metaConfirmed: meta.metaConfirmed,
    symbolSize: meta.symbolSize,
    legacyPeerFrames: meta.legacyPeerFrames,
    complete: isComplete,
    fileName: currentFileName,
    fileSize: meta.totalRawSize,
    totalRawSize: meta.totalRawSize,
    transferIdHex: meta.transferIdHex,
    entryCount: meta.entryCount,
    chunkCount: meta.chunkCount,
  }

  return { complete: isComplete, acceptedCount, snapshot }
}

let lastPostedFileName = ""
let lastPostedMetaTid = ""

function maybePostMeta(jobId: number): void {
  if (!session) return
  const meta = readMeta(session)
  if (meta.totalRawSize === 0 && !meta.metaConfirmed) return

  const nonDirEntries = meta.entries.filter((e) => e.kind !== KIND_DIRECTORY)
  let fileName = "文件传输"
  if (nonDirEntries.length === 1) {
    fileName = nonDirEntries[0].save_path || nonDirEntries[0].path || "文件传输"
  } else if (nonDirEntries.length > 1) {
    fileName = `多文件传输包 (${nonDirEntries.length} 项)`
  } else if (meta.entryCount > 1) {
    fileName = `多文件传输包 (${meta.entryCount} 项)`
  }

  // Only post when we have new metadata (e.g. initial ROOT lock or refined Manifest filename)
  if (
    lastPostedMetaTid === meta.transferIdHex &&
    lastPostedFileName === fileName &&
    lastMetaSent
  ) {
    return
  }

  lastPostedMetaTid = meta.transferIdHex
  lastPostedFileName = fileName
  if (meta.metaConfirmed) {
    lastMetaSent = true
  }

  const payload = {
    type: "meta",
    fileName,
    fileSize: meta.totalRawSize,
    totalRawSize: meta.totalRawSize,
    compressedSize: meta.totalRawSize,
    transferIdHex: meta.transferIdHex,
    entryCount: meta.entryCount,
    chunkCount: Math.max(1, meta.chunkCount),
    segmentIndex: 0,
    segmentCount: Math.max(1, meta.chunkCount),
    meta: {
      fileName,
      fileSize: meta.totalRawSize,
      totalRawSize: meta.totalRawSize,
      compressedSize: meta.totalRawSize,
      transferIdHex: meta.transferIdHex,
      entryCount: meta.entryCount,
      chunkCount: Math.max(1, meta.chunkCount),
    },
    jobId,
  }
  post(payload)
}

// ---------------------------------------------------------------------------
// Worker Message Handler
// ---------------------------------------------------------------------------

self.addEventListener("message", async (e: MessageEvent) => {
  const data = e.data
  if (!data || typeof data !== "object") return

  if (data.type === "init") {
    try {
      await ensureWasm()
      await dropSession()
      resumeChecked = false
      activeJobId = typeof data.jobId === "number" ? data.jobId : 0
      post({ type: "ready", jobId: activeJobId })
      post({ type: "init_ok", jobId: activeJobId })
    } catch (err) {
      post({
        type: "error",
        message: `WASM 初始化失败: ${err instanceof Error ? err.message : String(err)}`,
        jobId: activeJobId,
      })
    }
    return
  }

  if (data.type === "reset") {
    await dropSession()
    resumeChecked = false
    activeJobId = typeof data.jobId === "number" ? data.jobId : -1
    return
  }

  if (data.type === "ingest") {
    const frames = data.frames as Uint8Array[]
    const jobId = typeof data.jobId === "number" ? data.jobId : activeJobId
    if (jobId !== activeJobId) return

    try {
      const res = await ingestBatch(frames, jobId)
      post({
        type: "status",
        complete: res.complete,
        acceptedCount: res.acceptedCount,
        snapshot: res.snapshot,
        nowMs: Date.now(),
        jobId: activeJobId,
      })
    } catch (err) {
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
      for (let i = 0; i < meta.chunkCount; i++) {
        if (!chunkStore.has(i)) {
          post({
            type: "error",
            message: `分块 ${i + 1}/${meta.chunkCount} 缺失，无法组装`,
            jobId: activeJobId,
          })
          return
        }
      }

      // §11/§13 Integrity Gate:
      // Transfers ≤ 64 MiB stream through memory for the full §13 ⑧⑨ gate
      // (entry hashes, UTF-8 text, Content ID). Larger transfers verify each
      // chunk individually against the Manifest hash table.
      if (meta.totalRawSize <= FINAL_VERIFY_MEM_LIMIT) {
        const stream = chunkStore.readRange(0, meta.totalRawSize, meta.totalRawSize, meta.chunkRawSize)
        if (!stream || !session.verify_final_stream(stream)) {
          post({
            type: "error",
            message: "传输终验失败：条目哈希、UTF-8 或 Content ID 校验未通过",
            jobId: activeJobId,
          })
          return
        }
      } else {
        for (let i = 0; i < meta.chunkCount; i++) {
          const chunk = chunkStore.readChunk(i, meta.chunkRawSize, meta.totalRawSize)
          if (!chunk || !session.verify_chunk(i, chunk)) {
            session.invalidate_chunk(i)
            post({
              type: "error",
              message: `分块 ${i + 1}/${meta.chunkCount} 校验失败，请重新接收`,
              jobId: activeJobId,
            })
            return
          }
        }
      }

      // 2. Materialize entries from the Manifest entry table using save_path
      const entries = meta.entries.filter((e) => e.kind !== KIND_DIRECTORY)
      let recovered: Recovered

      if (entries.length === 1 && entries[0].kind === KIND_UTF8_TEXT) {
        const e0 = entries[0]
        const slice = chunkStore.readRange(e0.offset, e0.size, meta.totalRawSize, meta.chunkRawSize) || new Uint8Array(0)
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
          name: e0.save_path || e0.path,
        }
      } else if (entries.length === 1) {
        const e0 = entries[0]
        const slice = chunkStore.readRange(e0.offset, e0.size, meta.totalRawSize, meta.chunkRawSize) || new Uint8Array(0)
        recovered = {
          kind: "file",
          name: e0.save_path || e0.path,
          data: slice,
        }
      } else {
        const files: RecoveredFile[] = []
        for (const e of entries) {
          const slice = chunkStore.readRange(e.offset, e.size, meta.totalRawSize, meta.chunkRawSize) || new Uint8Array(0)
          files.push({
            kind: "file",
            name: e.save_path || e.path,
            data: slice,
          })
        }
        recovered = {
          kind: "bundle",
          entries: files,
        }
      }

      // Cleanup OPFS storage on successful assemble
      await chunkStore.discard()
      await journal.discard()

      post({
        type: "result",
        recovered,
        jobId: activeJobId,
      })
    } catch (err) {
      post({
        type: "error",
        message: `组装失败: ${err instanceof Error ? err.message : String(err)}`,
        jobId: activeJobId,
      })
    }
  }
})
