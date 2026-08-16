package com.airferry.app

import com.airferry.app.scan.ReceiverSessionManager
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Test
import java.io.File

/**
 * AF2 cross-platform golden-vector assertions (Kotlin / Android side).
 * Reads `core/testdata/af2/manifest.json` and verifies AF2 frame header parsing.
 */
class Af2GoldenVectorTest {

    private fun unhex(hex: String): ByteArray {
        val len = hex.length
        val out = ByteArray(len / 2)
        for (i in 0 until len step 2) {
            out[i / 2] = ((Character.digit(hex[i], 16) shl 4) + Character.digit(hex[i + 1], 16)).toByte()
        }
        return out
    }

    private fun loadManifest(): JSONObject {
        var dir: File? = File(System.getProperty("user.dir") ?: ".")
        while (dir != null) {
            val candidate = File(dir, "core/testdata/af2/manifest.json")
            if (candidate.isFile) {
                return JSONObject(candidate.readText())
            }
            dir = dir.parentFile
        }
        throw IllegalStateException("core/testdata/af2/manifest.json not found above working directory")
    }

    @Test
    fun af2GoldenVectors_verifyHeaders() {
        val manifest = loadManifest()
        val manager = ReceiverSessionManager()

        // 1. Verify ROOT frame header
        val rootFrameBytes = unhex(manifest.getString("root_frame_hex"))
        val rootHeader = manager.parseHeader(rootFrameBytes)
        assertNotNull(rootHeader)
        assertEquals(ReceiverSessionManager.MAGIC, rootHeader!!.magic)
        assertEquals(ReceiverSessionManager.PROTOCOL_VERSION, rootHeader.version)
        assertEquals(1, rootHeader.flags) // FrameTypeRoot = 1

        // 2. Verify OBJECT_META frame header
        val metaFrameBytes = unhex(manifest.getString("object_meta_frame_hex"))
        val metaHeader = manager.parseHeader(metaFrameBytes)
        assertNotNull(metaHeader)
        assertEquals(2, metaHeader!!.flags) // FrameTypeObjectMeta = 2

        // 3. Verify SYMBOL frame header
        val symbolFrameBytes = unhex(manifest.getString("symbol_frame_hex"))
        val symbolHeader = manager.parseHeader(symbolFrameBytes)
        assertNotNull(symbolHeader)
        assertEquals(3, symbolHeader!!.flags) // FrameTypeSymbol = 3
        assertEquals(1, symbolHeader.sbn)
        assertEquals(42, symbolHeader.esi)
    }
}
