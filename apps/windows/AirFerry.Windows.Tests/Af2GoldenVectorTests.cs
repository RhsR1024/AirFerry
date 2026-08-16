using System.Buffers.Binary;
using System.Text.Json;
using AirFerry.Windows.Scan;
using Xunit;
using Xunit.Abstractions;

namespace AirFerry.Windows.Tests;

/// <summary>
/// AF2 cross-platform golden-vector assertions (C# side).
/// Reads <c>core/testdata/af2/manifest.json</c> and verifies AF2 frame header parsing
/// and wire constants against the golden specification.
/// </summary>
public sealed class Af2GoldenVectorTests
{
    private readonly ITestOutputHelper _output;
    public Af2GoldenVectorTests(ITestOutputHelper output) => _output = output;

    private static string FindAf2FixtureDir()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            var candidate = Path.Combine(dir.FullName, "core", "testdata", "af2");
            if (File.Exists(Path.Combine(candidate, "manifest.json")))
            {
                return candidate;
            }
            dir = dir.Parent;
        }
        throw new FileNotFoundException(
            "core/testdata/af2/manifest.json not found above the test output directory");
    }

    private static byte[] Unhex(string hex)
    {
        return Convert.FromHexString(hex);
    }

    [Fact]
    public void Af2GoldenVectors_VerifyHeaders()
    {
        var dir = FindAf2FixtureDir();
        using var doc = JsonDocument.Parse(File.ReadAllText(Path.Combine(dir, "manifest.json")));
        var root = doc.RootElement;

        // 1. Verify ROOT frame header
        var rootFrameBytes = Unhex(root.GetProperty("root_frame_hex").GetString()!);
        var rootHeader = FrameHeader.Parse(rootFrameBytes);
        Assert.NotNull(rootHeader);
        Assert.Equal(FrameHeader.MagicValue, rootHeader!.Value.Magic);
        Assert.Equal(FrameHeader.ProtocolVersion, rootHeader.Value.Version);
        Assert.Equal(FrameHeader.FrameTypeRoot, rootHeader.Value.FrameType);
        Assert.True(rootHeader.Value.IsMetaOrRoot);

        // 2. Verify OBJECT_META frame header
        var metaFrameBytes = Unhex(root.GetProperty("object_meta_frame_hex").GetString()!);
        var metaHeader = FrameHeader.Parse(metaFrameBytes);
        Assert.NotNull(metaHeader);
        Assert.Equal(FrameHeader.FrameTypeObjectMeta, metaHeader!.Value.FrameType);
        Assert.True(metaHeader.Value.IsMetaOrRoot);

        // 3. Verify SYMBOL frame header
        var symbolFrameBytes = Unhex(root.GetProperty("symbol_frame_hex").GetString()!);
        var symbolHeader = FrameHeader.Parse(symbolFrameBytes);
        Assert.NotNull(symbolHeader);
        Assert.Equal(FrameHeader.FrameTypeSymbol, symbolHeader!.Value.FrameType);
        Assert.False(symbolHeader.Value.IsMetaOrRoot);
        Assert.Equal(1u, symbolHeader.Value.Sbn);
        Assert.Equal(42u, symbolHeader.Value.Esi);

        _output.WriteLine("AF2 C# golden headers verified successfully");
    }
}
