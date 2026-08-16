package com.airferry.app.scan

import java.io.File
import java.io.RandomAccessFile

/**
 * Sparse on-disk staging for completed AF2 chunks — the receiver-side half of
 * the bounded-memory ledger (plan E2).
 *
 * Completed chunks are RAW (post-decode, post-decompress) and fixed-size
 * except the last, so the spill file's layout IS the canonical content
 * stream: chunk `i` lives at byte offset `i * chunkRawSize`. Manifest entries
 * are then sliced straight from the file by offset/size — the full stream
 * never has to exist in memory, and native chunks are evicted as soon as
 * they are spilled ([ReceiverSessionManager.drainLastChunk]).
 *
 * Only ever touched from the ingest thread (the decode pool serializes
 * ingest) and the recovery path that runs under the same lock, so a single
 * [RandomAccessFile] needs no extra synchronization.
 */
class ChunkSpillStore(dir: File, transferIdHex: String) {

    private val path = File(dir, "af2-${transferIdHex.ifEmpty { "session" }}.partial")
    private var raf: RandomAccessFile? = null

    /** pwrite one completed chunk at its canonical-stream offset + fsync. */
    fun write(index: Int, chunkRawSize: Int, bytes: ByteArray) {
        if (index < 0 || chunkRawSize <= 0 || bytes.isEmpty()) return
        val f = raf ?: RandomAccessFile(path, "rw").also { raf = it }
        f.seek(index.toLong() * chunkRawSize.toLong())
        f.write(bytes)
        try {
            f.fd.sync()
        } catch (e: Exception) {
            android.util.Log.w("ChunkSpillStore", "flush failed", e)
        }
    }

    /** Current spill size in bytes (0 when nothing was spilled yet). */
    fun length(): Long = raf?.length() ?: if (path.isFile) path.length() else 0L

    /**
     * Read a canonical-stream range. Returns null when the spill is shorter
     * than the requested range end (incomplete spill) — callers then fall
     * back to the in-memory assemble path.
     */
    fun readRange(offset: Long, size: Long): ByteArray? {
        if (offset < 0 || size < 0 || size > Int.MAX_VALUE) return null
        if (!path.isFile && raf == null) return null
        val f = raf ?: try {
            RandomAccessFile(path, "r").also { raf = it }
        } catch (_: Exception) {
            return null
        }
        if (offset + size > f.length()) return null
        val out = ByteArray(size.toInt())
        f.seek(offset)
        var done = 0
        while (done < out.size) {
            val n = f.read(out, done, out.size - done)
            if (n < 0) return null
            done += n
        }
        return out
    }

    /** Close and delete the spill (transfer relocked / consumed / abandoned). */
    fun discard() {
        try {
            raf?.close()
        } catch (_: Exception) {
        }
        raf = null
        path.delete()
    }
}
