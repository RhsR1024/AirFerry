package com.airferry.app.ui

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.camera2.CaptureRequest
import android.os.Bundle
import android.view.WindowManager
import android.widget.Toast
import android.util.Log
import android.util.Range
import android.util.Size
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.camera2.interop.Camera2Interop
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import com.airferry.app.nativelib.NativeBridge
import com.airferry.app.scan.ChunkSpillStore
import com.airferry.app.scan.QrDecodePool
import com.airferry.app.scan.QrStreamAnalyzer
import com.airferry.app.scan.ReceiverSessionManager
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

// Design tokens
private val BgDark = Color(0xFF0F172A)
private val CardBg = Color(0xCC1E293B)
private val Accent = Color(0xFF3B82F6)
private val TextPrimary = Color(0xFFF1F5F9)
private val TextSecondary = Color(0xFF94A3B8)
private val Success = Color(0xFF22C55E)

class ScanActivity : ComponentActivity() {

    private var session = ReceiverSessionManager()
    /**
     * On-disk staging for completed chunks (bounded-memory ledger): chunks are
     * spilled + evicted as they complete on the ingest thread; recovery slices
     * manifest entries straight from the file. Null until the first ChunkReady
     * (most sessions are small and never need it) — and on text transfers the
     * single chunk still goes through the same path, keeping one code shape.
     */
    private var chunkSpill: ChunkSpillStore? = null
    private val cameraExecutor = Executors.newSingleThreadExecutor()
    /** Dedicated single-thread executor for the post-recovery heavy work
     *  (JNI assemble, CRC, disk writes, bundle unpacking) so it never blocks
     *  the main thread. The work runs under the decode pool's ingest lock. */
    private val ioExecutor = Executors.newSingleThreadExecutor()
    private var cameraStarted = false
    private var previewView: PreviewView? = null

    /** Parallel QR decode pool (capture → queue → N workers → serialized ingest). */
    private var decodePool: QrDecodePool? = null

    /**
     * Sliding-window rate samples for the UI.
     * Prefer the last [RATE_WINDOW_MS] over whole-session averages so the user
     * sees near-instant throughput when the stream speeds up or stalls.
     *
     * Store symbol *counts* (not wire bytes) so a late-arriving real
     * [symbolSize] does not create a fake throughput spike from a discontinuous
     * byte counter.
     */
    private data class RateSample(val tMs: Long, val decoded: Long, val receivedSymbols: Long)
    private val rateSamples = ArrayDeque<RateSample>()
    private var decodePerSec = 0
    /** Recent wire throughput (bytes/s) over the sliding window. */
    private var recentWireBps = 0L

    // Reactive state observed by Compose
    private val uiState = mutableStateOf(UiState())

    data class UiState(
        val statusText: String = "正在初始化…",
        val progressPct: Int = 0,
        val receivedSymbols: Int = 0,
        val totalSymbols: Int = 0,
        val decodedBlocks: Int = 0,
        val totalBlocks: Int = 0,
        val lossPct: Int = 0,
        val framesSeen: Long = 0,
        val decodePerSec: Int = 0,
        val framesDropped: Long = 0,
        val fileName: String = "",
        val fileSize: Long = 0,
        /** Real compressed payload size (descriptor `compressed_size`); for
         *  segmented transfers this is the whole compressed-stream size. */
        val compressedSize: Long = 0,
        /** Zero-based current segment index when the transfer is segmented (0 otherwise). */
        val segmentIndex: Int = 0,
        /** Total segment count when the transfer is segmented (1 otherwise). */
        val segmentCount: Int = 1,
        val complete: Boolean = false,
        val jniReady: Boolean = false,
        /** Elapsed transfer time in ms (0 = not started yet). */
        val transferElapsedMs: Long = 0,
        /** RaptorQ symbol size in bytes (from the sender's config). */
        val symbolSize: Int = 0,
        /**
         * Recent wire throughput (bytes/s) over ~[RATE_WINDOW_MS], not the
         * whole-session average. 0 when the window is still empty.
         */
        val recentWireBps: Long = 0,
    )

    private val recoveryStage = mutableStateOf<String?>(null)
    /** Wall-clock ms when the transfer first started (totalSymbols > 0). */
    private var transferStartMs = 0L

    private val requestCameraPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                cameraStarted = false
                startCamera()
            } else {
                updateUi { it.copy(statusText = "需要相机权限") }
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Keep the screen on for the whole scan session. Transfers can run for
        // many minutes; without this the system timeout dims/locks the screen,
        // stops the camera preview, and aborts an in-progress receive.
        // FLAG_KEEP_SCREEN_ON only applies while this window is visible — no
        // WAKE_LOCK permission needed, and leaving the activity restores normal
        // timeout automatically.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        // JNI self-test + native ABI version handshake.
        // The version check must come first: a stale `.so` from an older APK
        // lacks the v5 segmented-receive symbol, so `nativeAbiVersion()` throws
        // `UnsatisfiedLinkError` while the (older) `receiverCreate` still works —
        // the old library would otherwise pass the create/destroy self-test and
        // then stall forever at "正在同步" on >32 MiB segmented transfers.
        var abiVersion = -1
        val abiOk = try {
            abiVersion = NativeBridge.nativeAbiVersion()
            abiVersion >= NativeBridge.NATIVE_ABI_VERSION
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "JNI ABI version symbol missing (stale native lib)", e)
            false
        } catch (e: Exception) {
            Log.e(TAG, "JNI ABI version query FAILED", e)
            false
        }

        val jniOk = if (!abiOk) {
            false
        } else {
            try {
                val h = NativeBridge.receiverCreate(0L, 1L)
                NativeBridge.receiverDestroy(h)
                true
            } catch (e: Exception) {
                Log.e(TAG, "JNI self-test FAILED", e); false
            }
        }
        updateUi {
            it.copy(
                jniReady = jniOk,
                statusText = if (!jniOk) "JNI 加载失败" else idleStatus(),
            )
        }
        if (!jniOk) {
            setContent {
                ErrorScreen(
                    if (abiOk) {
                        "原生库加载失败，请重新安装应用。"
                    } else {
                        "原生库版本过旧（ABI v$abiVersion，需要 v${NativeBridge.NATIVE_ABI_VERSION}），" +
                            "请卸载后重新安装最新版应用。"
                    }
                )
            }
            return
        }

        setContent { ScanScreen() }

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            startCamera()
        } else {
            requestCameraPermission.launch(Manifest.permission.CAMERA)
        }
    }

    @Composable
    private fun ScanScreen() {
        val state by uiState
        val recovery by recoveryStage

        BoxWithConstraints(modifier = Modifier.fillMaxSize().background(BgDark)) {

            // Camera preview (full screen) — CameraX PreviewView + ImageAnalysis.
            AndroidView(
                factory = { ctx ->
                    PreviewView(ctx).also { pv ->
                        pv.scaleType = PreviewView.ScaleType.FILL_CENTER
                    }
                },
                modifier = Modifier.fillMaxSize(),
                update = { pv -> bindCameraIfNeeded(pv) }
            )

            Column(
                modifier = Modifier.fillMaxSize().padding(16.dp),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Spacer(modifier = Modifier.height(40.dp))

                // Circular progress indicator
                CircularProgress(
                    progress = state.progressPct / 100f,
                    label = "${state.progressPct}%",
                    sublabel = if (state.fileName.isNotEmpty()) state.fileName else "等待扫描…"
                )

                Spacer(modifier = Modifier.weight(1f))

                // Bottom info card
                if (state.totalSymbols > 0) {
                    Card(
                        modifier = Modifier.fillMaxWidth(),
                        shape = RoundedCornerShape(16.dp),
                        colors = CardDefaults.cardColors(containerColor = CardBg)
                    ) {
                        Column(modifier = Modifier.padding(20.dp)) {
                            // 文件标题行（大号字体，仅文件名）
                            if (state.fileName.isNotEmpty()) {
                                Text(
                                    state.fileName,
                                    color = TextPrimary,
                                    fontSize = 17.sp,
                                    fontWeight = FontWeight.Bold,
                                    maxLines = 1,
                                    modifier = Modifier.padding(bottom = 4.dp)
                                )
                            }
                            // 大小行（原大小~压缩后大小）。压缩后大小来自描述符
                            // （分段时为整条压缩流大小），不是线上含冗余的符号字节。
                            val showOrig = state.fileSize > 0
                            val showCompressed = state.compressedSize > 0
                            if (showOrig || showCompressed) {
                                val sizeStr = buildString {
                                    if (showOrig) {
                                        append(formatSize(state.fileSize))
                                        if (showCompressed) append("~压缩后 ")
                                    }
                                    if (showCompressed) append(formatSize(state.compressedSize))
                                }
                                InfoRow("大小", sizeStr)
                            }
                            // 分段传输：明确当前收的是第几段。
                            if (state.segmentCount > 1) {
                                InfoRow("分段", "${state.segmentIndex + 1} / ${state.segmentCount}")
                            }
                            InfoRow("已识别符号", "${state.receivedSymbols} / ${state.totalSymbols}")
                            InfoRow("解码速率", "${state.decodePerSec} 符号/秒")
                            // 传输用时 + 近几秒滑动窗口速度（非全程平均）
                            if (state.transferElapsedMs > 0) {
                                val elapsedStr = formatDuration(state.transferElapsedMs)
                                // 线上吞吐 = 最近 RATE_WINDOW_MS 内 Δ(符号×symbolSize)/Δt
                                val speedStr = if (state.recentWireBps > 0)
                                    formatSize(state.recentWireBps) + "/s" else ""
                                InfoRow("用时", if (speedStr.isNotEmpty()) "$elapsedStr @ $speedStr" else elapsedStr)
                            }
                            LinearProgressIndicator(
                                progress = { state.progressPct / 100f },
                                modifier = Modifier.fillMaxWidth().padding(top = 12.dp),
                                color = Accent,
                                trackColor = Color(0xFF334155)
                            )
                        }
                    }
                    Spacer(modifier = Modifier.height(12.dp))
                }

                // Status text. A live recovery stage (assemble/CRC/save) takes
                // precedence over the "文件恢复完成" snapshot, so the user sees
                // the post-scan pipeline advancing instead of a frozen 100%.
                Text(
                    text = recovery ?: state.statusText,
                    color = if (recovery != null) Accent
                            else if (state.complete) Success else TextPrimary,
                    fontSize = 16.sp,
                    fontWeight = FontWeight.Bold,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.fillMaxWidth()
                )

                Spacer(modifier = Modifier.height(16.dp))

                // Action buttons row
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceEvenly
                ) {
                    ActionButton(Icons.Default.Folder, "文件") {
                        startActivity(Intent(this@ScanActivity, FileListActivity::class.java))
                    }
                    ActionButton(Icons.Default.Settings, "设置") {
                        startActivity(Intent(this@ScanActivity, SettingsActivity::class.java))
                    }
                    if (state.totalSymbols > 0 || state.complete) {
                        ActionButton(Icons.Default.Refresh, "重扫") { resetSession() }
                    }
                }
                Spacer(modifier = Modifier.height(24.dp))
            }
        }
    }

    @Composable
    private fun CircularProgress(progress: Float, label: String, sublabel: String) {
        Box(contentAlignment = Alignment.Center) {
            Surface(
                shape = CircleShape,
                color = CardBg,
                modifier = Modifier.size(160.dp)
            ) {}
            // Progress ring
            androidx.compose.foundation.Canvas(modifier = Modifier.size(160.dp)) {
                val stroke = 8.dp.toPx()
                val diameter = size.minDimension - stroke
                val topLeft = androidx.compose.ui.geometry.Offset(
                    (size.width - diameter) / 2f,
                    (size.height - diameter) / 2f
                )
                val arc = androidx.compose.ui.geometry.Size(diameter, diameter)
                drawArc(
                    color = Color(0xFF334155),
                    startAngle = -90f, sweepAngle = 360f, useCenter = false,
                    topLeft = topLeft, size = arc,
                    style = androidx.compose.ui.graphics.drawscope.Stroke(width = stroke)
                )
                drawArc(
                    color = Accent,
                    startAngle = -90f, sweepAngle = 360f * progress, useCenter = false,
                    topLeft = topLeft, size = arc,
                    style = androidx.compose.ui.graphics.drawscope.Stroke(width = stroke)
                )
            }
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Text(label, color = TextPrimary, fontSize = 32.sp, fontWeight = FontWeight.Bold)
                Text(
                    sublabel.take(20),
                    color = TextSecondary, fontSize = 12.sp,
                    maxLines = 1
                )
            }
        }
    }

    @Composable
    private fun InfoRow(label: String, value: String) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(vertical = 3.dp)
        ) {
            Text(label, color = TextSecondary, fontSize = 13.sp)
            Text(
                value, color = TextPrimary, fontSize = 13.sp,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                softWrap = false,
                textAlign = TextAlign.End,
                modifier = Modifier.weight(1f)
            )
        }
    }

    @Composable
    private fun ActionButton(icon: androidx.compose.ui.graphics.vector.ImageVector, label: String, onClick: () -> Unit) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            FilledTonalIconButton(onClick = onClick, modifier = Modifier.size(52.dp)) {
                Icon(icon, contentDescription = label, tint = Accent)
            }
            Text(label, color = TextSecondary, fontSize = 12.sp, modifier = Modifier.padding(top = 4.dp))
        }
    }

    @Composable
    private fun ErrorScreen(msg: String) {
        Box(
            modifier = Modifier.fillMaxSize().background(BgDark),
            contentAlignment = Alignment.Center
        ) {
            Text(msg, color = TextPrimary, textAlign = TextAlign.Center, modifier = Modifier.padding(32.dp))
        }
    }

    // ===== Camera + session logic =====

    private fun bindCameraIfNeeded(previewView: PreviewView) {
        this.previewView = previewView
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) !=
            PackageManager.PERMISSION_GRANTED
        ) return
        if (cameraStarted) return
        cameraStarted = true
        startCameraWithView(previewView)
    }

    private fun startCamera() {
        val view = previewView ?: return
        bindCameraIfNeeded(view)
    }

    /** Lazily create + start the shared parallel decode pool. */
    private fun ensurePool(): QrDecodePool {
        var p = decodePool
        if (p == null) {
            // Multi-QR mode is always on: the pool decodes every code on screen per
            // frame (not just the first), so a sender tiling N codes yields ~N×
            // throughput. Single-code senders decode just as well (the multi path
            // returns one result), so there's no need for a user-facing toggle — it
            // worked regardless of the switch position, and only added confusion.
            p = QrDecodePool(
                onDecoded = { payload, _ -> handleFrameAsync(payload) },
                multiMode = true,
            ).also { it.start() }
            decodePool = p
        }
        return p
    }

    @androidx.annotation.OptIn(androidx.camera.camera2.interop.ExperimentalCamera2Interop::class)
    private fun startCameraWithView(previewView: PreviewView) {
        val cameraProviderFuture = ProcessCameraProvider.getInstance(this)
        cameraProviderFuture.addListener({
            try {
                val cameraProvider = cameraProviderFuture.get()

                // Get/create the parallel decode pool. Each decoded payload is fed
                // to the native receiver via handleFrameAsync, serialized by the
                // pool's ingest lock so the non-thread-safe JNI handle is only ever
                // touched by one thread at a time.
                val pool = ensurePool()

                val preview = Preview.Builder().build().also {
                    it.setSurfaceProvider(previewView.surfaceProvider)
                }

                // Request a 1080p analysis stream so each QR module has more camera pixels,
                // improving ZXing decode reliability — especially important with the
                // reduced quiet zone on multi-QR. CameraX may pick the closest
                // supported size.
                val resolutionSelector = ResolutionSelector.Builder()
                    .setResolutionStrategy(
                        ResolutionStrategy(
                            Size(1920, 1080),
                            ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER
                        )
                    )
                    .build()

                fun buildAnalysis(fpsRange: Range<Int>): ImageAnalysis =
                    ImageAnalysis.Builder()
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .setResolutionSelector(resolutionSelector)
                        .also { builder ->
                            // Pin the sensor frame-rate via Camera2Interop (CameraX
                            // 1.3.x has no public ImageAnalysis#setTargetFrameRate).
                            Camera2Interop.Extender(builder).setCaptureRequestOption(
                                CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE,
                                fpsRange
                            )
                        }
                        .build()
                        .also { it.setAnalyzer(cameraExecutor, QrStreamAnalyzer(pool)) }

                // Pin a steady 60fps so low-light AE can't trade frame rate for a
                // longer exposure (the on-screen QR is a bright emissive source, so
                // a fixed 60 is usually fine and reduces motion / rolling-shutter
                // smear). Some devices reject a fixed [60,60]; fall back to [30,60].
                cameraProvider.unbindAll()
                try {
                    cameraProvider.bindToLifecycle(
                        this, CameraSelector.DEFAULT_BACK_CAMERA, preview, buildAnalysis(Range(60, 60))
                    )
                } catch (e: Exception) {
                    Log.w(TAG, "fixed 60fps bind failed; falling back to 30–60", e)
                    cameraProvider.unbindAll()
                    cameraProvider.bindToLifecycle(
                        this, CameraSelector.DEFAULT_BACK_CAMERA, preview, buildAnalysis(Range(30, 60))
                    )
                }
            } catch (e: Exception) {
                Log.e(TAG, "Camera bind failed", e)
                cameraStarted = false
                updateUi { it.copy(statusText = "相机启动失败") }
            }
        }, ContextCompat.getMainExecutor(this))
    }

    private var lastUiUpdate = 0L
    /** Written on the main thread AND from ioExecutor (recovery failure /
     *  re-archive paths), read on the main thread (onResume) → must be volatile. */
    @Volatile
    private var completedHandled = false
    /** Once recovery completes, stop feeding the native receiver so the
     *  main-thread assemble() (a `&` borrow) can't race a worker ingest (`&mut`). */
    private val ingestStopped = AtomicBoolean(false)

    /**
     * Pure-data snapshot produced on a decode-worker thread and handed to the
     * main thread. Keeping every JNI / JSON / RaptorQ step off the main thread is
     * what lets the receiver keep up with the camera — the heavy ingest chain
     * runs on the [QrDecodePool]'s serialized ingest path, and only the throttled
     * UI snapshot is posted to the main thread.
     */
    private data class FrameSnapshot(
        val progress: ReceiverSessionManager.Progress,
        val fileName: String,
        val fileSize: Long,
        /** Real compressed size (whole compressed stream for segmented). */
        val compressedSize: Long,
        /** Zero-based current segment index (0 when not segmented). */
        val segmentIndex: Int,
        /** Total segment count (1 when not segmented). */
        val segmentCount: Int
    )

    /** Ingest-thread entry (serialized by the pool): heavy work here, post a snapshot. */
    private fun handleFrameAsync(payload: ByteArray) {
        // After completion, drop further frames: the main thread is (or will be)
        // calling assemble() on the receiver, which must not run concurrently
        // with another ingest. This runs under the pool's ingest lock, so the
        // check+ingest+stop sequence is atomic w.r.t. other workers.
        if (ingestStopped.get()) return
        // ingest() returns a lightweight status (no JSON) so the per-frame path
        // stays cheap; the full progress is fetched only on the throttled UI tick.
        val status = session.ingest(payload) ?: return

        // Bounded-memory ledger: spill + evict the chunk this frame completed so
        // native memory stays O(chunk) instead of O(whole object). The same
        // serialized ingest thread drains it, so no extra synchronization.
        if (status.accepted && status.receivedSymbols == 0) {
            // Relock (or first lock): a foreign Transfer now owns the session —
            // the old spill's bytes belong to nobody. Discard before any drain.
            chunkSpill?.discard()
            chunkSpill = null
        }
        if (status.chunkReady) {
            val spill = chunkSpill ?: ChunkSpillStore(
                cacheDir, session.snapshot().transferIdHex
            ).also { chunkSpill = it }
            session.drainLastChunk { index, chunkRawSize, bytes ->
                spill.write(index, chunkRawSize, bytes)
            }
        }

        // UI refresh throttle: ~7 Hz is plenty for a progress bar, and keeps the
        // main thread free. Always let the final "complete" frame through.
        val now = System.currentTimeMillis()
        if (now - lastUiUpdate < 150 && !status.complete) return
        lastUiUpdate = now

        // On the UI tick (or completion), pull the full progress snapshot. This
        // is the only place the JSON is parsed — not every frame.
        val progress = session.progress() ?: return

        // Read file metadata from session (JNI) — keep on this background thread.
        val fn = if (session.isInitialized) session.fileName() else ""
        val fs = if (session.isInitialized) session.fileSize() else 0L
        // Size display: in AF2 the snapshot reports ONE total raw size for the
        // whole content regardless of chunk count; the segment* shims below map
        // to chunk count / total raw size (no per-segment child sessions).
        val segmented = session.isInitialized && session.isSegmented()
        val cs = if (session.isInitialized) {
            if (segmented) session.rootOriginalSize() else session.compressedSize()
        } else {
            0L
        }
        val segIdx = if (segmented) session.segmentIndex() else 0
        val segCount = if (segmented) session.segmentCount() else 1

        val snapshot = FrameSnapshot(progress, fn, fs, cs, segIdx, segCount)
        if (status.complete) {
            // Block any further ingest before the completion path (assemble +
            // file I/O + Activity start) runs on the main thread.
            ingestStopped.set(true)
            runOnUiThread { applySnapshot(snapshot, handleCompletion = true) }
        } else {
            runOnUiThread { applySnapshot(snapshot, handleCompletion = false) }
        }
    }

    /** Main-thread only: apply the precomputed snapshot to Compose state. */
    private fun applySnapshot(s: FrameSnapshot, handleCompletion: Boolean) {
        val progress = s.progress
        // Progress bar tracks *received (de-duplicated) symbols*, not decoded
        // symbols. RaptorQ decodes a whole source block at once when it has
        // collected enough independent symbols, so a "decoded fraction" bar sits
        // flat near 0% for a long time and then jumps in steps — it reads as
        // "stuck". The received-symbol count, by contrast, increments by one
        // for every new symbol the receiver accepts, so the bar climbs ~linearly
        // and matches what the user sees on screen. Fountain repair symbols can
        // push receivedSymbols above totalSymbols K, so clamp to 100.
        val pct = when {
            progress.complete -> 100
            progress.metaConfirmed || progress.totalSymbols > 0 -> {
                if (progress.totalSymbols > 0) {
                    (progress.receivedSymbols * 100 / progress.totalSymbols).coerceIn(0, 100)
                } else {
                    0
                }
            }
            // Cache mode: no confirmed total yet. Estimate from the first frame's
            // total_symbols (advisory only) and cap at 15% — the descriptor may
            // later reveal a larger total, so don't over-promise early.
            progress.receivedSymbols > 0 -> {
                val estimated = session.getEstimatedTotalSymbols()
                if (estimated > 0) {
                    (progress.receivedSymbols * 100 / estimated).coerceIn(0, 15)
                } else {
                    0
                }
            }
            else -> 0
        }
        val statusMsg = when {
            progress.complete -> "文件恢复完成"
            !progress.metaConfirmed && progress.receivedSymbols > 0 ->
                "正在同步… 已缓存 ${progress.receivedSymbols} 符号 (~$pct%)"
            progress.totalSymbols == 0 -> "等待二维码…"
            progress.receivedSymbols > 0 && progress.decodedBlocks == 0 ->
                "接收中… ${progress.receivedSymbols}/${progress.totalSymbols} 符号 (等待解码)"
            else -> "恢复中… $pct%"
        }
        // Sliding-window rates: decode symbols/s + wire bytes/s over RATE_WINDOW_MS.
        // UI ticks are already throttled (~7 Hz), so each sample is a fresh point;
        // prune anything older than the window and derive Δcount/Δt.
        val pool = decodePool
        val nowMs = System.currentTimeMillis()
        // Rate math uses ≥1 so early pre-descriptor ticks don't div0; samples
        // store symbol counts so a late real symbolSize never rewrites history.
        val symbolSize = session.symbolSizeBytes().coerceAtLeast(1)
        val receivedNow = progress.receivedSymbols.toLong().coerceAtLeast(0)
        val decodedNow = pool?.decodedCount() ?: 0L
        if (progress.complete) {
            // Freeze at 0 once done — final tick would otherwise show the last
            // non-zero window forever on the completed card.
            decodePerSec = 0
            recentWireBps = 0L
            rateSamples.clear()
        } else if (receivedNow > 0L || decodedNow > 0L) {
            rateSamples.addLast(RateSample(nowMs, decodedNow, receivedNow))
            while (rateSamples.size > 1 && nowMs - rateSamples.first().tMs > RATE_WINDOW_MS) {
                rateSamples.removeFirst()
            }
            if (rateSamples.size >= 2) {
                val oldest = rateSamples.first()
                val newest = rateSamples.last()
                val dt = newest.tMs - oldest.tMs
                // Need a short baseline so a single tick doesn't explode the rate.
                if (dt >= RATE_MIN_DT_MS) {
                    decodePerSec = (((newest.decoded - oldest.decoded) * 1000L) / dt)
                        .toInt().coerceAtLeast(0)
                    val dSym = (newest.receivedSymbols - oldest.receivedSymbols).coerceAtLeast(0L)
                    recentWireBps = ((dSym * symbolSize * 1000L) / dt).coerceAtLeast(0L)
                }
            } else {
                // Window collapsed (e.g. long stall then one fresh tick) — don't
                // keep showing a stale non-zero rate from before the gap.
                decodePerSec = 0
                recentWireBps = 0L
            }
        }
        val droppedTotal = pool?.droppedCount() ?: 0L

        // Start the transfer timer on first symbol receipt.
        if (progress.totalSymbols > 0 && transferStartMs == 0L) {
            transferStartMs = nowMs
        }
        val elapsedMs = if (transferStartMs > 0) nowMs - transferStartMs else 0L

        updateUi {
            it.copy(
                progressPct = pct,
                receivedSymbols = progress.receivedSymbols,
                totalSymbols = progress.totalSymbols,
                decodedBlocks = progress.decodedBlocks,
                totalBlocks = progress.totalBlocks,
                lossPct = (progress.lossRatio * 100).toInt(),
                framesSeen = progress.framesSeen,
                decodePerSec = decodePerSec,
                framesDropped = droppedTotal,
                fileName = s.fileName,
                fileSize = s.fileSize,
                compressedSize = s.compressedSize,
                segmentIndex = s.segmentIndex,
                segmentCount = s.segmentCount,
                statusText = statusMsg,
                complete = progress.complete,
                transferElapsedMs = elapsedMs,
                symbolSize = symbolSize,
                recentWireBps = recentWireBps,
            )
        }

        if (handleCompletion && progress.complete && !completedHandled) {
            completedHandled = true
            // Move the heavy recovery work (JNI assemble, CRC over the full
            // payload, disk writes, bundle unpacking) off the main thread — it
            // previously ran here synchronously and ANR'd on multi-MB transfers.
            // ingestStopped (set on the completing worker) already guarantees no
            // further ingest touches the native session, and we wrap the JNI
            // access in runExclusive so it cannot race a straggler or destroy().
            val snapshotFileName = s.fileName
            // Capture the pool at enqueue time and use THIS captured ref inside
            // the task — never re-read the `decodePool` field. onDestroy (main
            // thread) nulls `decodePool` and captures `session` to a local for its
            // own background destroy via the SAME pool instance. If this task
            // re-read the field it could (a) take the lock-less `?: work()`
            // branch once onDestroy has nulled the field, and (b) race destroy()
            // on the native handle (isInitialized is only a TOCTOU hint, not a
            // real guard). By pinning the pool here, the recovery and onDestroy's
            // destroy are GUARANTEED to serialize on the same pool.runExclusive
            // (ingestLock) — recovery holds the lock while assemble() runs,
            // destroy blocks on the lock until recovery returns, no
            // use-after-free, no TOCTOU. recoverAndStage reads the `session`
            // field, but onDestroy never reassigns it (only destroys in place),
            // so after destroy the field's isInitialized==false and the guarded
            // getters no-op → recoverAndStage returns null harmlessly.
            val poolAtEnqueue = decodePool
            ioExecutor.execute {
                try {
                    var intent: Intent? = null
                    val work = fun() {
                        intent = recoverAndStage(snapshotFileName)
                    }
                    // Always serialize via the captured pool. If the pool was
                    // already null at enqueue (shouldn't happen mid-scan, but be
                    // defensive), skip recovery entirely — calling recoverAndStage
                    // without the lock would race destroy() on the native handle.
                    poolAtEnqueue?.runExclusive(work)
                    intent?.let { runOnUiThread { startActivity(it) } }
                } catch (e: Exception) {
                    clearRecoveryStage()
                    resetReceiverAfterRecoveryFailure()
                    runOnUiThread {
                        Toast.makeText(
                            this,
                            e.message ?: "保存接收内容失败",
                            Toast.LENGTH_LONG,
                        ).show()
                    }
                } catch (e: OutOfMemoryError) {
                    // A large recovered payload (e.g. multi-MB text decoded to a
                    // ~2x String) can transiently exceed the default heap. Do not
                    // crash the whole scanner — drop to a graceful message. The
                    // bytes are typically already persisted by this point, so the
                    // user can reopen the file from the list.
                    android.util.Log.e("ScanActivity", "recoverAndStage OOM", e)
                    clearRecoveryStage()
                    resetReceiverAfterRecoveryFailure()
                    runOnUiThread {
                        Toast.makeText(this, "文件过大，接收内存不足", Toast.LENGTH_LONG).show()
                    }
                }
            }
        }
    }

    /**
     * Assemble the recovered bytes, verify CRC, and stage the file(s) to disk.
     * Returns the [Intent] to launch the detail/bundle screen, or null if there
     * was nothing to recover. Runs on a background thread under the decode pool's
     * ingest lock (so it can't race an in-flight ingest or a destroy()).
     */    /**
     * Assemble the recovered AF2 Canonical Content Stream and stage entries to
     * disk based on the Manifest entry table (kind = text / file / bundle).
     * Runs on a background thread under the decode pool's ingest lock.
     */
    private fun recoverAndStage(displayName: String): Intent? {
        val intent = stageFromLedger(displayName)
        if (intent != null) {
            // Entries are in ContentStore now — the spill has been consumed.
            chunkSpill?.discard()
            chunkSpill = null
        }
        return intent
    }

    private fun stageFromLedger(displayName: String): Intent? {
        updateRecoveryStage("正在组装数据…")
        val snapshot = session.snapshot()

        // Prefer the on-disk chunk spill: every completed chunk was pwrite'd
        // there (and evicted from native memory) as it arrived, so slicing
        // entries from the file keeps peak memory at one entry instead of the
        // whole canonical stream. Fall back to the in-memory assemble for
        // sessions that completed without a spill (defensive).
        val spill = chunkSpill
        val spillUsable =
            spill != null && snapshot.totalRawSize > 0 &&
                spill.length() >= snapshot.totalRawSize
        val stream: ByteArray? = if (spillUsable) null else session.assemble()
        if (stream == null && !spillUsable) {
            clearRecoveryStage()
            if (session.isComplete()) {
                runOnUiThread {
                    Toast.makeText(this, "恢复失败: 数据组装失败", Toast.LENGTH_LONG).show()
                }
            }
            return null
        }

        val nonDirEntries = snapshot.entries.filter { it.kind != 3 } // 3 = DIRECTORY
        val store = com.airferry.app.scan.ContentStore

        // Manifest offsets/sizes are u64 and cannot be trusted: bounds-check in
        // the Long domain before narrowing to Int, so a bogus entry degrades to
        // empty bytes instead of wrapping into a wrong slice. Reads come from
        // the spill file when usable, else from the in-memory stream.
        fun sliceAt(off: Long, sz: Long): ByteArray {
            if (spillUsable) {
                return spill!!.readRange(off, sz) ?: ByteArray(0)
            }
            val st = stream!!
            return if (off >= 0 && sz >= 0 && off + sz <= st.size.toLong() &&
                off <= Int.MAX_VALUE && sz <= Int.MAX_VALUE
            ) st.copyOfRange(off.toInt(), (off + sz).toInt()) else ByteArray(0)
        }

        // ── Single UTF8_TEXT entry → text view (AF2 kind, no magic sniffing) ──
        if (nonDirEntries.size == 1 && nonDirEntries[0].kind == 2) {
            val e0 = nonDirEntries[0]
            val textBytes = sliceAt(e0.offset, e0.size)
            val textName = e0.path.ifEmpty { TEXT_RECEIVED_NAME }
            val text =
                if (com.airferry.app.scan.TextLike.fitsTextUi(textBytes.size))
                    com.airferry.app.scan.TextLike.decodeUtf8Strict(textBytes)
                else null

            if (text != null) {
                updateRecoveryStage("正在保存文字…")
                val put = store.putBytes(
                    this, textName, textBytes,
                    crcUnknown = true, kind = "text",
                )
                clearRecoveryStage()
                return Intent(this, ReceiveTextActivity::class.java).apply {
                    putExtra("FILE_PATH", put.path.absolutePath)
                    putExtra("FILE_NAME", textName)
                    putExtra("ENTRY_ID", put.entry.id)
                    putExtra("CRC32_UNKNOWN", true)
                }
            }
            // Oversized or invalid UTF-8 → ordinary .txt file.
            updateRecoveryStage("正在保存文件…")
            val put = store.putBytes(
                this, textName, textBytes,
                crcUnknown = true, kind = "file",
            )
            clearRecoveryStage()
            return Intent(this, ReceiveDetailActivity::class.java).apply {
                putExtra("FILE_PATH", put.path.absolutePath)
                putExtra("FILE_SIZE", textBytes.size.toLong())
                putExtra("FILE_NAME", textName)
                putExtra("ENTRY_ID", put.entry.id)
                putExtra("CRC32_UNKNOWN", true)
                putExtra("RESAVE", true)
            }
        }

        // ── Multiple entries → bundle, one ContentStore entry per member ──
        if (nonDirEntries.size > 1) {
            val totalFiles = nonDirEntries.size
            val ts = java.text.SimpleDateFormat("MMdd_HHmmss", java.util.Locale.getDefault())
                .format(java.util.Date())
            val bundleId = java.util.UUID.randomUUID().toString()
            val bundleTitle = "发送_$ts"
            updateRecoveryStage("正在保存 $totalFiles 个文件…")
            val requests = nonDirEntries.map { e ->
                val bytes = sliceAt(e.offset, e.size)
                com.airferry.app.scan.ContentStore.PutBytesRequest(
                    e.path, bytes,
                    crcUnknown = true, kind = "file",
                    bundleId = bundleId, bundleTitle = bundleTitle,
                )
            }
            val puts = store.putBytesBatch(this, requests)

            val paths = ArrayList<String>()
            val names = ArrayList<String>()
            val sizes = ArrayList<String>()
            val entryIds = ArrayList<String>()
            for (p in puts) {
                paths.add(p.path.absolutePath)
                names.add(p.entry.name)
                sizes.add(p.entry.size.toString())
                entryIds.add(p.entry.id)
            }
            clearRecoveryStage()
            return Intent(this, ReceiveBundleActivity::class.java).apply {
                putStringArrayListExtra("FILE_PATHS", paths)
                putStringArrayListExtra("FILE_NAMES", names)
                putStringArrayListExtra("FILE_SIZES", sizes)
                putStringArrayListExtra("ENTRY_IDS", entryIds)
                putExtra("BUNDLE_ID", bundleId)
                putExtra("BUNDLE_TITLE", bundleTitle)
                putExtra("TOTAL_FILES", totalFiles)
                putExtra("RESAVE", true)
            }
        }

        // ── Single file entry (or empty-entry defensive fallback) ──
        val entry = nonDirEntries.firstOrNull()
        val fileName = entry?.path?.takeIf { it.isNotEmpty() }
            ?: displayName.ifEmpty { "received_file" }
        val fileBytes = when {
            entry != null && entry.offset >= 0 && entry.size >= 0 &&
                entry.offset + entry.size <= snapshot.totalRawSize &&
                entry.offset <= Int.MAX_VALUE && entry.size <= Int.MAX_VALUE ->
                sliceAt(entry.offset, entry.size)
            else -> sliceAt(0L, snapshot.totalRawSize)
        }

        updateRecoveryStage("正在保存文件…")
        val put = store.putBytes(
            this, fileName, fileBytes,
            crcUnknown = true, kind = "file",
        )
        clearRecoveryStage()
        return Intent(this, ReceiveDetailActivity::class.java).apply {
            putExtra("FILE_PATH", put.path.absolutePath)
            putExtra("FILE_SIZE", fileBytes.size.toLong())
            putExtra("FILE_NAME", fileName)
            putExtra("ENTRY_ID", put.entry.id)
            putExtra("CRC32_UNKNOWN", true)
            putExtra("RESAVE", true)
        }
    }

    /** Recover from any post-decode failure without stranding the scanner in
     * `completedHandled=true` / `ingestStopped=true`. */
    private fun resetReceiverAfterRecoveryFailure() {
        val swap = {
            session.destroy()
            session = ReceiverSessionManager()
            chunkSpill?.discard()
            chunkSpill = null
            ingestStopped.set(false)
            completedHandled = false
            lastUiUpdate = 0
            rateSamples.clear()
        }
        try {
            decodePool?.runExclusive(swap) ?: swap()
        } catch (resetError: Exception) {
            Log.e(TAG, "failed to reset receiver after recovery error", resetError)
        }
    }

    private fun idleStatus(): String = "就绪 — 对准二维码…"

    /**
     * Reset the native receiver on a background thread, under the pool's ingest
     * lock. Main-thread callers (重扫 button, onResume) must NEVER block on that
     * lock directly: an in-flight archive (recoverAndStage → asm.finish() 解压 +
     * CRC + SHA → putFile, executed on ioExecutor via runExclusive) can hold it
     * for tens of seconds on large transfers — a lock acquire without timeout
     * on the main thread is a guaranteed ANR (H3). The swap is posted to the
     * single-threaded [ioExecutor] so it also stays ordered behind any queued
     * archive work.
     */
    private fun resetReceiverAsync() {
        val poolAtEnqueue = decodePool
        ioExecutor.execute {
            val swap = {
                session.destroy()
                session = ReceiverSessionManager()
                chunkSpill?.discard()
                chunkSpill = null
                ingestStopped.set(false)
            }
            try {
                poolAtEnqueue?.runExclusive(swap) ?: swap()
            } catch (resetError: Exception) {
                Log.e(TAG, "failed to reset receiver", resetError)
            }
        }
    }

    private fun resetSession() {
        // Swap the receiver under the pool's ingest lock so no worker is mid-ingest
        // while we destroy the old native handle — asynchronously (see
        // [resetReceiverAsync]); the UI-visible counters below reset immediately.
        resetReceiverAsync()
        completedHandled = false
        lastUiUpdate = 0
        rateSamples.clear()
        decodePerSec = 0
        recentWireBps = 0L
        transferStartMs = 0L
        recoveryStage.value = null
        updateUi {
            UiState(jniReady = true, statusText = idleStatus())
        }
    }

    private fun updateUi(block: (UiState) -> UiState) {
        uiState.value = block(uiState.value)
    }

    /** Set the live recovery-stage status text (posted to the main thread).
     *  Called from [ioExecutor] during [recoverAndStage] so the user sees the
     *  post-scan pipeline advancing instead of a frozen "完成". */
    private fun updateRecoveryStage(text: String) {
        runOnUiThread { recoveryStage.value = text }
    }

    /** Clear the recovery-stage status (e.g. right before launching the result
     *  Activity, or on error / reset). */
    private fun clearRecoveryStage() {
        runOnUiThread { recoveryStage.value = null }
    }

    // slotScreenPos 已移除（火花动画已删除）。

    // refreshOverlay / dedupeSparksBySlot 已移除（火花动画已删除）。
    override fun onResume() {
        super.onResume()
        // If returning from ReceiveDetailActivity after completion, reset for next scan.
        if (completedHandled) {
            // Never wait on the ingest lock on the main thread — an in-flight
            // archive may hold it for tens of seconds (H3). Post the swap to
            // ioExecutor; reset the UI-visible counters immediately.
            resetReceiverAsync()
            completedHandled = false
            lastUiUpdate = 0
            rateSamples.clear()
            decodePerSec = 0
            recentWireBps = 0L
            transferStartMs = 0L
            recoveryStage.value = null
            updateUi { UiState(jniReady = true, statusText = idleStatus()) }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        cameraExecutor.shutdown()
        // Snapshot the native-session owner and decode pool to locals, then drop
        // the Activity fields so any post-destroy callback / Activity-recreation
        // cannot reach them. The actual drain + destroy happens on a detached
        // daemon thread (below) — NEVER on the main thread. The previous code
        // called ioExecutor.awaitTermination(30s) here, which for a large
        // segmented transfer (stream-decompress + SHA over hundreds of MiB on
        // ioExecutor) blocked the main thread for up to 30 s → guaranteed ANR on
        // rotation / recents. Mirrors the Windows ScanViewModel 2 s quarantine.
        val pool = decodePool
        decodePool = null
        val sessionRef = session
        val spillRef = chunkSpill
        chunkSpill = null
        // Drain the IO executor BEFORE tearing down the decode pool: the pending
        // recovery task holds the pool's ingest lock and touches the native
        // session, so freeing the handle first would race it. Shutdown lets an
        // in-flight stage finish (bounded; assemble is the slow part and already
        // running under ingestStopped, which halted further ingest). The drain
        // + destroy run on a daemon thread so the main thread is never blocked.
        ioExecutor.shutdown()
        // The correctness anchor: the in-flight recovery job and destroy() both
        // go through pool.runExclusive (ingestLock), so they are mutually
        // exclusive regardless of timing — no use-after-free even if the await
        // below is skipped.
        Thread {
            try {
                ioExecutor.awaitTermination(30, java.util.concurrent.TimeUnit.SECONDS)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
            }
            // Stop workers, then destroy the (captured) native receiver UNDER
            // the ingest lock so a straggler that outran shutdown()'s join
            // timeout can't still be mid-ingest (&mut) when destroy() frees the
            // handle (use-after-free). destroy() is idempotent.
            if (pool != null) {
                pool.shutdown()
                pool.runExclusive {
                    sessionRef.destroy()
                    spillRef?.discard()
                }
            } else {
                sessionRef.destroy()
                spillRef?.discard()
            }
        }.apply { isDaemon = true; name = "airferry-destroy" }.start()
    }

    companion object {
        private const val TAG = "ScanActivity"
        /**
         * Sliding window for decode rate + wire throughput shown in the info card.
         * ~3s is responsive enough to feel "live" without jittering every tick.
         */
        private const val RATE_WINDOW_MS = 3_000L
        /** Minimum Δt before publishing a rate (avoids 1-tick spikes). */
        private const val RATE_MIN_DT_MS = 300L
        /** Default display/store name when an older TEXT descriptor has no filename. */
        private const val TEXT_RECEIVED_NAME = "文字消息.txt"

        fun formatSize(bytes: Long): String {
            if (bytes < 1024) return "$bytes B"
            if (bytes < 1024 * 1024) return "%.1f KB".format(bytes / 1024.0)
            return "%.1f MB".format(bytes / 1024.0 / 1024.0)
        }

        /** Format milliseconds as a human-readable duration (e.g. "23 秒", "1 分 05 秒"). */
        fun formatDuration(ms: Long): String {
            val totalSec = ms / 1000
            if (totalSec < 60) return "${totalSec} 秒"
            val m = totalSec / 60
            val s = totalSec % 60
            return "${m} 分 ${s.toString().padStart(2, '0')} 秒"
        }

        fun crc32OfBytes(data: ByteArray): Long {
            // Compute CRC32 and return as an unsigned 32-bit value in a Long
            // (0..=0xFFFFFFFF) so it compares correctly with the JNI-supplied
            // expected CRC (also a Long). Using Int would sign-flip high-bit
            // values and break equality.
            // java.util.zip.CRC32 is the table-driven JVM implementation — the
            // previous bit-by-bit software loop was ~50× slower and sat on the
            // recovery hot path (multi-MB payloads) and the file-list open path.
            val crc = java.util.zip.CRC32()
            crc.update(data)
            return crc.value
        }

        /**
         * Streaming CRC32 over a file (64 KiB buffer) — O(1) memory regardless
         * of file size. Replaces the old `crc32OfBytes(file.readBytes())`
         * pattern in the file list, which whole-loaded blobs of hundreds of MiB
         * and OOM-crashed (an Error the surrounding `catch (Exception)` could
         * not intercept). Returns the same unsigned 32-bit Long as
         * [crc32OfBytes]; throws on I/O failure (caller decides the fallback).
         */
        fun crc32OfFile(file: java.io.File): Long {
            val crc = java.util.zip.CRC32()
            java.io.FileInputStream(file).use { ins ->
                val buf = ByteArray(64 * 1024)
                while (true) {
                    val n = ins.read(buf)
                    if (n <= 0) break
                    crc.update(buf, 0, n)
                }
            }
            return crc.value
        }
    }
}
