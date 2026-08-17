namespace AirFerry.Windows.Scan;

/// <summary>
/// Sparse on-disk staging for completed AF2 chunks — the receiver-side half of
/// the bounded-memory ledger (plan E2), the C# twin of Android's
/// <c>ChunkSpillStore.kt</c>.
/// <para>
/// Completed chunks are RAW (post-decode, post-decompress) and fixed-size
/// except the last, so the spill file's layout IS the canonical content
/// stream: chunk <c>i</c> lives at byte offset <c>i * chunkRawSize</c>. The
/// file is then read at recovery time so the full stream never has to exist
/// in native memory — chunks are evicted (<c>ReceiverForgetChunk</c>) as soon
/// as they are spilled.
/// </para>
/// <para>
/// Only ever touched from the pool's serialized ingest callback (under
/// <c>IngestLock</c>) and the recovery core that runs under the same lock, so
/// a single <see cref="FileStream"/> needs no extra synchronization.
/// </para>
/// </summary>
public sealed class ChunkSpillStore : IDisposable
{
    private readonly string _path;
    private FileStream? _stream;

    public ChunkSpillStore(string dir, string transferIdHex)
    {
        Directory.CreateDirectory(dir);
        string id = string.IsNullOrEmpty(transferIdHex) ? "session" : transferIdHex;
        _path = Path.Combine(dir, $"af2-{id}.partial");
        // A same-id orphan from an earlier attempt must not leak bytes into
        // this transfer's stream.
        try { File.Delete(_path); } catch (IOException) { }
    }

    /// <summary>pwrite one completed chunk at its canonical-stream offset.</summary>
    public void Write(int index, int chunkRawSize, byte[] bytes)
    {
        if (index < 0 || chunkRawSize <= 0 || bytes.Length == 0)
        {
            return;
        }
        FileStream fs = _stream ??= new FileStream(
            _path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
        fs.Seek((long)index * chunkRawSize, SeekOrigin.Begin);
        fs.Write(bytes, 0, bytes.Length);
        try
        {
            fs.Flush(true);
        }
        catch (IOException ex)
        {
            System.Diagnostics.Debug.WriteLine($"[ChunkSpillStore] flush failed: {ex.Message}");
        }
    }

    /// <summary>Current spill size in bytes (0 when nothing was spilled yet).</summary>
    public long Length()
    {
        if (_stream is not null)
        {
            return _stream.Length;
        }
        return File.Exists(_path) ? new FileInfo(_path).Length : 0L;
    }

    /// <summary>
    /// Read the whole canonical stream back (recovery time). Returns
    /// <see langword="null"/> when the spill is shorter than
    /// <paramref name="totalRawSize"/> (incomplete) — callers then fall back to
    /// the native assemble path.
    /// </summary>
    public byte[]? ReadAll(ulong totalRawSize)
    {
        if (totalRawSize == 0 || totalRawSize > int.MaxValue)
        {
            return null;
        }
        if (_stream is null && !File.Exists(_path))
        {
            return null;
        }
        FileStream fs;
        try
        {
            fs = _stream ??= new FileStream(
                _path, FileMode.Open, FileAccess.Read, FileShare.Read);
        }
        catch (IOException)
        {
            return null;
        }
        if ((ulong)fs.Length < totalRawSize)
        {
            return null;
        }
        var buf = new byte[totalRawSize];
        int done = 0;
        while (done < buf.Length)
        {
            int n = fs.Read(buf, done, buf.Length - done);
            if (n <= 0)
            {
                return null;
            }
            done += n;
        }
        return buf;
    }

    /// <summary>Close and delete the spill (relocked / consumed / abandoned).</summary>
    public void Discard()
    {
        try { _stream?.Dispose(); } catch (IOException) { }
        _stream = null;
        try { File.Delete(_path); } catch (IOException) { }
    }

    /// <summary>
    /// Read one canonical-stream range (§12 reopen re-verification reads
    /// individual chunks back). Returns null when the spill is shorter than
    /// the requested range end.
    /// </summary>
    public byte[]? ReadRange(long offset, long size)
    {
        if (offset < 0 || size < 0 || size > int.MaxValue)
        {
            return null;
        }
        if (_stream is null && !File.Exists(_path))
        {
            return null;
        }
        FileStream fs;
        try
        {
            fs = _stream ??= new FileStream(
                _path, FileMode.Open, FileAccess.Read, FileShare.Read);
        }
        catch (IOException)
        {
            return null;
        }
        if (offset + size > fs.Length)
        {
            return null;
        }
        var buf = new byte[size];
        fs.Seek(offset, SeekOrigin.Begin);
        int done = 0;
        while (done < buf.Length)
        {
            int n = fs.Read(buf, done, buf.Length - done);
            if (n <= 0)
            {
                return null;
            }
            done += n;
        }
        return buf;
    }

    public void Dispose() => Discard();
}
