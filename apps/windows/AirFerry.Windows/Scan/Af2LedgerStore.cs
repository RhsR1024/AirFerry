using System.Text;
using System.Text.Json;

namespace AirFerry.Windows.Scan;

/// <summary>
/// Crash-safe §12 resume ledger — the journal twin of <see cref="ChunkSpillStore"/>'s
/// <c>.partial</c> file. JSONL, one file per transfer (<c>af2-&lt;tid&gt;.ledger.jsonl</c>):
/// <para>
/// Line 1 (header): <c>{"v":1,"tid":…,"root":…,"crs":…}</c> — written atomically
/// (temp + flush + rename) before the first chunk commit. Each later line is
/// <c>{"c":i}</c> (chunk committed after its bytes were pwrite+fsync'd into the
/// spill) or <c>{"i":i}</c> (chunk invalidated after a re-verification failure).
/// A torn tail line fails JSON parsing and is skipped, so the journal never
/// reports more than what reached the disk.
/// </para>
/// <para>
/// Only touched from the pool's serialized ingest callback (under
/// <c>IngestLock</c>) and the recovery core that runs under the same lock.
/// </para>
/// </summary>
public sealed class Af2LedgerStore
{
    private readonly string _path;
    public string TransferIdHex { get; private set; } = "";
    public byte[] RootFrameBytes { get; private set; } = Array.Empty<byte>();
    public SortedSet<int> Completed { get; } = new();

    private Af2LedgerStore(string path)
    {
        _path = path;
    }

    public int[] CompletedIndices => Completed.ToArray();

    /// <summary>Parse (or re-parse) the journal. True when a valid header exists.</summary>
    public bool Reload()
    {
        Completed.Clear();
        if (!File.Exists(_path))
        {
            return false;
        }
        string[] lines;
        try
        {
            lines = File.ReadAllLines(_path);
        }
        catch (IOException)
        {
            return false;
        }
        bool headerSeen = false;
        foreach (string line in lines)
        {
            if (string.IsNullOrWhiteSpace(line))
            {
                continue;
            }
            JsonElement o;
            try
            {
                using JsonDocument doc = JsonDocument.Parse(line);
                o = doc.RootElement.Clone();
            }
            catch (JsonException)
            {
                continue; // torn tail line from a mid-write crash
            }
            if (!headerSeen)
            {
                if (!o.TryGetProperty("v", out _))
                {
                    continue;
                }
                TransferIdHex = o.TryGetProperty("tid", out JsonElement tid) ? tid.GetString() ?? "" : "";
                RootFrameBytes = o.TryGetProperty("root", out JsonElement root)
                    ? HexToBytes(root.GetString() ?? "") : Array.Empty<byte>();
                headerSeen = true;
                continue;
            }
            if (o.TryGetProperty("c", out JsonElement c) && c.TryGetInt32(out int ci))
            {
                Completed.Add(ci);
            }
            if (o.TryGetProperty("i", out JsonElement inv) && inv.TryGetInt32(out int ii))
            {
                Completed.Remove(ii);
            }
        }
        return headerSeen &&
            !string.IsNullOrEmpty(TransferIdHex) &&
            RootFrameBytes.Length > 0;
    }

    /// <summary>Append one commit event (after the chunk was spilled + flushed).</summary>
    public void Commit(int index)
    {
        AppendLine($"{{\"c\":{index}}}");
        Completed.Add(index);
    }

    /// <summary>Append one invalidate event (after a re-verification failure).</summary>
    public void Invalidate(int index)
    {
        AppendLine($"{{\"i\":{index}}}");
        Completed.Remove(index);
    }

    private void AppendLine(string json)
    {
        try
        {
            using var fs = new FileStream(
                _path, FileMode.Append, FileAccess.Write, FileShare.None);
            byte[] bytes = Encoding.UTF8.GetBytes(json + "\n");
            fs.Write(bytes, 0, bytes.Length);
            fs.Flush(flushToDisk: true);
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"[Af2LedgerStore] append failed: {ex.Message}");
        }
    }

    /// <summary>Delete the journal (transfer finished / relocked away / abandoned).</summary>
    public void Discard()
    {
        try { File.Delete(_path); } catch (IOException) { }
    }

    /// <summary>Resume source: the most recent journal in <paramref name="dir"/> (by mtime).</summary>
    public static Af2LedgerStore? LoadMostRecent(string dir)
    {
        try
        {
            FileInfo? latest = new DirectoryInfo(dir)
                .EnumerateFiles("*.ledger.jsonl")
                .OrderByDescending(f => f.LastWriteTimeUtc)
                .FirstOrDefault();
            if (latest is null)
            {
                return null;
            }
            var store = new Af2LedgerStore(latest.FullName);
            return store.Reload() ? store : null;
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// <summary>Create + atomically write the header for a fresh transfer's journal.</summary>
    public static Af2LedgerStore Create(
        string dir, string transferIdHex, int chunkRawSize, byte[] rootFrameBytes)
    {
        string id = string.IsNullOrEmpty(transferIdHex) ? "session" : transferIdHex;
        string path = Path.Combine(dir, $"af2-{id}.ledger.jsonl");
        try { File.Delete(path); } catch (IOException) { }
        string header = JsonSerializer.Serialize(new
        {
            v = 1,
            tid = transferIdHex,
            crs = chunkRawSize,
            root = BytesToHex(rootFrameBytes),
        });
        try
        {
            Directory.CreateDirectory(dir);
            string tmp = path + ".tmp";
            File.WriteAllText(tmp, header + "\n");
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"[Af2LedgerStore] header write failed: {ex.Message}");
        }
        return new Af2LedgerStore(path)
        {
            TransferIdHex = transferIdHex,
            RootFrameBytes = rootFrameBytes,
        };
    }

    private static string BytesToHex(byte[] b)
    {
        var sb = new StringBuilder(b.Length * 2);
        foreach (byte x in b)
        {
            sb.Append(x.ToString("x2"));
        }
        return sb.ToString();
    }

    private static byte[] HexToBytes(string s)
    {
        if (s.Length == 0 || s.Length % 2 != 0)
        {
            return Array.Empty<byte>();
        }
        var outBytes = new byte[s.Length / 2];
        for (int i = 0; i < outBytes.Length; i++)
        {
            outBytes[i] = Convert.ToByte(s.Substring(i * 2, 2), 16);
        }
        return outBytes;
    }
}
