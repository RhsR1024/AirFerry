package com.airferry.app.send

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * JVM tests for the sender's pure parsers: the chunk-plan JSON and the packed
 * QR batch wire format (mirrors the web sender's `parseMultiQrBuf`).
 */
class SendParsersTest {

    @Test
    fun chunkPlanParsesFlatTriples() {
        val plan = ChunkPlan.parse("{\"chunks\":[[1,0,10,0,0,5],[0,5,5]]}")
        assertEquals(2, plan.chunkCount)
        assertEquals(
            listOf(ChunkSegment(1, 0, 10), ChunkSegment(0, 0, 5)),
            plan.chunks[0]
        )
        assertEquals(listOf(ChunkSegment(0, 5, 5)), plan.chunks[1])
        assertEquals(15L, plan.rawSizeOf(0))
        assertEquals(5L, plan.rawSizeOf(1))
    }

    @Test(expected = IllegalArgumentException::class)
    fun chunkPlanRejectsMisalignedSegmentArray() {
        ChunkPlan.parse("{\"chunks\":[[0,0]]}")
    }

    @Test(expected = IllegalArgumentException::class)
    fun chunkPlanRejectsEmptyPlan() {
        ChunkPlan.parse("{\"chunks\":[]}")
    }

    @Test(expected = Exception::class)
    fun chunkPlanRejectsGarbage() {
        ChunkPlan.parse("not json")
    }

    private fun packedBatch(vararg sides: Int): ByteArray {
        var size = 4
        for (s in sides) size += 4 + s * s
        val buf = ByteArray(size)
        putLe(buf, 0, sides.size)
        var pos = 4
        for (s in sides) {
            putLe(buf, pos, s)
            pos += 4
            // deterministic pattern: dark iff (x + y) even
            for (i in 0 until s * s) buf[pos + i] = (i % 2).toByte()
            pos += s * s
        }
        return buf
    }

    private fun putLe(buf: ByteArray, off: Int, v: Int) {
        buf[off] = (v and 0xFF).toByte()
        buf[off + 1] = ((v ushr 8) and 0xFF).toByte()
        buf[off + 2] = ((v ushr 16) and 0xFF).toByte()
        buf[off + 3] = ((v ushr 24) and 0xFF).toByte()
    }

    @Test
    fun qrBatchRoundTripsModules() {
        val batch = parseQrBatch(packedBatch(21, 25))
        assertEquals(2, batch.tiles.size)
        val first = batch.tiles[0]
        assertEquals(21, first.side)
        // (0,0) → i=0 → dark=0 → light; (1,0) → i=1 → dark
        assertFalse(first.isDark(0, 0))
        assertTrue(first.isDark(1, 0))
        assertFalse(first.isDark(2, 0))
        val second = batch.tiles[1]
        assertEquals(25, second.side)
        assertTrue(second.isDark(1, 0))
        assertTrue(second.isDark(0, 1)) // i = 25 → odd → dark
    }

    @Test(expected = IllegalArgumentException::class)
    fun qrBatchRejectsTruncatedTile() {
        val buf = packedBatch(21)
        parseQrBatch(buf.copyOf(buf.size - 10))
    }

    @Test(expected = IllegalArgumentException::class)
    fun qrBatchRejectsTrailingBytes() {
        parseQrBatch(packedBatch(21) + byteArrayOf(1, 2, 3))
    }

    @Test(expected = IllegalArgumentException::class)
    fun qrBatchRejectsInvalidSide() {
        parseQrBatch(packedBatch(22)) // QR sides are 4k+1
    }

    @Test
    fun notStagedMarkerParses() {
        assertEquals(7, parseNotStagedIndex("AF2_CHUNK_NOT_STAGED:7"))
        assertNull(parseNotStagedIndex("AF2_CHUNK_NOT_STAGED:"))
        assertNull(parseNotStagedIndex("some other error"))
        assertNull(parseNotStagedIndex(null))
    }
}
