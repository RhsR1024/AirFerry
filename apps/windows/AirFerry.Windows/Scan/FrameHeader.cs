using System.Buffers.Binary;

namespace AirFerry.Windows.Scan;

/// <summary>
/// Parsed frame header for AF2 wire frames (protocol 2).
/// Mirrors <c>ReceiverSessionManager.kt::FrameHeader</c> field-for-field;
/// wire layout lives in <c>core/af2/src/frame.rs</c> (26-byte big-endian header + payload + 4-byte CRC footer).
/// </summary>
public readonly record struct FrameHeader(
    int Magic,
    int Version,
    int FrameType,
    ulong SessionIdLo,
    ulong SessionIdHi,
    uint Sbn,
    uint Esi,
    uint TotalBlocks,
    uint TotalSymbols,
    uint SymbolSize)
{
    public const int MagicValue = 0x4146;     // 'AF' — AF2 wire magic.
    public const int ProtocolVersion = 2;
    public const int FrameTypeRoot = 1;
    public const int FrameTypeObjectMeta = 2;
    public const int FrameTypeSymbol = 3;

    /// <summary>Minimum frame size: 26-byte header + 4-byte footer (payload area >= 0).</summary>
    public const int MinFrameSize = 30;

    /// <summary>
    /// Parse + validate an AF2 frame's header. Returns <see langword="null"/> if the
    /// buffer is too short, the magic is wrong, or the version is unsupported.
    /// </summary>
    public static FrameHeader? Parse(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < MinFrameSize)
        {
            return null;
        }
        int magic = BinaryPrimitives.ReadUInt16BigEndian(bytes[..2]);
        if (magic != MagicValue)
        {
            return null;
        }
        int version = bytes[2];
        if (version != ProtocolVersion)
        {
            return null;
        }
        int frameType = bytes[3];

        ulong sessionIdHi = BinaryPrimitives.ReadUInt64BigEndian(bytes.Slice(4, 8));
        ulong sessionIdLo = BinaryPrimitives.ReadUInt64BigEndian(bytes.Slice(12, 8));

        ushort bodyLen = BinaryPrimitives.ReadUInt16BigEndian(bytes.Slice(20, 2));
        uint sbn = bytes[22];
        uint esi = ((uint)bytes[23] << 16) | ((uint)bytes[24] << 8) | (uint)bytes[25];

        return new FrameHeader(
            magic, version, frameType, sessionIdLo, sessionIdHi,
            sbn, esi, 1u, 1u, (uint)bodyLen);
    }

    /// True for ROOT / OBJECT_META frames (the two metadata frame types that
    /// carry the authoritative session identity and OTI).
    public bool IsMetaOrRoot => FrameType == FrameTypeRoot || FrameType == FrameTypeObjectMeta;
}
