package com.airferry.app.nativelib

/**
 * JNI bridge to the Rust `transfer_engine` library (libtransfer_engine.so).
 *
 * Native methods read/write Kotlin `ByteArray`s via the JNIEnv function table
 * (GetByteArrayRegion / SetByteArrayRegion) — the standard, ABI-stable path.
 * The handle is a raw pointer stored as Long.
 */
object NativeBridge {
    init {
        System.loadLibrary("transfer_engine")
    }

    /**
     * Native ABI / protocol capability version (see
     * `AIRFERRY_NATIVE_ABI_VERSION` in `core/transfer-engine/src/jni.rs`).
     * - 1: legacy v1 (pre-AF2) segmented / large-file receive path.
     * - 2: the 16 per-field receiver getters were replaced by the single
     *   [receiverSnapshotJson] (`ReceiverSnapshotV2`).
     * - 3: bounded-memory incremental §13 final verification.
     * - 4: sender-side bindings added (`senderBuildStreamed`/`senderNextQr`/
     *   `senderStageChunk` + prep helpers); receiver API unchanged.
     * A stale `.so` either lacks this symbol (calling it throws
     * `UnsatisfiedLinkError`) or reports an older version — either way the
     * host must refuse to run instead of silently "staying synchronising".
     */
    const val NATIVE_ABI_VERSION = 4

    /** Report the native ABI / protocol capability version. */
    external fun nativeAbiVersion(): Int

    /** Create a receiver session. Returns an opaque pointer (Long). */
    external fun receiverCreate(
        sessionIdLo: Long,
        sessionIdHi: Long
    ): Long

    /**
     * Ingest a frame. Returns a packed status word (see [IngestStatus]) instead
     * of a per-frame JSON string: the UI refreshes only ~7 Hz, so building and
     * parsing a JSON on every decoded frame is wasted work. The packed word
     * carries completion, accepted-flag, mismatch streak, and received-symbol
     * count — enough for the ingest path to decide completion + re-init. Fetch
     * the full progress via [receiverProgressJson] at the UI cadence.
     */
    external fun receiverIngest(handle: Long, frameBytes: ByteArray): Long

    /**
     * On-demand progress query (NUL-terminated JSON byte[], or empty on error).
     * Call at the UI refresh cadence (~7 Hz), not per-frame.
     */
    external fun receiverProgressJson(handle: Long): ByteArray?

    external fun receiverIsComplete(handle: Long): Int

    /**
     * Single-JSON receiver snapshot (`ReceiverSnapshotV2`): every AF2
     * snapshot field (name/sizes/CRC/codec, session id, manifest/chunk
     * metadata) in ONE atomic call, replacing the former 16 per-field
     * getters. Parse with `JSONObject`.
     * Null only on a null handle / string failure.
     */
    external fun receiverSnapshotJson(handle: Long): String?

    /**
     * Recover the assembled file as a freshly-allocated `byte[]`, or an empty
     * array / null if not complete. Single atomic call (replaces the old
     * length+fill pair that truncated > 2 GB files via a `jint` length and had a
     * length/fill race).
     */
    external fun receiverAssembleBytes(handle: Long): ByteArray?

    /** Reassemble chunk `index` bytes. */
    external fun receiverAssembleChunk(handle: Long, index: Int): ByteArray?

    /**
     * Index of the chunk completed by the most recent ChunkReady frame, or -1.
     * Pair with [receiverAssembleChunk] + [receiverForgetChunk] to persist
     * chunks incrementally and keep native memory bounded by one chunk.
     */
    external fun receiverLastChunkIndex(handle: Long): Int

    /**
     * Release a persisted chunk from native memory (eviction). Returns true
     * when the chunk was resident. Completion tracking is unaffected.
     */
    external fun receiverForgetChunk(handle: Long, index: Int): Boolean

    /** Verify a staged raw chunk against the ROOT-bound Manifest table (§11). */
    external fun receiverVerifyChunk(handle: Long, index: Int, rawBytes: ByteArray): Boolean

    /** Run §13 ⑧⑨ integrity chain over the reassembled canonical stream. */
    external fun receiverVerifyFinalStream(handle: Long, streamBytes: ByteArray): Boolean

    /** Begin bounded-memory §13 ⑧⑨ final verification. */
    external fun receiverFinalVerifyBegin(handle: Long): Boolean

    /** Feed the next contiguous canonical-stream block. */
    external fun receiverFinalVerifyFeed(handle: Long, streamBytes: ByteArray): Boolean

    /** Finish bounded-memory §13 ⑧⑨ final verification. */
    external fun receiverFinalVerifyFinish(handle: Long): Boolean

    /** Restore receiver from stored ROOT frame bytes + completed chunk indices (§12 resume). */
    external fun receiverResume(handle: Long, rootFrameBytes: ByteArray, completedIndices: IntArray): Boolean

    /**
     * Evict one chunk from both ledgers after a spill re-verification failure
     * (§11/§12): the sender's next epoch re-supplies it.
     */
    external fun receiverInvalidateChunk(handle: Long, index: Int): Boolean

    external fun receiverDestroy(handle: Long)

    // ===== sender side (ABI 4) =====
    // Mirrors `SenderSessionWasm` in core/transfer-engine/src/wasm.rs; all
    // logic lives in `sender_host.rs`, these are thin handles. Sessions are
    // NOT thread-safe — serialize all calls on one handle with one lock.

    /**
     * Canonical chunk layout WITHOUT reading content. Parallel arrays describe
     * the items (`kinds`: 1=file, 2=utf8_text, 3=dir; `paths`/`sizes` align
     * positionally). Returns `{"chunks":[[item,start,len,...],...]}` — flat
     * triples per chunk — or throws IllegalStateException.
     */
    external fun senderPlanChunks(
        kinds: ByteArray,
        paths: Array<String>,
        sizes: LongArray,
        chunkRawSize: Int
    ): String

    /**
     * Streamed (bounded-memory) sender build. `contentHashes` is the packed
     * 32×N BLAKE3 table aligned with the meta arrays; `chunkHashes` is the
     * 32×M table aligned with the [senderPlanChunks] output. Returns the
     * session handle, or throws IllegalStateException on failure.
     */
    external fun senderBuildStreamed(
        kinds: ByteArray,
        paths: Array<String>,
        sizes: LongArray,
        contentHashes: ByteArray,
        chunkHashes: ByteArray,
        symbolSize: Int,
        chunkRawSize: Int,
        redundancyPct: Int
    ): Long

    external fun senderDestroy(handle: Long)

    /**
     * Pull the next QR batch: packed `u32le count`, then per tile
     * `u32le side` + `side²` 0/1 module bytes (1 = dark) — the same layout the
     * web sender's `next_qr_scratch` produces. Throws IllegalStateException
     * with message `AF2_CHUNK_NOT_STAGED:<index>` when the playlist reaches an
     * unstaged chunk (stage it via [senderStageChunk] and retry; the failed
     * call has no side effects).
     */
    external fun senderNextQr(handle: Long, count: Int): ByteArray

    /**
     * Stage one encoded chunk (codec 0=RAW, 1=Zstd, 2=Xz). `rawHash` is the
     * host-precomputed BLAKE3 of the RAW chunk (32 bytes; empty = hash
     * in-core). Throws IllegalStateException on validation failure.
     */
    external fun senderStageChunk(
        handle: Long,
        index: Int,
        codecId: Int,
        bytes: ByteArray,
        rawHash: ByteArray
    ): Boolean

    /** Playlist position hint: chunk index inside a chunk window, -1 during bootstrap. */
    external fun senderCurrentChunkIndex(handle: Long): Int

    /** 1-based broadcast epoch. */
    external fun senderEpoch(handle: Long): Int

    /** True while chunk [index] still holds prefetched staged bytes. */
    external fun senderIsStaged(handle: Long, index: Int): Boolean

    /**
     * On-demand stats JSON (`frames/fps/throughput_bps/bytes/elapsed_ms`).
     * Call at the UI refresh cadence (~4 Hz), not per frame.
     */
    external fun senderStatsJson(handle: Long): String?

    external fun senderTransferIdHex(handle: Long): String?

    // ===== prep helpers (ABI 4) =====

    /** Streaming BLAKE3 hasher: create → update* → digest (digest DESTROYS the handle). */
    external fun blake3Create(): Long

    external fun blake3Update(handle: Long, bytes: ByteArray)

    /** Finalize and DESTROY the hasher; returns the 32-byte digest. */
    external fun blake3Digest(handle: Long): ByteArray

    /**
     * `encode_chunk_balanced` (§10.1 balanced policy) packed with a 1-byte
     * codec id prefix followed by the data. `channelBps` = fps × symbolSize ×
     * QR count (0 disables p6 escalation); `forceFull` for single-chunk
     * transfers.
     */
    external fun encodeChunkBalanced(
        raw: ByteArray,
        channelBps: Long,
        forceFull: Boolean
    ): ByteArray
}
