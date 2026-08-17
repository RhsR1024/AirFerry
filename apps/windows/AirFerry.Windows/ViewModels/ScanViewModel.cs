using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Text;
using AirFerry.Windows.Bundle;
using AirFerry.Windows.Models;
using AirFerry.Windows.Scan;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using OpenCvSharp;

namespace AirFerry.Windows.ViewModels;

/// <summary>
/// The scan-page state machine — the Windows counterpart of Android's
/// <c>ScanActivity</c>. Owns the <see cref="IFrameSource"/> (producer — a
/// DirectShow device, a screen rectangle or a window), <see cref="QrDecodePool"/>
/// (N parallel decoders + serialized ingest), and a single
/// <see cref="ReceiverSession"/> (the Rust RaptorQ engine). On completion
/// it assembles the bytes, trims RaptorQ zero-padding, verifies CRC, unpacks a
/// bundle if present, and stages the result for the detail/bundle views.
/// </summary>
/// <remarks>
/// <para>
/// <b>Threading model</b>: a dedicated producer thread pulls frames from the
/// camera and feeds the pool. The pool's workers do the ZXing decode in
/// parallel; ingest (the <see cref="ReceiverSession.Ingest"/> call) is
/// serialized inside the pool under <see cref="QrDecodePool.IngestLock"/>. The
/// final assemble also runs under that lock (via <see cref="QrDecodePool.RunExclusive{T}"/>)
/// so no straggler ingest can race the borrow. The recovery task remains part
/// of the session lifetime: teardown waits for it and all workers before
/// destroying the native receiver.
/// </para>
/// <para>
/// <b>Files land in</b> the content-addressed <see cref="ContentStore"/> under
/// <c>%USERPROFILE%\Documents\AirFerry\store\</c>.
/// </para>
/// </remarks>
public partial class ScanViewModel : ObservableObject, IDisposable
{
    private IFrameSource? _capture;
    private QrDecodePool? _pool;
    private ReceiverSession? _session;
    /// <summary>
    /// On-disk staging for completed chunks (bounded-memory ledger): chunks are
    /// spilled + evicted on the ingest thread as they complete; recovery reads
    /// the canonical stream straight from the file. Null until the first
    /// ChunkReady. Touched on the pool's serialized ingest path and the
    /// lifecycle swap paths that run under the same ingest lock.
    /// </summary>
    private ChunkSpillStore? _chunkSpill;
    /// <summary>§12 resume ledger journal bound to the current transfer.</summary>
    private Af2LedgerStore? _af2Ledger;
    /// <summary>Resumed chunk indices awaiting post-manifest re-verification (§12).</summary>
    private SortedSet<int>? _pendingReverify;
    private Thread? _producerThread;
    private volatile bool _producerRunning;
    private bool _disposed;
    private int _recoveryStarted;
    private int _sessionEpoch;
    private readonly object _lifecycleGate = new();
    private Task<RecoveryOutcome>? _recoveryCoreTask;
    private Task _deferredCleanupTask = Task.CompletedTask;
    private readonly Queue<RateSample> _rateSamples = new();
    private long _transferStartTimestamp;
    private long _decodePerSecond;
    private long _recentWireBytesPerSecond;
    private const int PreviewFps = 15;
    private const int RateWindowSeconds = 3;
    private const int RateMinMilliseconds = 500;
    /// <summary>Continuous-receive folder sink (null = single-receive mode).</summary>
    private AirFerry.Windows.Bundle.ContinuousSaver? _continuousSaver;
    /// <summary>Session whose pre-scan duplicate checks already ran (once per receiver).</summary>
    private ReceiverSession? _preScanCheckedSession;

    private sealed record AssembledPayload(
        byte[] Bytes,
        ulong ExpectedCrc,
        bool CrcKnown,
        string DisplayName,
        ulong OriginalSize);

    private readonly record struct RateSample(
        long Timestamp, long DecodedSymbols, long ReceivedSymbols);

    private readonly record struct LiveSnapshot(
        ProgressSnapshot? Progress,
        string FileName,
        ulong FileSize,
        uint SymbolSize,
        int EstimatedTotalSymbols);

    /// <summary>The frame source chosen in the device-select page.</summary>
    [ObservableProperty]
    private ScanSource? _selectedSource;

    [ObservableProperty]
    private string _statusText = "等待扫码…";

    [ObservableProperty]
    private double _progress;

    [ObservableProperty]
    private string _receivedSymbolsText = "0";

    [ObservableProperty]
    private string _totalSymbolsText = "0";

    [ObservableProperty]
    private string _lossRatioText = "0.0%";

    [ObservableProperty]
    private string _recoveryStageText = string.Empty;

    [ObservableProperty]
    private bool _isComplete;

    [ObservableProperty]
    private bool _isRecovering;

    [ObservableProperty]
    private bool _isScanning;

    [ObservableProperty]
    private string _scanMetricsText = "采集 0 帧 · 丢弃 0 帧 · 解码 0 码";

    [ObservableProperty]
    private string _fileSummaryText = "等待描述符…";

    [ObservableProperty]
    private string _transferMetricsText = "解码 0 符号/秒 · 有效 0 B/s · 用时 00:00";

    /// <summary>Raised when a transfer finishes recovering — carries the result.</summary>
    public event Action<RecoveryResult>? TransferCompleted;

    /// <summary>
    /// Raised by the producer thread at most <see cref="PreviewFps"/> times per
    /// second. Subscribers must marshal rendering to their UI dispatcher.
    /// </summary>
    public event Action<PreviewFrame>? PreviewFrameReady;

    /// <summary>One entry in the continuous-receive feed (newest first).</summary>
    public enum ContinuousItemStatus { Saved, Skipped, Failed }

    public sealed record ContinuousReceivedItem(
        string TimeText,
        string Name,
        string SizeText,
        ContinuousItemStatus Status,
        string? Error = null)
    {
        /// <summary>Preformatted feed line for the ItemsControl template.</summary>
        public string Line => Status switch
        {
            ContinuousItemStatus.Skipped => $"{TimeText}  {Name} · 重复，已跳过",
            ContinuousItemStatus.Failed => $"{TimeText}  {Name} · 保存失败: {Error}",
            _ => $"{TimeText}  {Name} · {SizeText}",
        };
    }

    /// <summary>
    /// Continuous mode: on completion do not navigate — save into the chosen
    /// folder, re-arm a fresh receiver and keep scanning for the next file.
    /// </summary>
    public bool ContinuousMode { get; private set; }

    public string ContinuousSaveDir => _continuousSaver?.TargetDir ?? string.Empty;

    public int ContinuousSavedCount { get; private set; }

    public int ContinuousSkippedCount { get; private set; }

    /// <summary>Most-recent-first feed of continuous saves (dispatcher thread only).</summary>
    public ObservableCollection<ContinuousReceivedItem> ContinuousItems { get; } = [];

    public string ContinuousSummaryText =>
        ContinuousMode || ContinuousSavedCount > 0 || ContinuousSkippedCount > 0
            ? $"已保存 {ContinuousSavedCount} 份 · 跳过 {ContinuousSkippedCount} 份重复"
            : string.Empty;

    /// <summary>
    /// Point continuous mode at a folder and enable it. Re-enabling with the
    /// same folder keeps the dedup set (a toggled-off period does not forget
    /// what was already saved); a different folder starts fresh.
    /// </summary>
    public void SetContinuousDir(string dir)
    {
        bool sameDir = _continuousSaver is not null && string.Equals(
            _continuousSaver.TargetDir, dir, StringComparison.OrdinalIgnoreCase);
        if (!sameDir)
        {
            _continuousSaver = new AirFerry.Windows.Bundle.ContinuousSaver(dir);
            ContinuousSavedCount = 0;
            ContinuousSkippedCount = 0;
            ContinuousItems.Clear();
        }
        ContinuousMode = true;
    }

    public void DisableContinuous()
    {
        // Keep the saver instance: toggling back on with the same folder
        // continues to dedup against earlier saves.
        ContinuousMode = false;
    }


    /// <summary>Temp dir for staging recovered bytes before archive.</summary>
    private static string TempDir => Path.Combine(Path.GetTempPath(), "AirFerry");

    /// <summary>
    /// Start the pipeline on <paramref name="source"/> (device, screen region
    /// or window). Idempotent — calling while running first stops the previous
    /// session.
    /// </summary>
    [RelayCommand]
    public void StartScan(ScanSource source)
    {
        StopScan();
        lock (_lifecycleGate)
        {
            if (!_deferredCleanupTask.IsCompleted)
            {
                StatusText = "上一个摄像头仍在后台释放，请稍后重试";
                return;
            }
        }
        Interlocked.Increment(ref _sessionEpoch);
        SelectedSource = source;
        IsComplete = false;
        IsRecovering = false;
        Progress = 0;
        ReceivedSymbolsText = "0";
        TotalSymbolsText = "0";
        LossRatioText = "0.0%";
        ResetLiveMetrics();
        RecoveryStageText = string.Empty;
        _preScanCheckedSession = null;

        try
        {
            uint zxingAbi = ZxingDecoder.AbiVersion();
            if (zxingAbi != 1)
            {
                throw new InvalidOperationException(
                    $"二维码解码库 ABI 不兼容（期望 1，实际 {zxingAbi}）");
            }
            uint nativeAbi = NativeBridge.NativeAbiVersion();
            if (nativeAbi < NativeBridge.NativeAbiVersion2)
            {
                throw new InvalidOperationException(
                    $"传输引擎 ABI 不兼容（期望 >= {NativeBridge.NativeAbiVersion2}，实际 {nativeAbi}）");
            }
            // §12 resume attempt BEFORE the first frame is ingested (the
            // receiver accepts resume only while unlocked). On success the
            // previous spill + ledger journal stay bound; on failure both are
            // dropped like any other leftover.
            _session = new ReceiverSession();
            if (!TryResumeFromLedger())
            {
                _chunkSpill?.Discard();
                _chunkSpill = null;
                _af2Ledger?.Discard();
                _af2Ledger = null;
                _pendingReverify = null;
            }
            Interlocked.Exchange(ref _recoveryStarted, 0);
            _capture = FrameSourceFactory.Create(source);
            if (!_capture.IsOpen)
            {
                StopScan();
                StatusText = source is DeviceSource
                    ? "无法打开设备，请检查是否被其他程序占用"
                    : $"无法打开视频源: {source.DisplayName}";
                return;
            }

            // The onDecoded callback runs under the pool's IngestLock. Returns true
            // when this symbol completes recovery so the pool stops ingesting.
            _pool = new QrDecodePool((payload, bbox) => OnDecoded(payload, bbox));
            _pool.Start();

            // Producer thread: pull frames and enqueue them. The pool handles the
            // drop-newest backpressure when workers can't keep up.
            _producerRunning = true;
            _producerThread = new Thread(ProducerLoop)
            {
                IsBackground = true,
                Name = "video-producer",
            };
            _producerThread.Start();

            IsScanning = true;
            StatusText = $"正在扫描… 视频源: {source.DisplayName}";
        }
        catch (Exception ex)
        {
            StopScan();
            StatusText = $"启动设备失败: {ex.Message}";
        }
    }

    [RelayCommand]
    public void StopScan()
    {
        Thread? producer;
        QrDecodePool? pool;
        IFrameSource? capture;
        ReceiverSession? session;
        ChunkSpillStore? spill;
        Task<RecoveryOutcome>? recoveryTask;
        Task cleanup;
        lock (_lifecycleGate)
        {
            _producerRunning = false;
            IsScanning = false;
            Interlocked.Increment(ref _sessionEpoch);

            // A previously detached camera read is still being cleaned up. Do
            // not lose that task or attempt to dispose the same pipeline twice.
            if (_capture is null && _pool is null && _session is null &&
                !_deferredCleanupTask.IsCompleted)
            {
                StatusText = "摄像头响应缓慢，正在后台安全释放…";
                return;
            }
            producer = _producerThread;
            _producerThread = null;
            pool = _pool;
            _pool = null;
            capture = _capture;
            _capture = null;
            session = _session;
            _session = null;
            spill = _chunkSpill;
            _chunkSpill = null;
            recoveryTask = _recoveryCoreTask;
            if (producer is null && pool is null && capture is null &&
                session is null && recoveryTask is null)
            {
                cleanup = Task.CompletedTask;
            }
            else
            {
                // Publish the cleanup task while still holding the lifecycle
                // gate. A simultaneous StopScan then observes it and cannot
                // detach/dispose a second copy of this pipeline.
                cleanup = Task.Run(() => CleanupDetachedPipeline(
                    producer, pool, capture, session, spill, recoveryTask));
                _deferredCleanupTask = cleanup;
            }
        }

        if (ReferenceEquals(cleanup, Task.CompletedTask))
        {
            ResetStoppedUi();
            return;
        }

        // Never free a capture, decode pool or Rust session while a producer,
        // native decode, ingest or recovery call may still be using it. Perform
        // the complete ordered teardown as one task. A wedged DirectShow read is
        // quarantined after a short wait so navigation remains responsive; the
        // task retains every resource and disposes them only after the read exits.
        Task completed = Task.WhenAny(cleanup, Task.Delay(TimeSpan.FromSeconds(2)))
            .GetAwaiter().GetResult();
        if (!ReferenceEquals(completed, cleanup))
        {
            _ = cleanup.ContinueWith(t =>
            {
                _ = t.Exception; // Observe a delayed teardown fault.
                lock (_lifecycleGate)
                {
                    if (ReferenceEquals(_deferredCleanupTask, cleanup))
                        _deferredCleanupTask = Task.CompletedTask;
                    if (ReferenceEquals(_recoveryCoreTask, recoveryTask))
                        _recoveryCoreTask = null;
                }
            }, CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously,
                TaskScheduler.Default);
            StatusText = "摄像头响应缓慢，正在后台安全释放…";
            IsRecovering = false;
            return;
        }

        try
        {
            cleanup.GetAwaiter().GetResult();
        }
        finally
        {
            lock (_lifecycleGate)
            {
                if (ReferenceEquals(_deferredCleanupTask, cleanup))
                    _deferredCleanupTask = Task.CompletedTask;
                if (ReferenceEquals(_recoveryCoreTask, recoveryTask))
                    _recoveryCoreTask = null;
            }
        }
        ResetStoppedUi();
    }

    private static void CleanupDetachedPipeline(
        Thread? producer,
        QrDecodePool? pool,
        IFrameSource? capture,
        ReceiverSession? session,
        ChunkSpillStore? spill,
        Task<RecoveryOutcome>? recoveryTask)
    {
        // Producer owns ReadGray/SnapshotBgr. It must exit before capture.Dispose.
        if (producer?.IsAlive == true) producer.Join();

        if (recoveryTask is not null)
        {
            try
            {
                recoveryTask.GetAwaiter().GetResult();
            }
            catch
            {
                // The UI continuation reports recovery errors. Teardown still
                // owns and must release all native/managed resources.
            }
        }

        try
        {
            if (pool is not null)
            {
                pool.RunExclusive(() =>
                {
                    pool.IngestStopped = true;
                    return true;
                });
                pool.Dispose();
            }
        }
        finally
        {
            try
            {
                spill?.Discard();
            }
            finally
            {
                try
                {
                    session?.Dispose();
                }
                finally
                {
                    capture?.Dispose();
                }
            }
        }
    }

    private void ResetStoppedUi()
    {
        IsRecovering = false;
        if (!IsComplete)
        {
            Progress = 0;
            ReceivedSymbolsText = "0";
            StatusText = "已停止";
        }
    }

    /// <summary>
    /// Reset for a fresh scan: clear completion + progress so a new transfer can
    /// start from zero.
    /// </summary>
    [RelayCommand]
    public void ResetSession()
    {
        StopScan();
        IsComplete = false;
        Progress = 0;
        ReceivedSymbolsText = "0";
        TotalSymbolsText = "0";
        LossRatioText = "0.0%";
        ResetLiveMetrics();
        RecoveryStageText = string.Empty;
        StatusText = "等待扫码…";
    }

    /// <summary>
    /// Producer: perform the only camera read, feed grayscale pixels to the
    /// decode pool, and publish a throttled BGR snapshot for preview.
    /// </summary>
    private void ProducerLoop()
    {
        long previewInterval = Math.Max(1, Stopwatch.Frequency / PreviewFps);
        long nextPreviewAt = 0;
        while (_producerRunning)
        {
            // Snapshot references once per iteration. StopScan may detach the
            // fields while a driver call is blocked, but keeps these objects
            // alive until this producer exits.
            IFrameSource? capture = _capture;
            QrDecodePool? pool = _pool;
            if (capture is null || pool is null) break;
            Mat? gray = capture.ReadGray();
            if (gray is null)
            {
                if (!capture.IsOpen)
                {
                    // Source is gone for good (captured window closed, region
                    // invalid, device died). StopScan joins this producer —
                    // dispatching it synchronously here would self-deadlock.
                    HandleFrameSourceLost();
                    break;
                }
                // Transient miss — a few nulls in a row is normal.
                Thread.Sleep(10);
                continue;
            }
            // Submit clones the pixels; the Mat itself is reused by VideoCapture.
            pool.Submit(gray);

            long now = Stopwatch.GetTimestamp();
            if (now >= nextPreviewAt)
            {
                PreviewFrame? preview = capture.SnapshotBgr();
                if (preview is not null)
                {
                    Action<PreviewFrame>? handler = PreviewFrameReady;
                    if (handler is null)
                    {
                        preview.Dispose();
                        nextPreviewAt = now + previewInterval;
                        continue;
                    }
                    try
                    {
                        // Ownership transfers to the single UI subscriber.
                        handler(preview);
                    }
                    catch
                    {
                        preview.Dispose();
                        // Preview is cosmetic. A subscriber must never kill the
                        // capture/decode producer thread.
                    }
                }
                nextPreviewAt = now + previewInterval;
            }
        }
    }

    /// <summary>
    /// The frame source died mid-scan (captured window closed, screen region
    /// invalid, device unplugged). Runs on the producer thread, so the stop
    /// itself is dispatched — <see cref="StopScan"/> joins the producer and
    /// would otherwise wait on itself. The status message is written after the
    /// stop so <c>ResetStoppedUi</c> cannot overwrite it.
    /// </summary>
    private void HandleFrameSourceLost()
    {
        _ = Task.Run(() =>
        {
            try
            {
                StopScan();
            }
            catch
            {
                // The cleanup path reports its own failures; the message below
                // is the actionable one either way.
            }
            StatusText = "视频源已关闭，扫描已停止";
        });
    }

    /// <summary>
    /// §12 crash recovery: rebuild the session from the most recent ledger
    /// journal before any frame is ingested (Resume requires an unlocked
    /// receiver). A journal without its spill file is worthless — the chunk
    /// bytes live there — and both are dropped by the caller.
    /// </summary>
    private bool TryResumeFromLedger()
    {
        Af2LedgerStore? ledger = Af2LedgerStore.LoadMostRecent(TempDir);
        if (ledger is null)
        {
            return false;
        }
        ReceiverSession? session = _session;
        if (session is null)
        {
            return false;
        }
        string spillPath = Path.Combine(
            TempDir, $"af2-{ledger.TransferIdHex}.partial");
        uint[] completed = ledger.CompletedIndices.Select(i => (uint)i).ToArray();
        if (!File.Exists(spillPath) || !session.Resume(ledger.RootFrameBytes, completed))
        {
            ledger.Discard();
            return false;
        }
        _af2Ledger = ledger;
        _chunkSpill = new ChunkSpillStore(TempDir, ledger.TransferIdHex);
        _pendingReverify = new SortedSet<int>(ledger.CompletedIndices);
        return true;
    }

    /// <summary>
    /// §12 reopen re-verification: once the Manifest is in, every resumed
    /// completed bit is checked against the spill bytes via the core's
    /// manifest-bound VerifyChunk; failures are invalidated (the sender's
    /// next epoch re-supplies them).
    /// </summary>
    private void ReverifyResumedChunks(ReceiverSession session)
    {
        SortedSet<int>? pending = _pendingReverify;
        Af2LedgerStore? ledger = _af2Ledger;
        ChunkSpillStore? spill = _chunkSpill;
        if (pending is null || pending.Count == 0 || ledger is null || spill is null)
        {
            return;
        }
        ReceiverSession.Snapshot snap = session.GetSnapshot();
        if (!snap.MetaConfirmed || snap.ChunkRawSize == 0)
        {
            return;
        }
        long crs = snap.ChunkRawSize;
        foreach (int i in pending.ToArray())
        {
            long off = i * crs;
            long len = Math.Clamp((long)snap.TotalRawSize - off, 0, crs);
            byte[]? bytes = spill.ReadRange(off, len);
            if (bytes is null)
            {
                continue; // spill short — retry on a later tick
            }
            pending.Remove(i);
            if (!session.VerifyChunk((uint)i, bytes))
            {
                session.InvalidateChunk((uint)i);
                ledger.Invalidate(i);
                System.Diagnostics.Debug.WriteLine(
                    $"[Af2] resumed chunk {i} failed re-verification; invalidated");
            }
        }
        if (pending.Count == 0)
        {
            _pendingReverify = null;
        }
    }

    /// <summary>
    /// Per-frame ingest callback (runs under <see cref="QrDecodePool.IngestLock"/>).
    /// Returns true when this symbol completes recovery.
    /// </summary>
    private bool OnDecoded(byte[] payload, int[]? unusedBbox)
    {
        QrDecodePool? pool = _pool;
        ReceiverSession? session = _session;
        if (pool is null || pool.IngestStopped || session is null)
        {
            return false;
        }
        IngestStatus? status = session.Ingest(payload);
        if (status is null)
        {
            return false;
        }
        IngestStatus s = status.Value;
        int epoch = Volatile.Read(ref _sessionEpoch);

        // Bounded-memory ledger: spill the chunk this frame completed to disk
        // and evict it from native memory, so peak native usage stays O(chunk)
        // instead of O(whole object). The serialized ingest thread drains it —
        // no extra synchronization.
        if (s.Accepted && s.ReceivedSymbols == 0)
        {
            // Relock (or first lock): a foreign Transfer owns the session now,
            // so the old spill's bytes belong to nobody. The ledger journal
            // follows — its ROOT/completed set reference the abandoned
            // transfer. (A resumed same-transfer session never gets here:
            // duplicate ROOTs are dropped, not re-locked.)
            _chunkSpill?.Discard();
            _chunkSpill = null;
            _af2Ledger?.Discard();
            _af2Ledger = null;
            _pendingReverify = null;
        }
        if (s.ManifestReady)
        {
            ReverifyResumedChunks(session);
        }
        if (s.ChunkReady)
        {
            try
            {
                ReceiverSession.Snapshot snap = session.GetSnapshot();
                ChunkSpillStore spill = _chunkSpill ??= new ChunkSpillStore(
                    TempDir, snap.TransferIdHex);
                int completedIndex = -1;
                session.DrainLastChunk((index, chunkRawSize, bytes) =>
                {
                    spill.Write(index, chunkRawSize, bytes);
                    completedIndex = index;
                });
                // §12 commit order: the chunk bytes were pwritten + flushed
                // into the spill inside DrainLastChunk; only now may the
                // ledger journal record the bit.
                if (completedIndex >= 0)
                {
                    Af2LedgerStore ledger = _af2Ledger ??= Af2LedgerStore.Create(
                        TempDir, snap.TransferIdHex, (int)snap.ChunkRawSize, snap.RootFrameBytes);
                    ledger.Commit(completedIndex);
                }
            }
            catch (Exception ex)
            {
                // A spilled-over disc / deleted temp dir must never kill the
                // ingest path — the native copy stays resident instead.
                System.Diagnostics.Debug.WriteLine($"chunk spill failed: {ex.Message}");
            }
        }

        if (s.Complete)
        {
            if (Interlocked.Exchange(ref _recoveryStarted, 1) == 0)
            {
                // Only UI state is changed on the dispatcher. Native assembly,
                // hashing and disk I/O run on the thread pool.
                System.Windows.Application.Current?.Dispatcher.BeginInvoke(() =>
                {
                    if (epoch != Volatile.Read(ref _sessionEpoch) ||
                        !ReferenceEquals(session, _session) ||
                        !ReferenceEquals(pool, _pool))
                    {
                        return;
                    }
                    IsComplete = true;
                    // Snapshot BOTH the mode and the saver instance at
                    // completion-detection time: an in-flight transfer keeps
                    // the folder that was active when it completed — changing
                    // the folder mid-recovery only affects the next transfer.
                    AirFerry.Windows.Bundle.ContinuousSaver? saver =
                        ContinuousMode ? _continuousSaver : null;
                    _ = RecoverAndStageAsync(session, pool, epoch, saver);
                });
            }
            return true;
        }
        return false;
    }

    /// <summary>
    /// Assemble + verify + stage the recovered bytes. Mirrors Android's
    /// <c>recoverAndStage</c> step by step.
    /// </summary>
    private async Task RecoverAndStageAsync(
        ReceiverSession session, QrDecodePool pool, int epoch,
        AirFerry.Windows.Bundle.ContinuousSaver? saver)
    {
        Task<RecoveryOutcome> coreTask;
        lock (_lifecycleGate)
        {
            if (epoch != Volatile.Read(ref _sessionEpoch) ||
                !ReferenceEquals(session, _session) ||
                !ReferenceEquals(pool, _pool) ||
                // a recovery may already own the pipeline
                _recoveryCoreTask is not null)
            {
                return;
            }
            coreTask = Task.Run(() => RecoverAndStageCore(session, pool, saver));
            _recoveryCoreTask = coreTask;
        }
        IsRecovering = true;
        RecoveryStageText = "正在组装数据…";

        RecoveryOutcome outcome;
        try
        {
            outcome = await coreTask;
        }
        catch (Exception ex)
        {
            ResetReceiverAfterRecoveryFailure(session, pool, epoch);
            if (epoch == Volatile.Read(ref _sessionEpoch))
            {
                IsComplete = false;
                IsRecovering = false;
                RecoveryStageText = string.Empty;
                StatusText = $"恢复失败: {ex.Message}";
            }
            return;
        }
        finally
        {
            lock (_lifecycleGate)
            {
                if (ReferenceEquals(_recoveryCoreTask, coreTask))
                {
                    _recoveryCoreTask = null;
                }
            }
        }

        HandleRecoveryOutcome(session, pool, epoch, saver, outcome);
    }

    /// <summary>
    /// Whole canonical stream for recovery: prefer the on-disk chunk spill
    /// (chunks were pwritten + evicted as they completed, so native memory
    /// stayed bounded during reception); fall back to the native in-memory
    /// assemble. Callers hold the ingest lock.
    /// </summary>
    private byte[]? ReadRecoveredStream(ReceiverSession session)
    {
        ChunkSpillStore? spill = _chunkSpill;
        ulong total = session.GetSnapshot().TotalRawSize;
        if (spill is not null && total > 0)
        {
            byte[]? fromFile = spill.ReadAll(total);
            if (fromFile is not null)
            {
                // §13 ⑧⑨ final gate BEFORE staging: entry hashes, UTF-8 text
                // and Content ID recompute over the full stream (Windows
                // holds the whole stream here by design).
                if (!session.VerifyFinalStream(fromFile))
                {
                    throw new InvalidOperationException(
                        "最终校验失败，请对准二维码重新接收");
                }
                // Consumed: staging may still fail, but the failure path resets
                // the whole receiver anyway, so no retry needs this file.
                spill.Discard();
                _chunkSpill = null;
                _af2Ledger?.Discard();
                _af2Ledger = null;
                _pendingReverify = null;
                return fromFile;
            }
        }
        byte[]? assembled = session.Assemble();
        if (assembled is not null && assembled.Length > 0 &&
            !session.VerifyFinalStream(assembled))
        {
            throw new InvalidOperationException("最终校验失败，请对准二维码重新接收");
        }
        return assembled;
    }

    private RecoveryOutcome RecoverAndStageCore(
        ReceiverSession session, QrDecodePool pool,
        AirFerry.Windows.Bundle.ContinuousSaver? saver)
    {
        pool.IngestStopped = true;

        // Take one coherent native snapshot under the ingest lock. No metadata
        // getter is allowed to outlive or race disposal of the native handle.
        AssembledPayload? payload = pool.RunExclusive<AssembledPayload?>(() =>
        {
            byte[]? bytes = ReadRecoveredStream(session);
            return bytes is null || bytes.Length == 0
                ? null
                : new AssembledPayload(
                    bytes,
                    session.Crc32(),
                    session.Crc32Known(),
                    session.FileName(),
                    session.FileSize());
        });
        if (payload is null)
        {
            return RecoveryOutcome.None();
        }

        // AF2: classify from the Manifest entry table (kind 2 = UTF8_TEXT,
        // multiple non-directory entries = bundle, else single file).
        IReadOnlyList<ReceiverSession.ManifestEntryDto> entries = Array.Empty<ReceiverSession.ManifestEntryDto>();
        try
        {
            entries = pool.RunExclusive(() => session.GetSnapshot().Entries)
                .Where(e => e.Kind != 3).ToList();
        }
        catch (ObjectDisposedException)
        {
            // Session torn down mid-recovery; nothing to stage.
            return RecoveryOutcome.None();
        }

        ulong receivedCrc = Crc32.Compute(payload.Bytes);
        ClassifiedPayload classified = ClassifyAf2Recovered(
            payload.Bytes, entries, payload.DisplayName);
        if (saver is not null)
        {
            return RecoveryOutcome.Continuous(TrySaveContinuous(saver, classified));
        }
        return RecoveryOutcome.Single(StageClassified(
            classified, payload.ExpectedCrc, payload.CrcKnown, receivedCrc));
    }

    /// <summary>
    /// Classify an assembled AF2 Canonical Content Stream using the Manifest
    /// entry table — no wire-magic sniffing. Slicing is bounds-checked; an
    /// out-of-range entry falls back to empty bytes for that member rather
    /// than throwing.
    /// </summary>
    private static ClassifiedPayload ClassifyAf2Recovered(
        byte[] stream,
        IReadOnlyList<ReceiverSession.ManifestEntryDto> entries,
        string displayName)
    {
        static byte[] Slice(byte[] s, ReceiverSession.ManifestEntryDto e)
        {
            long off = (long)e.Offset;
            long len = (long)e.Size;
            if (off < 0 || len < 0 || off + len > s.LongLength)
            {
                return Array.Empty<byte>();
            }
            byte[] outBuf = new byte[len];
            Array.Copy(s, off, outBuf, 0, len);
            return outBuf;
        }

        // Single UTF8_TEXT entry → the text UI (or a .txt file when oversized
        // / invalid UTF-8).
        if (entries.Count == 1 && entries[0].Kind == 2)
        {
            byte[] bytes = Slice(stream, entries[0]);
            string name = string.IsNullOrEmpty(entries[0].Path)
                ? "文字消息.txt"
                : entries[0].Path;
            return FileNameUtil.DecodeUtf8Strict(bytes) is { } text
                ? new ClassifiedPayload(RecoveredKind.EtText, name, (ulong)bytes.LongLength, bytes, text, null)
                : new ClassifiedPayload(RecoveredKind.SingleFile, name, (ulong)bytes.LongLength, bytes, null, null);
        }

        // Multiple entries → bundle, one member per entry.
        if (entries.Count > 1)
        {
            var files = entries
                .Select(e => new BundleFile(e.Path, Slice(stream, e)))
                .ToList();
            return new ClassifiedPayload(
                RecoveredKind.Bundle, displayName, (ulong)stream.LongLength, stream, null, files);
        }

        // Single file entry (or an empty-entry defensive fallback).
        if (entries.Count == 1)
        {
            byte[] bytes = Slice(stream, entries[0]);
            string name = string.IsNullOrEmpty(entries[0].Path)
                ? (string.IsNullOrEmpty(displayName) ? "received_file" : displayName)
                : entries[0].Path;
            if (FileNameUtil.IsTextLikeName(name) && FileNameUtil.FitsTextUi(bytes.LongLength))
            {
                return FileNameUtil.DecodeUtf8Strict(bytes) is { } text
                    ? new ClassifiedPayload(RecoveredKind.TextLikeFile, name, (ulong)bytes.LongLength, bytes, text, null)
                    : new ClassifiedPayload(RecoveredKind.SingleFile, name, (ulong)bytes.LongLength, bytes, null, null);
            }
            return new ClassifiedPayload(RecoveredKind.SingleFile, name, (ulong)bytes.LongLength, bytes, null, null);
        }

        // No manifest entries (defensive): treat the whole stream as one file.
        return new ClassifiedPayload(
            RecoveredKind.SingleFile, displayName, (ulong)stream.LongLength, stream, null, null);
    }

    private enum RecoveredKind { EtText, Bundle, TextLikeFile, SingleFile }

    private sealed record ClassifiedPayload(
        RecoveredKind Kind,
        string DisplayName,
        ulong OriginalSize,
        byte[] Bytes,
        string? Text,
        IReadOnlyList<BundleFile>? BundleFiles);

    /// <summary>Recovery pipeline outcome: a staged store result, or a
    /// continuous-folder save report, or neither (nothing stageable).</summary>
    private sealed record RecoveryOutcome(RecoveryResult? Result, ContinuousSaveReport? ContinuousReport)
    {
        public static RecoveryOutcome None() => new(null, null);
        public static RecoveryOutcome Single(RecoveryResult result) => new(result, null);
        public static RecoveryOutcome Continuous(ContinuousSaveReport report) => new(null, report);
    }

    /// <summary>Stage a classified payload into the ContentStore (single-receive mode).</summary>
    private RecoveryResult StageClassified(
        ClassifiedPayload c, ulong expectedCrc, bool crcKnown, ulong receivedCrc)
    {
        return c.Kind switch
        {
            RecoveredKind.EtText =>
                StageText(c.Text!, c.DisplayName, expectedCrc, crcKnown, receivedCrc),
            RecoveredKind.Bundle =>
                StageBundle(c.BundleFiles!, expectedCrc, crcKnown, receivedCrc)
                ?? StageSingleFile(c.Bytes, c.DisplayName, c.OriginalSize,
                    expectedCrc, crcKnown, receivedCrc),
            RecoveredKind.TextLikeFile =>
                StageTextLikeFile(c.Bytes, c.DisplayName, c.OriginalSize,
                    expectedCrc, crcKnown, receivedCrc, c.Text!),
            _ => StageSingleFile(c.Bytes, c.DisplayName, c.OriginalSize,
                expectedCrc, crcKnown, receivedCrc),
        };
    }

    /// <summary>
    /// Save a classified payload into the continuous folder snapshot taken at
    /// completion time. Disk failures become Failed reports — the pipeline
    /// keeps scanning either way.
    /// </summary>
    private ContinuousSaveReport TrySaveContinuous(
        AirFerry.Windows.Bundle.ContinuousSaver saver, ClassifiedPayload c)
    {
        try
        {
            return c.Kind switch
            {
                RecoveredKind.EtText => saver.SaveText(c.DisplayName, c.Text!),
                RecoveredKind.Bundle => saver.SaveBundle(c.BundleFiles!, null),
                // Text-like files are real files first: save the original
                // bytes, not a text re-encode.
                _ => saver.SaveSingle(c.DisplayName, c.Bytes),
            };
        }
        catch (Exception ex)
        {
            return ContinuousSaveReport.Failed(c.DisplayName, ex.Message);
        }
    }

    /// <summary>Swap in a fresh receiver (re-arm for the next transfer).</summary>
    private void SwapReceiverForNextSegment(ReceiverSession session, QrDecodePool pool)
    {
        lock (_lifecycleGate)
        {
            if (!ReferenceEquals(session, _session) || !ReferenceEquals(pool, _pool))
                return;
            pool.RunExclusive<bool>(() =>
            {
                session.Destroy();
                _chunkSpill?.Discard();
                _chunkSpill = null;
                _af2Ledger?.Discard();
                _af2Ledger = null;
                _pendingReverify = null;
                _session = new ReceiverSession();
                Interlocked.Exchange(ref _recoveryStarted, 0);
                pool.IngestStopped = false;
                return true;
            });
        }
    }

    /// <summary>
    /// The stable identity of the session's transfer: Content ID or Transfer ID
    /// from the AF2 Root / Manifest snapshot when confirmed, falling back to
    /// the session id. Callers must hold the ingest lock.
    /// </summary>
    private static string TransferIdentityOf(ReceiverSession session)
    {
        var snap = session.GetSnapshot();
        if (snap.MetaConfirmed)
        {
            if (!string.IsNullOrEmpty(snap.ContentIdHex)) return snap.ContentIdHex;
            if (!string.IsNullOrEmpty(snap.TransferIdHex)) return snap.TransferIdHex;
        }
        return session.SessionIdHex();
    }

    /// <summary>
    /// Transfer identity facts for the pre-scan duplicate check / identity recording:
    /// identity (session/content/transfer id), name, decompressed size.
    /// Callers must hold the ingest lock.
    /// </summary>
    private static AirFerry.Windows.Bundle.TransferProbe TransferProbeOf(
        ReceiverSession session)
    {
        var snap = session.GetSnapshot();
        return new AirFerry.Windows.Bundle.TransferProbe(
            TransferIdentityOf(session),
            session.FileName(),
            (long)snap.TotalRawSize,
            null,
            null);
    }

    /// <summary>
    /// Continuous mode: the descriptor just confirmed a transfer that was
    /// already saved (and whose folder copy is still intact) — skip receiving
    /// it entirely, record the skip and re-arm for the next file.
    /// Runs on the UI refresh cadence.
    /// </summary>
    private void SkipDuplicatedTransferAtDescriptor(
        ReceiverSession session, QrDecodePool pool, string identity)
    {
        bool segmented = pool.RunExclusive(() => session.IsSegmented());
        string name = pool.RunExclusive(() => session.FileName());
        name = string.IsNullOrEmpty(name) ? "未命名文件" : name;
        ContinuousSkippedCount++;
        ContinuousItems.Insert(0, new ContinuousReceivedItem(
            DateTime.Now.ToString("HH:mm:ss"),
            name,
            string.Empty,
            ContinuousItemStatus.Skipped));
        while (ContinuousItems.Count > 50)
        {
            ContinuousItems.RemoveAt(ContinuousItems.Count - 1);
        }
        StatusText = $"重复，已跳过: {name}（秒判，无需扫描）";
        FileSummaryText = "等待下一份文件…";
        Progress = 0;
        SwapReceiverForNextSegment(session, pool);
    }

    /// <summary>
    /// Shared tail of the recovery entry point (normal completion): publish
    /// the outcome, remember the transfer identity for pre-scan dedup, then
    /// re-arm for the next transfer
    /// (continuous) or hand the result to the view (single mode). Runs on the
    /// dispatcher thread.
    /// </summary>
    private void HandleRecoveryOutcome(
        ReceiverSession session, QrDecodePool pool, int epoch,
        AirFerry.Windows.Bundle.ContinuousSaver? saver, RecoveryOutcome outcome)
    {
        if (epoch != Volatile.Read(ref _sessionEpoch))
        {
            return;
        }

        IsRecovering = false;
        RecoveryStageText = string.Empty;
        if (outcome.Result is null && outcome.ContinuousReport is null)
        {
            IsComplete = false;
            StatusText = "组装失败";
            return;
        }
        if (saver is not null && outcome.ContinuousReport is not null)
        {
            if (outcome.ContinuousReport.Status != ContinuousSaveStatus.Failed)
            {
                // Remember the transfer identity so a replay of this transfer
                // is skipped at its descriptor next time (pre-scan dedup).
                saver.MarkTransfer(
                    pool.RunExclusive(() => TransferProbeOf(session)),
                    outcome.ContinuousReport);
            }
            // Continuous mode: record the folder save, re-arm a fresh receiver
            // and keep scanning — no navigation, no teardown.
            RecordContinuousOutcome(saver, outcome.ContinuousReport);
            ContinueNextTransfer(session, pool, epoch);
            return;
        }
        StatusText = "接收完成";
        TransferCompleted?.Invoke(outcome.Result!);
    }

    private bool ResetReceiverAfterRecoveryFailure(
        ReceiverSession session, QrDecodePool pool, int epoch)
    {
        lock (_lifecycleGate)
        {
            if (epoch != Volatile.Read(ref _sessionEpoch) ||
                !ReferenceEquals(session, _session) ||
                !ReferenceEquals(pool, _pool))
                return false;
            pool.RunExclusive<bool>(() =>
            {
                session.Destroy();
                _chunkSpill?.Discard();
                _chunkSpill = null;
                _af2Ledger?.Discard();
                _af2Ledger = null;
                _pendingReverify = null;
                _session = new ReceiverSession();
                Interlocked.Exchange(ref _recoveryStarted, 0);
                pool.IngestStopped = false;
                return true;
            });
            return true;
        }
    }

    /// <summary>
    /// Update the continuous counters/feed/status (dispatcher thread). The
    /// counters and feed describe the **currently selected** folder: when the
    /// folder was switched mid-transfer (the save went to the snapshot saver's
    /// older folder), only the status line reports it.
    /// </summary>
    private void RecordContinuousOutcome(
        AirFerry.Windows.Bundle.ContinuousSaver saver, ContinuousSaveReport report)
    {
        switch (report.Status)
        {
            case ContinuousSaveStatus.SkippedDuplicate:
                StatusText = ReferenceEquals(saver, _continuousSaver)
                    ? $"重复，已跳过: {report.DisplayName}"
                    : $"重复，已跳过: {report.DisplayName}（{saver.TargetDir}）";
                break;
            case ContinuousSaveStatus.Failed:
                StatusText = $"保存失败: {report.Error}（继续接收中）";
                break;
            default:
                StatusText = ReferenceEquals(saver, _continuousSaver)
                    ? $"已保存: {report.DisplayName}"
                    : $"已保存: {report.DisplayName}（{saver.TargetDir}）";
                break;
        }
        if (!ReferenceEquals(saver, _continuousSaver))
        {
            return;
        }
        ContinuousItemStatus status;
        string? error = null;
        switch (report.Status)
        {
            case ContinuousSaveStatus.SkippedDuplicate:
                ContinuousSkippedCount++;
                status = ContinuousItemStatus.Skipped;
                break;
            case ContinuousSaveStatus.Failed:
                status = ContinuousItemStatus.Failed;
                error = report.Error;
                break;
            default:
                ContinuousSavedCount++;
                status = ContinuousItemStatus.Saved;
                break;
        }
        ContinuousItems.Insert(0, new ContinuousReceivedItem(
            DateTime.Now.ToString("HH:mm:ss"),
            report.DisplayName,
            report.SizeBytes > 0 ? FormatBytes((ulong)report.SizeBytes) : string.Empty,
            status,
            error));
        while (ContinuousItems.Count > 50)
        {
            ContinuousItems.RemoveAt(ContinuousItems.Count - 1);
        }
    }

    /// <summary>
    /// Continuous mode's "receive the next file" step: re-arm a fresh receiver
    /// (the producer thread, decode pool and capture device keep running) and
    /// reset the per-transfer UI so the next descriptor starts from zero.
    /// </summary>
    private void ContinueNextTransfer(ReceiverSession session, QrDecodePool pool, int epoch)
    {
        IsComplete = false;
        Progress = 0;
        ReceivedSymbolsText = "0";
        TotalSymbolsText = "0";
        FileSummaryText = "等待下一份文件…";
        _rateSamples.Clear();
        _transferStartTimestamp = 0;
        _decodePerSecond = 0;
        _recentWireBytesPerSecond = 0;
        SwapReceiverForNextSegment(session, pool);
    }

    private RecoveryResult StageSingleFile(byte[] bytes, string displayName,
        ulong originalSize, ulong expectedCrc, bool crcKnown, ulong receivedCrc)
    {
        string finalName = string.IsNullOrEmpty(displayName) ? "received_file" : displayName;
        string crcHex = crcKnown ? expectedCrc.ToString("x") : "unknown";
        ContentStore.PutResult put = ContentStore.PutBytes(
            finalName, bytes, crcHex, crcUnknown: !crcKnown, kind: "file");
        return new RecoveryResult(
            SingleFilePath: put.Path,
            SingleFileSize: originalSize > 0 ? originalSize : (ulong)bytes.Length,
            ExpectedCrc32: expectedCrc,
            Crc32Known: crcKnown,
            ReceivedCrc32: receivedCrc,
            Bundle: null,
            BundleDir: null,
            DisplayName: finalName);
    }

    /// <summary>
    /// Stage a pure UTF8_TEXT manifest entry: store UTF-8 body under the
    /// entry name (user-chosen on sender; default "文字消息.txt").
    /// </summary>
    private RecoveryResult StageText(string text, string displayName,
        ulong expectedCrc, bool crcKnown, ulong receivedCrc)
    {
        // Store the UTF-8 body, while retaining transport CRC
        // fields so corruption is not hidden by recomputing a different hash.
        string finalName = string.IsNullOrEmpty(displayName)
            ? "文字消息.txt"
            : (displayName.Contains('.') ? displayName : displayName + ".txt");
        byte[] contentBytes = Encoding.UTF8.GetBytes(text);
        ulong contentCrc = Crc32.Compute(contentBytes);
        string crcHex = contentCrc.ToString("x");
        ContentStore.PutResult put = ContentStore.PutBytes(
            finalName, contentBytes, crcHex, crcUnknown: false, kind: "text");
        return new RecoveryResult(
            SingleFilePath: put.Path,
            SingleFileSize: (ulong)contentBytes.Length,
            ExpectedCrc32: expectedCrc,
            Crc32Known: crcKnown,
            ReceivedCrc32: receivedCrc,
            Bundle: null,
            BundleDir: null,
            Text: text,
            DisplayName: finalName);
    }

    /// <summary>
    /// Stage a text-like single file into ContentStore and keep text for the copy UI.
    /// </summary>
    private RecoveryResult StageTextLikeFile(byte[] bytes, string displayName,
        ulong originalSize, ulong expectedCrc, bool crcKnown, ulong receivedCrc, string text)
    {
        string finalName = string.IsNullOrEmpty(displayName) ? "文字消息.txt" : displayName;
        ContentStore.PutResult put = ContentStore.PutBytes(
            finalName, bytes,
            crcHex: crcKnown ? expectedCrc.ToString("x") : "unknown",
            crcUnknown: !crcKnown, kind: "text");
        return new RecoveryResult(
            SingleFilePath: put.Path,
            SingleFileSize: originalSize > 0 ? originalSize : (ulong)bytes.Length,
            ExpectedCrc32: expectedCrc,
            Crc32Known: crcKnown,
            ReceivedCrc32: receivedCrc,
            Bundle: null,
            BundleDir: null,
            Text: text,
            DisplayName: finalName);
    }

    private RecoveryResult? StageBundle(
        IReadOnlyList<BundleFile> files, ulong expectedCrc, bool crcKnown, ulong receivedCrc)
    {
        if (files.Count == 0)
        {
            return null;
        }
        string bundleId = Guid.NewGuid().ToString("N");
        string bundleTitle = $"发送_{DateTime.Now:MMdd_HHmmss}";
        ContentStore.PutBytesBatch(files.Select(f =>
            new ContentStore.PutBytesRequest(
                f.Name, f.Data, Kind: "file",
                BundleId: bundleId, BundleTitle: bundleTitle)).ToList());
        return new RecoveryResult(
            SingleFilePath: null,
            SingleFileSize: null,
            ExpectedCrc32: expectedCrc,
            Crc32Known: crcKnown,
            ReceivedCrc32: receivedCrc,
            // Keep in-memory bytes for the bundle UI; disk is content-addressed.
            Bundle: files.Select(f => new BundleFile(f.Name, f.Data)).ToList(),
            BundleDir: null);
    }

    /// <summary>
    /// Periodically poll progress for the live UI (called by a timer at ~7 Hz).
    /// Keeps the hot ingest path allocation-free.
    /// </summary>
    public void RefreshProgress()
    {
        QrDecodePool? pool = _pool;
        ReceiverSession? session = _session;
        long now = Stopwatch.GetTimestamp();
        if (pool is not null)
        {
            ScanMetricsText = $"采集 {pool.CapturedFrames} 帧 · " +
                $"丢弃 {pool.DroppedFrames} 帧 · 解码 {pool.DecodedSymbols} 码";
        }
        if (pool is null || session is null)
        {
            return;
        }
        // Service a pending sender-switch re-lock (continuous mode). Runs on
        // the UI thread holding no locks — the same context as every other
        // swap call site, so the lifecycle/ingest lock order stays uniform.
        LiveSnapshot live = pool.RunExclusive(() =>
        {
            if (!session.IsInitialized)
            {
                return new LiveSnapshot(null, string.Empty, 0, 0, 0);
            }
            return new LiveSnapshot(
                session.Progress(),
                session.FileName(),
                session.FileSize(),
                session.SymbolSizeBytes,
                session.EstimatedTotalSymbols);
        });
        if (live.Progress is null)
        {
            return;
        }
        ProgressSnapshot p = live.Progress.Value;
        // Pre-scan duplicate check runs exactly once per receiver session,
        // the moment a descriptor is confirmed — before any meaningful data
        // ingest: the continuous-mode whole-transfer identity skip.
        if (p.MetaConfirmed && !p.Complete &&
            !ReferenceEquals(session, _preScanCheckedSession))
        {
            _preScanCheckedSession = session;
            // Continuous mode pre-scan dedup: the moment a descriptor is
            // confirmed the transfer identity is already known (content-derived
            // session id) — if this run already saved it AND the folder copy
            // still verifies intact, skip the whole receive instead of
            // re-scanning.
            if (ContinuousMode && _continuousSaver is not null)
            {
                var probe = pool.RunExclusive(() => TransferProbeOf(session));
                if (_continuousSaver.ShouldSkipTransfer(probe))
                {
                    SkipDuplicatedTransferAtDescriptor(session, pool, probe.Identity);
                    return;
                }
            }
        }
        UpdateRates(now, pool.DecodedSymbols, p.ReceivedSymbols, live.SymbolSize, p.Complete);
        UpdateFileSummary(live, p);

        if (p.TotalSymbols > 0)
        {
            if (_transferStartTimestamp == 0)
            {
                _transferStartTimestamp = now;
            }
            Progress = p.Complete
                ? 100
                : Math.Clamp(p.ReceivedSymbols * 100.0 / p.TotalSymbols, 0, 100);
            TotalSymbolsText = p.TotalSymbols.ToString();
        }
        else if (p.ReceivedSymbols > 0)
        {
            Progress = live.EstimatedTotalSymbols > 0
                ? Math.Clamp(p.ReceivedSymbols * 100.0 / live.EstimatedTotalSymbols, 0, 15)
                : 0;
        }
        ReceivedSymbolsText = p.ReceivedSymbols.ToString();
        LossRatioText = $"{p.LossRatio * 100:F1}%";

        if (!IsRecovering)
        {
            var legacy = _session?.GetSnapshot().LegacyPeerFrames ?? 0;
            StatusText = p.Complete
                ? "文件恢复完成"
                : legacy > 0
                    ? $"检测到旧版 v1 协议二维码（已拒 {legacy} 帧），请将发送端升级到 AF2 版本"
                    : !p.MetaConfirmed && p.ReceivedSymbols > 0
                        ? $"正在同步…已缓存 {p.ReceivedSymbols} 个符号"
                        : p.TotalSymbols == 0
                            ? "等待二维码…"
                            : p.ReceivedSymbols > 0 && p.DecodedBlocks == 0
                                ? $"接收中… {p.ReceivedSymbols}/{p.TotalSymbols}（等待解码）"
                                : $"恢复中… {Progress:F0}%";
        }
    }

    private void UpdateRates(long now, long decoded, long received, uint symbolSize, bool complete)
    {
        if (complete)
        {
            _rateSamples.Clear();
            _decodePerSecond = 0;
            _recentWireBytesPerSecond = 0;
        }
        else if (decoded > 0 || received > 0)
        {
            _rateSamples.Enqueue(new RateSample(now, decoded, received));
            long cutoff = now - Stopwatch.Frequency * RateWindowSeconds;
            while (_rateSamples.Count > 1 && _rateSamples.Peek().Timestamp < cutoff)
            {
                _rateSamples.Dequeue();
            }
            if (_rateSamples.Count >= 2)
            {
                RateSample oldest = _rateSamples.Peek();
                RateSample newest = _rateSamples.Last();
                long elapsedTicks = newest.Timestamp - oldest.Timestamp;
                if (elapsedTicks >= Stopwatch.Frequency * RateMinMilliseconds / 1000)
                {
                    long decodedDelta = Math.Max(0, newest.DecodedSymbols - oldest.DecodedSymbols);
                    long receivedDelta = Math.Max(0, newest.ReceivedSymbols - oldest.ReceivedSymbols);
                    _decodePerSecond = (long)Math.Min(long.MaxValue,
                        decodedDelta * (double)Stopwatch.Frequency / elapsedTicks);
                    _recentWireBytesPerSecond = (long)Math.Min(long.MaxValue,
                        receivedDelta * (double)symbolSize * Stopwatch.Frequency / elapsedTicks);
                }
            }
        }

        TimeSpan elapsed = _transferStartTimestamp == 0
            ? TimeSpan.Zero
            : TimeSpan.FromSeconds((now - _transferStartTimestamp) /
                (double)Stopwatch.Frequency);
        TransferMetricsText = $"解码 {_decodePerSecond} 符号/秒 · " +
            $"有效 {FormatBytes((ulong)Math.Max(0, _recentWireBytesPerSecond))}/s · " +
            $"用时 {FormatDuration(elapsed)}";
    }

    private void UpdateFileSummary(LiveSnapshot live, ProgressSnapshot progress)
    {
        if (string.IsNullOrWhiteSpace(live.FileName))
        {
            FileSummaryText = "等待描述符…";
            return;
        }
        string original = live.FileSize > 0 ? FormatBytes(live.FileSize) : "大小未知";
        ulong wireBytes = progress.TotalSymbols > 0
            ? (ulong)progress.TotalSymbols * live.SymbolSize
            : 0;
        FileSummaryText = wireBytes > 0
            ? $"{live.FileName} · {original} → 传输 {FormatBytes(wireBytes)}"
            : $"{live.FileName} · {original}";
    }

    private void ResetLiveMetrics()
    {
        ScanMetricsText = "采集 0 帧 · 丢弃 0 帧 · 解码 0 码";
        FileSummaryText = "等待描述符…";
        TransferMetricsText = "解码 0 符号/秒 · 有效 0 B/s · 用时 00:00";
        _rateSamples.Clear();
        _transferStartTimestamp = 0;
        _decodePerSecond = 0;
        _recentWireBytesPerSecond = 0;
    }

    private static string FormatBytes(ulong bytes)
    {
        string[] units = ["B", "KB", "MB", "GB", "TB"];
        double value = bytes;
        int unit = 0;
        while (value >= 1024 && unit < units.Length - 1)
        {
            value /= 1024;
            unit++;
        }
        return unit == 0 ? $"{bytes} B" : $"{value:F1} {units[unit]}";
    }

    private static string FormatDuration(TimeSpan elapsed) => elapsed.TotalHours >= 1
        ? $"{(int)elapsed.TotalHours:D2}:{elapsed.Minutes:D2}:{elapsed.Seconds:D2}"
        : $"{elapsed.Minutes:D2}:{elapsed.Seconds:D2}";

    /// <summary>
    /// Ensure <paramref name="sourcePath"/> is in ContentStore (idempotent if already a blob).
    /// Returns the canonical blob path.
    /// </summary>
    public static string ArchiveSingleFile(string sourcePath, string displayName)
    {
        if (File.Exists(sourcePath) &&
            sourcePath.StartsWith(ContentStore.RootDir, StringComparison.OrdinalIgnoreCase))
        {
            return sourcePath;
        }
        byte[] bytes = File.Exists(sourcePath) ? File.ReadAllBytes(sourcePath) : [];
        return ContentStore.PutBytes(displayName, bytes).Path;
    }

    /// <summary>Archive a bundle into ContentStore (content-addressed members).</summary>
    public static string ArchiveBundle(IReadOnlyList<BundleFile> files)
    {
        string bundleId = Guid.NewGuid().ToString("N");
        string bundleTitle = $"发送_{DateTime.Now:MMdd_HHmmss}";
        // One batched index write instead of one full index rewrite per entry
        // (PutBytes rewrites index.json for every call → O(n²) for a bundle).
        IReadOnlyList<ContentStore.PutResult> results = ContentStore.PutBytesBatch(
            files.Select(f => new ContentStore.PutBytesRequest(
                f.Name, f.Data, Kind: "file",
                BundleId: bundleId, BundleTitle: bundleTitle)).ToList());
        return results.Count > 0 ? results[0].Path : ContentStore.RootDir;
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;
        StopScan();
    }
}
