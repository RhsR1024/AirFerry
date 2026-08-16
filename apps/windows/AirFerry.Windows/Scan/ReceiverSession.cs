using System.Runtime.InteropServices;
using System.Text;
using AirFerry.Windows.Native;

namespace AirFerry.Windows.Scan;

/// <summary>
/// High-level receiver session manager — the Windows equivalent of Android's
/// <c>ReceiverSessionManager.kt</c>. Wraps the Rust C ABI and drives the same
/// state machine: lazy-init from the first descriptor frame, then a forced
/// re-init if the session-mismatch streak climbs without ever accepting a
/// symbol.
/// </summary>
/// <remarks>
/// <para>
/// The native receiver is <b>only</b> initialized from a descriptor frame
/// (<see cref="FrameHeader.FlagDescriptor"/>). Ordinary data frames are
/// silently dropped until a descriptor arrives. This prevents a corrupted
/// first QR decode (which only passes magic+version but may carry a garbage
/// session_id) from permanently locking out every subsequent correct frame.
/// </para>
/// <para>
/// Once initialized, a persistent session-mismatch streak with zero accepted
/// symbols triggers a forced re-init from the next descriptor that arrives —
/// covering the edge-case where the first descriptor itself was corrupted but
/// a later one is clean.
/// </para>
/// <para>
/// Every access to the native handle is serialized by this wrapper. Callers may
/// poll progress while ingest is running and may dispose only after producers
/// have been stopped.
/// </para>
/// </remarks>
public sealed class ReceiverSession : IDisposable
{
    private readonly object _gate = new();
    private IntPtr _handle = IntPtr.Zero;
    private ulong _sessionIdLo;
    private ulong _sessionIdHi;
    private uint _symbolSize;
    private bool _initialized;
    private int _estimatedTotalSymbols;
    private int _mismatchStreak;
    private bool _everAccepted;

    public bool IsInitialized { get { lock (_gate) return _initialized; } }
    public int EstimatedTotalSymbols { get { lock (_gate) return _estimatedTotalSymbols; } }
    public uint SymbolSizeBytes { get { lock (_gate) return _symbolSize; } }

    /// <summary>
    /// Ingest a decoded QR payload. Returns a lightweight
    /// <see cref="IngestStatus"/> (no JSON) so the hot ingest path doesn't
    /// allocate/parse a string per frame; call <see cref="Progress"/> on the
    /// UI refresh cadence for the full snapshot.
    /// </summary>
    public IngestStatus? Ingest(byte[] frameBytes)
    {
        lock (_gate)
        {
        FrameHeader? header = FrameHeader.Parse(frameBytes);
        if (header is null)
        {
            return null;
        }
        FrameHeader h = header.Value;

        // Cache estimated total symbols from the first frame for approximate
        // UI progress before the descriptor arrives.
        if (_estimatedTotalSymbols == 0 && h.TotalSymbols > 0)
        {
            _estimatedTotalSymbols = (int)h.TotalSymbols;
        }

        bool isDescriptor = h.IsDescriptor;

        // --- Lazy init: only from descriptor frames ---
        // Until a descriptor arrives the authoritative OTI is unknown; feeding
        // data frames to a guessed decoder (the old path) corrupted multi-block
        // recovery. Drop them silently and wait.
        if (!_initialized)
        {
            if (!isDescriptor)
            {
                return null; // wait for a descriptor
            }
            CreateReceiver(h);
            if (!_initialized)
            {
                return null;
            }
        }

        // --- Session-mismatch re-init ---
        // If the streak is high and we never accepted anything, the first
        // descriptor was likely corrupt → destroy and re-init on the next
        // descriptor (the next Ingest call re-enters the lazy-init block above).
        if (_initialized && !isDescriptor && _mismatchStreak >= 3 && !_everAccepted)
        {
            Destroy();
            return null;
        }

        ulong word = NativeBridge.ReceiverIngest(_handle, frameBytes, (nuint)frameBytes.Length);
        IngestStatus? status = IngestStatus.Unpack(word);
        if (status is null)
        {
            return null; // error sentinel: rejected frame, nothing to do.
        }
        IngestStatus s = status.Value;

        // Track mismatch streak for the re-init heuristic above.
        if (s.MismatchStreak >= 3)
        {
            _mismatchStreak = s.MismatchStreak;
        }
        else if (s.Accepted)
        {
            _everAccepted = true;
            _mismatchStreak = 0;
            if (s.ReceivedSymbols == 0)
            {
                // Relocked in native AF2: clear stale snapshot cache.
                _cachedSnapshot = null;
            }
        }

        return s;
        }
    }

    /// <summary>
    /// Full progress snapshot (parsed from the on-demand JSON). Intended for
    /// the UI refresh cadence (~7 Hz), NOT per-frame. Returns <see langword="null"/>
    /// if the session isn't initialized or the native call fails.
    /// </summary>
    public ProgressSnapshot? Progress()
    {
        lock (_gate)
        {
        if (!_initialized || _handle == IntPtr.Zero)
        {
            return null;
        }

        // Two-pass length protocol: first learn the required length, then fill.
        nuint needed = NativeBridge.ReceiverProgressJson(_handle, null, 0);
        if (needed == 0)
        {
            return null;
        }
        if (needed > int.MaxValue)
        {
            return null;
        }
        byte[] buf = new byte[(int)needed];
        nuint written = NativeBridge.ReceiverProgressJson(_handle, buf, (nuint)buf.Length);
        if (written == 0 || written > (nuint)buf.Length)
        {
            return null;
        }
        // The JSON is NUL-terminated; trim the trailing NUL before parsing.
        int len = (int)written - 1;
        if (len <= 0)
        {
            return null;
        }
        string json = Encoding.UTF8.GetString(buf, 0, len);
        return ProgressSnapshot.Parse(json);
        }
    }

    public bool IsComplete
    {
        get
        {
            lock (_gate)
            {
                return _initialized && NativeBridge.ReceiverIsComplete(_handle) == 1;
            }
        }
    }

    // ── descriptor snapshot (ReceiverSnapshotV1) ─────────────────────────────
    //
    // The former 16 per-field P/Invoke getters were folded into ONE
    // `airferry_receiver_snapshot_json` call (native ABI v2). The public
    // per-field methods below keep their shapes so callers are unchanged, but
    // read a cached snapshot: descriptor-derived fields are immutable once
    // `meta_confirmed` is true, so the cache refreshes only until confirmation
    // and freezes afterwards — one P/Invoke + one JSON parse per session
    // instead of 16 per UI refresh.

    /// <summary>Parsed <c>ReceiverSnapshotV2</c> fields.</summary>
    public sealed class Snapshot
    {
        public bool MetaConfirmed;
        public string TransferIdHex = "";
        public string ContentIdHex = "";
        public ulong TotalRawSize;
        public uint EntryCount;
        public uint ChunkCount;
        public uint ChunkRawSize;
        public IReadOnlyList<ManifestEntryDto> Entries = Array.Empty<ManifestEntryDto>();
    }

    /// <summary>One AF2 Manifest entry (kind/path/offset/size).</summary>
    public sealed record ManifestEntryDto(int Kind, string Path, ulong Offset, ulong Size);

    private Snapshot? _cachedSnapshot;

    /// <summary>Current snapshot (cached once the manifest has entries).</summary>
    public Snapshot GetSnapshot()
    {
        lock (_gate)
        {
            if (!_initialized)
            {
                return new Snapshot();
            }
            if (_cachedSnapshot is { MetaConfirmed: true, Entries.Count: > 0 } cached)
            {
                return cached;
            }
            IntPtr ptr = NativeBridge.ReceiverSnapshotJson(_handle);
            if (ptr == IntPtr.Zero)
            {
                return _cachedSnapshot ?? new Snapshot();
            }
            try
            {
                string json = Marshal.PtrToStringUTF8(ptr) ?? "";
                using var doc = System.Text.Json.JsonDocument.Parse(
                    json, new System.Text.Json.JsonDocumentOptions { AllowTrailingCommas = true });
                var root = doc.RootElement;
                var snap = new Snapshot
                {
                    MetaConfirmed = root.TryGetProperty("meta_confirmed", out var mc) && mc.GetBoolean(),
                    TransferIdHex = root.TryGetProperty("transfer_id_hex", out var tid) ? tid.GetString() ?? "" : "",
                    ContentIdHex = root.TryGetProperty("content_id_hex", out var cid) ? cid.GetString() ?? "" : "",
                    TotalRawSize = root.TryGetProperty("total_raw_size", out var trs) ? trs.GetUInt64() : 0UL,
                    EntryCount = root.TryGetProperty("entry_count", out var ec) ? (uint)ec.GetUInt64() : 0u,
                    ChunkCount = root.TryGetProperty("chunk_count", out var cc) ? (uint)cc.GetUInt64() : 0u,
                    ChunkRawSize = root.TryGetProperty("chunk_raw_size", out var crs) ? (uint)crs.GetUInt64() : 0u,
                };
                if (root.TryGetProperty("entries", out var entriesEl) &&
                    entriesEl.ValueKind == System.Text.Json.JsonValueKind.Array)
                {
                    var list = new List<ManifestEntryDto>(entriesEl.GetArrayLength());
                    foreach (var e in entriesEl.EnumerateArray())
                    {
                        list.Add(new ManifestEntryDto(
                            e.TryGetProperty("kind", out var k) ? k.GetInt32() : 1,
                            e.TryGetProperty("path", out var p) ? p.GetString() ?? "" : "",
                            e.TryGetProperty("offset", out var o) ? o.GetUInt64() : 0UL,
                            e.TryGetProperty("size", out var s) ? s.GetUInt64() : 0UL));
                    }
                    snap.Entries = list;
                }
                _cachedSnapshot = snap;
                return snap;
            }
            catch (System.Text.Json.JsonException)
            {
                return _cachedSnapshot ?? new Snapshot();
            }
            finally
            {
                NativeBridge.FreeString(ptr);
            }
        }
    }

    public string FileName()
    {
        var snap = GetSnapshot();
        var nonDir = snap.Entries.FindAll(e => e.Kind != 3);
        if (nonDir.Count == 1) return nonDir[0].Path;
        if (nonDir.Count > 1) return $"多文件传输包 ({nonDir.Count} 项)";
        if (snap.EntryCount > 1) return $"多文件传输包 ({snap.EntryCount} 项)";
        return "文件传输";
    }
    public ulong FileSize() => GetSnapshot().TotalRawSize;
    public bool IsSegmented() => GetSnapshot().ChunkCount > 1;
    public uint SegmentIndex() => 0u;
    public uint SegmentCount() => Math.Max(GetSnapshot().ChunkCount, 1u);
    public ulong RootOriginalSize() => GetSnapshot().TotalRawSize;

    public byte[]? AssembleChunk(uint index)
    {
        lock (_gate)
        {
            if (!_initialized) return null;
            int ok = NativeBridge.ReceiverAssembleChunk(_handle, index, out IntPtr buf, out nuint len);
            if (ok == 0 || buf == IntPtr.Zero || len == 0) return null;
            try
            {
                byte[] data = new byte[(int)len];
                Marshal.Copy(buf, data, 0, (int)len);
                return data;
            }
            finally
            {
                NativeBridge.BufferFree(buf, len);
            }
        }
    }

    /// <summary>
    /// Recover the assembled file bytes, trimming RaptorQ zero-padding back to
    /// the descriptor's original size (mirrors Android's
    /// <c>recoverAndStage</c>). Returns <see langword="null"/> if not complete.
    /// </summary>
    /// <remarks>
    /// The buffer returned by the Rust side is Rust-allocated; this method
    /// copies the bytes into a managed array and frees the native allocation
    /// before returning, so the caller never touches unmanaged memory.
    /// </remarks>
    public byte[]? Assemble()
    {
        lock (_gate)
        {
            if (!_initialized)
            {
                return null;
            }
            int ok = NativeBridge.ReceiverAssemble(_handle, out IntPtr buf, out nuint len);
            if (ok == 0 || buf == IntPtr.Zero || len == 0)
            {
                return null;
            }
            if (len > int.MaxValue)
            {
                NativeBridge.BufferFree(buf, len);
                return null;
            }
            try
            {
                byte[] all = new byte[(int)len];
                Marshal.Copy(buf, all, 0, (int)len);
                return all;
            }
            finally
            {
                NativeBridge.BufferFree(buf, len);
            }
        }
    }

    /// <summary>This object's transmitted payload length.</summary>
    public ulong CompressedSize() => GetSnapshot().TotalRawSize;

    /// <summary>Whole decompressed original size.</summary>
    public ulong OriginalSize() => GetSnapshot().TotalRawSize;

    /// <summary>Session id as a lowercase hex string (high||low, 32 chars).</summary>
    public string SessionIdHex()
    {
        lock (_gate)
        {
            string lo = _sessionIdLo.ToString("x16");
            string hi = _sessionIdHi.ToString("x16");
            return hi + lo;
        }
    }

    /// <summary>True when the id equals the locked session's (false while uninitialized).</summary>
    public bool MatchesLocked(ulong lo, ulong hi)
    {
        lock (_gate)
        {
            return _initialized && lo == _sessionIdLo && hi == _sessionIdHi;
        }
    }

    /// <summary>The locked session id, for frame-level filtering (false while uninitialized).</summary>
    public bool TryGetLockedSessionId(out ulong lo, out ulong hi)
    {
        lock (_gate)
        {
            lo = _sessionIdLo;
            hi = _sessionIdHi;
            return _initialized;
        }
    }

    /// <summary>Create (or re-create) the native receiver from a parsed header.</summary>
    private void CreateReceiver(FrameHeader h)
    {
        _sessionIdLo = h.SessionIdLo;
        _sessionIdHi = h.SessionIdHi;
        _symbolSize = h.SymbolSize > 0 ? h.SymbolSize : 1024;
        _handle = NativeBridge.ReceiverCreate(_sessionIdLo, _sessionIdHi);
        _initialized = _handle != IntPtr.Zero;
        _mismatchStreak = 0;
        _everAccepted = false;
        _cachedSnapshot = null;
    }

    public void Destroy()
    {
        lock (_gate)
        {
        if (_initialized && _handle != IntPtr.Zero)
        {
            NativeBridge.ReceiverDestroy(_handle);
            _handle = IntPtr.Zero;
            _initialized = false;
        }
        }
    }

    public void Dispose() => Destroy();
}
