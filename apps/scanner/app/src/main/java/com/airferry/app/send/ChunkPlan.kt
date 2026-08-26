package com.airferry.app.send

import org.json.JSONObject

/** One `(item index, offset within item, length)` slice of the canonical stream. */
data class ChunkSegment(val item: Int, val start: Long, val len: Long)

/**
 * Parsed `NativeBridge.senderPlanChunks` output
 * (`{"chunks":[[item,start,len,...],...]}` — flat triples per chunk, item
 * indices referring to the host's original array order).
 * Pure JVM logic — unit-tested without a device.
 */
data class ChunkPlan(val chunks: List<List<ChunkSegment>>) {
    val chunkCount: Int get() = chunks.size

    /** Total raw bytes of one chunk (last chunk is usually short). */
    fun rawSizeOf(chunkIndex: Int): Long = chunks[chunkIndex].sumOf { it.len }

    companion object {
        fun parse(json: String): ChunkPlan {
            val root = JSONObject(json)
            val arr = root.getJSONArray("chunks")
            val chunks = ArrayList<List<ChunkSegment>>(arr.length())
            for (ci in 0 until arr.length()) {
                val flat = arr.getJSONArray(ci)
                require(flat.length() % 3 == 0) { "chunk $ci segment array not triple-aligned" }
                val segs = ArrayList<ChunkSegment>(flat.length() / 3)
                var i = 0
                while (i < flat.length()) {
                    val item = flat.getInt(i)
                    val start = flat.getLong(i + 1)
                    val len = flat.getLong(i + 2)
                    require(item >= 0 && start >= 0 && len > 0) {
                        "chunk $ci bad segment ($item,$start,$len)"
                    }
                    segs += ChunkSegment(item, start, len)
                    i += 3
                }
                chunks += segs
            }
            require(chunks.isNotEmpty()) { "empty chunk plan" }
            return ChunkPlan(chunks)
        }
    }
}
