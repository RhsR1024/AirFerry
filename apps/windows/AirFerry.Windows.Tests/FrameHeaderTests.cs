using System.Buffers.Binary;
using AirFerry.Windows.Scan;
using Xunit;

namespace AirFerry.Windows.Tests;

/// <summary>
/// Verifies FrameHeader parsing against the authoritative AF2 wire layout in
/// <c>core/af2/src/frame.rs</c> (26-byte big-endian header). Mirrors
/// the same checks Kotlin's <c>parseHeader</c> must pass.
/// </summary>
public class FrameHeaderTests
{
    private static byte[] BuildHeader(ushort magic = FrameHeader.MagicValue, byte version = FrameHeader.ProtocolVersion,
        byte frameType = FrameHeader.FrameTypeSymbol, ulong sidHi = 0x1111222233334444, ulong sidLo = 0x5555666677778888,
        ushort bodyLen = 256, byte sbn = 2, uint esi = 0x010203)
    {
        // 26-byte header + 4-byte footer slot (min 30 bytes).
        byte[] buf = new byte[30];
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(0, 2), magic);
        buf[2] = version;
        buf[3] = frameType;
        BinaryPrimitives.WriteUInt64BigEndian(buf.AsSpan(4, 8), sidHi);
        BinaryPrimitives.WriteUInt64BigEndian(buf.AsSpan(12, 8), sidLo);
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(20, 2), bodyLen);
        buf[22] = sbn;
        buf[23] = (byte)((esi >> 16) & 0xFF);
        buf[24] = (byte)((esi >> 8) & 0xFF);
        buf[25] = (byte)(esi & 0xFF);
        return buf;
    }

    [Fact]
    public void Parse_ValidHeader_ReadsAllFields()
    {
        FrameHeader h = FrameHeader.Parse(BuildHeader()).Value;
        Assert.Equal(0x4146, h.Magic);
        Assert.Equal(2, h.Version);
        Assert.Equal(FrameHeader.FrameTypeSymbol, h.FrameType);
        Assert.Equal(0x1111222233334444ul, h.SessionIdHi);
        Assert.Equal(0x5555666677778888ul, h.SessionIdLo);
        Assert.Equal(256u, h.SymbolSize);
        Assert.Equal(2u, h.Sbn);
        Assert.Equal(0x010203u, h.Esi);
        Assert.False(h.IsMetaOrRoot);
    }

    [Fact]
    public void Parse_MetaOrRootTypes_Detected()
    {
        FrameHeader root = FrameHeader.Parse(BuildHeader(frameType: FrameHeader.FrameTypeRoot)).Value;
        Assert.True(root.IsMetaOrRoot);

        FrameHeader meta = FrameHeader.Parse(BuildHeader(frameType: FrameHeader.FrameTypeObjectMeta)).Value;
        Assert.True(meta.IsMetaOrRoot);

        FrameHeader symbol = FrameHeader.Parse(BuildHeader(frameType: FrameHeader.FrameTypeSymbol)).Value;
        Assert.False(symbol.IsMetaOrRoot);
    }

    [Fact]
    public void Parse_Rejects_BadMagic()
    {
        Assert.Null(FrameHeader.Parse(BuildHeader(magic: 0x1234)));
    }

    [Fact]
    public void Parse_Rejects_BadVersion()
    {
        Assert.Null(FrameHeader.Parse(BuildHeader(version: 99)));
    }

    [Fact]
    public void Parse_Rejects_TooShortBuffer()
    {
        Assert.Null(FrameHeader.Parse(new byte[29]));
        Assert.NotNull(FrameHeader.Parse(BuildHeader()));
    }
}
