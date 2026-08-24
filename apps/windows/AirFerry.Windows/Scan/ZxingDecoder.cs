using System.Buffers.Binary;
using System.Runtime.InteropServices;
using AirFerry.Windows.Native;

namespace AirFerry.Windows.Scan;

/// <summary>
/// Managed facade over the shared ZXing-C++ luminance decoder. Android's JNI
/// wrapper and Windows' C ABI wrapper call the same C++ implementation and use
/// the same packed multi-result layout.
/// </summary>
internal static class ZxingDecoder
{
    internal readonly record struct MultiResult(byte[] Payload, int[] Bbox);

    private const int MaxResults = 4;
    private const int MaxPackedBytes = 16 * 1024 * 1024;

    internal static uint AbiVersion() => NativeZxingBridge.AbiVersion();

    internal static List<MultiResult> DecodeMulti(
        byte[] pixels,
        int validLength,
        int width,
        int height,
        int rowStride,
        int[]? hints,
        int hintCount,
        float marginFraction)
    {
        if (validLength <= 0 || validLength > pixels.Length ||
            width <= 0 || height <= 0 || rowStride < width ||
            hintCount < 0 || hintCount > MaxResults ||
            (hintCount > 0 && (hints is null || hints.Length < hintCount * 4)))
        {
            return [];
        }

        int ok = NativeZxingBridge.DecodeMultiY(
            pixels,
            (nuint)validLength,
            width,
            height,
            rowStride,
            hints,
            (nuint)hintCount,
            marginFraction,
            out IntPtr nativeBuffer,
            out nuint nativeLength);
        if (ok == 0 || nativeBuffer == IntPtr.Zero || nativeLength == 0)
        {
            return [];
        }

        try
        {
            if (nativeLength > MaxPackedBytes)
            {
                return [];
            }
            byte[] packed = new byte[(int)nativeLength];
            Marshal.Copy(nativeBuffer, packed, 0, packed.Length);
            return ParseMulti(packed);
        }
        finally
        {
            NativeZxingBridge.BufferFree(nativeBuffer);
        }
    }

    internal static List<MultiResult> ParseMulti(ReadOnlySpan<byte> packed)
    {
        if (packed.Length < 4)
        {
            return [];
        }
        uint rawCount = BinaryPrimitives.ReadUInt32LittleEndian(packed[..4]);
        if (rawCount == 0 || rawCount > MaxResults)
        {
            return [];
        }

        var decoded = new List<MultiResult>((int)rawCount);
        int offset = 4;
        for (uint index = 0; index < rawCount; index++)
        {
            if (offset > packed.Length - 4)
            {
                return [];
            }
            uint rawPayloadLength = BinaryPrimitives.ReadUInt32LittleEndian(
                packed.Slice(offset, 4));
            offset += 4;
            if (rawPayloadLength == 0 || rawPayloadLength > int.MaxValue)
            {
                return [];
            }
            int payloadLength = (int)rawPayloadLength;
            if (offset > packed.Length - payloadLength ||
                offset + payloadLength > packed.Length - 16)
            {
                return [];
            }
            byte[] payload = packed.Slice(offset, payloadLength).ToArray();
            offset += payloadLength;
            int[] bbox =
            [
                BinaryPrimitives.ReadInt32LittleEndian(packed.Slice(offset, 4)),
                BinaryPrimitives.ReadInt32LittleEndian(packed.Slice(offset + 4, 4)),
                BinaryPrimitives.ReadInt32LittleEndian(packed.Slice(offset + 8, 4)),
                BinaryPrimitives.ReadInt32LittleEndian(packed.Slice(offset + 12, 4)),
            ];
            offset += 16;
            decoded.Add(new MultiResult(payload, bbox));
        }
        return offset == packed.Length ? decoded : [];
    }
}
