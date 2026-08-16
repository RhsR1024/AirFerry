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
     * A stale `.so` either lacks this symbol (calling it throws
     * `UnsatisfiedLinkError`) or reports an older version — either way the
     * host must refuse to run instead of silently "staying synchronising".
     */
    const val NATIVE_ABI_VERSION = 2

    /** Report the native ABI / protocol capability version. */
    external fun nativeAbiVersion(): Int

    /** Create a receiver session. Returns an opaque pointer (Long). */
    external fun receiverCreate(
        sessionIdLo: Long,
        sessionIdHi: Long,
        totalBlocks: Int,
        totalSymbols: Int,
        symbolSize: Int
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

    external fun receiverDestroy(handle: Long)
}
