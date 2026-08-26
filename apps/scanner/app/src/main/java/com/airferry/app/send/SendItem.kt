package com.airferry.app.send

import android.net.Uri

/**
 * One item queued for sending. Two shapes only (no directory support on
 * Android v1): a SAF-picked file (`uri` set) or an in-memory UTF-8 text
 * message (`text` set). `displayName` becomes the wire path exactly as the
 * web sender names items (NFC-normalized, e.g. `文字消息.txt`).
 */
data class SendItem(
    /** 1 = file, 2 = utf8 text (KIND_FILE / KIND_UTF8_TEXT in core/af2). */
    val kind: Int,
    val displayName: String,
    val size: Long,
    val uri: Uri? = null,
    val text: ByteArray? = null
) {
    companion object {
        const val KIND_FILE = 1
        const val KIND_UTF8_TEXT = 2
        const val DEFAULT_TEXT_NAME = "文字消息.txt"
    }
}
