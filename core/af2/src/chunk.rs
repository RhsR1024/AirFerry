//! AF2 per-chunk codec (protocol 2, §10).
//!
//! Each Raw Chunk is independently coded RAW / Zstd / Xz. The **strictly
//! smaller** invariant is dual-end: a compressed tag is only legal when the
//! encoded bytes are strictly shorter than raw; receivers reject violations.
//! Decompression output must equal the chunk's canonical raw length exactly.
//!
//! The bounded-decompression guards reuse qr-protocol's zstd window clamp
//! (`ZSTD_WINDOW_LOG_MAX=23`) and XZ memory caps via the same decoder stack —
//! one implementation, three ends.

use crate::meta::{CODEC_RAW, CODEC_XZ, CODEC_ZSTD};

pub const MAX_ZSTD_WINDOW_LOG: u32 = 23;
pub const MAX_XZ_DICT_BYTES: u64 = 32 << 20;
pub const MAX_XZ_MEM_BYTES: u64 = 128 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("chunk: encoded ({encoded}) is not strictly smaller than raw ({raw})")]
    NotStrictlySmaller { encoded: usize, raw: usize },
    #[error("chunk: decompressed size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: usize, got: usize },
    #[error("chunk: decompression failed: {0}")]
    Decompress(String),
    #[error("chunk: unknown codec id {0}")]
    UnknownCodec(u8),
}

/// Encode one chunk: try Zstd then Xz, keep a compressed tag ONLY when it is
/// strictly smaller than raw (§10.1). Three-algorithm selection with early
/// exit is a sender POLICY living in the hosts; this is the core primitive.
#[cfg(not(target_arch = "wasm32"))]
pub fn encode_chunk(raw: &[u8]) -> (u8, Vec<u8>) {
    // Empty and tiny chunks: compression can never win meaningfully; zstd on
    // empty input still emits a frame header (3 bytes) — strictly larger.
    if raw.len() < 64 {
        return (CODEC_RAW, raw.to_vec());
    }
    if let Ok(z) = qr_protocol::compress::compress(raw, 1) {
        if z.len() < raw.len() {
            return (CODEC_ZSTD, z);
        }
    }
    if let Ok(x) = qr_protocol::compress::compress_with(raw, qr_protocol::compress::COMPRESSION_XZ) {
        if x.len() < raw.len() {
            return (CODEC_XZ, x);
        }
    }
    (CODEC_RAW, raw.to_vec())
}

#[cfg(target_arch = "wasm32")]
pub fn encode_chunk(raw: &[u8]) -> (u8, Vec<u8>) {
    // The zstd/xz2 C libraries do not build for wasm32-unknown-unknown (see
    // qr-protocol's Cargo.toml), so the web sender always sends RAW. This is
    // a deliberate capability decision, not a bug: receivers stay compatible
    // because RAW needs no decoder.
    (CODEC_RAW, raw.to_vec())
}

/// Decode one chunk with full bounded verification. `expected_raw_len` is the
/// canonical chunk length (from ROOT); the output must match it exactly.
/// `chunk_raw_size` (also from ROOT) bounds the XZ declared dictionary
/// (§10.1: dict ≤ min(chunk_raw_size, 32 MiB)).
///
/// The §10.1 wire structure (single frame/stream, no trailing bytes, bounded
/// window/dict) is enforced by qr-protocol's decoder stack on every target —
/// native (libzstd/liblzma) and wasm32 (ruzstd/lzma-rs) alike.
pub fn decode_chunk(
    codec_id: u8,
    encoded: &[u8],
    expected_raw_len: usize,
    chunk_raw_size: u32,
) -> Result<Vec<u8>, ChunkError> {
    match codec_id {
        CODEC_RAW => {
            if encoded.len() != expected_raw_len {
                return Err(ChunkError::SizeMismatch {
                    expected: expected_raw_len,
                    got: encoded.len(),
                });
            }
            Ok(encoded.to_vec())
        }
        CODEC_ZSTD | CODEC_XZ => decode_compressed(codec_id, encoded, expected_raw_len, chunk_raw_size),
        other => Err(ChunkError::UnknownCodec(other)),
    }
}

/// Compressed-tag path shared by both targets: the strictly-smaller
/// invariant, then qr-protocol's §10.1-bounded decoder.
fn decode_compressed(
    codec_id: u8,
    encoded: &[u8],
    expected_raw_len: usize,
    chunk_raw_size: u32,
) -> Result<Vec<u8>, ChunkError> {
    // Reject a compressed tag that is NOT strictly smaller than the
    // canonical raw length (protocol invariant, enforced on receipt).
    if encoded.len() >= expected_raw_len {
        return Err(ChunkError::NotStrictlySmaller {
            encoded: encoded.len(),
            raw: expected_raw_len,
        });
    }
    let tag = if codec_id == CODEC_ZSTD {
        qr_protocol::compress::COMPRESSION_ZSTD
    } else {
        qr_protocol::compress::COMPRESSION_XZ
    };
    let out = qr_protocol::compress::decompress_chunk(
        encoded,
        tag,
        // Exact expected length: anything longer is a violation.
        expected_raw_len,
        chunk_raw_size,
    )
    .map_err(|e| ChunkError::Decompress(e.to_string()))?;
    if out.len() != expected_raw_len {
        return Err(ChunkError::SizeMismatch {
            expected: expected_raw_len,
            got: out.len(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            v.extend_from_slice(&state.to_le_bytes());
        }
        v.truncate(n);
        v
    }

    /// Round-trip chunk_raw_size used by the tests below.
    const CRS: u32 = 8 << 20;

    #[test]
    fn round_trip_all_codecs_and_boundaries() {
        // {empty, 1B, symbol-ish 1024, chunk-ish} × {incompressible, compressible}.
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0xAB],
            pseudo_random(1024, 1),       // incompressible
            vec![0x00; 1024],             // highly compressible
            pseudo_random(65_536, 2),     // ~ a symbol-size incompressible
            vec![b'A'; 65_536],           // compressible
        ];
        for raw in cases {
            let (codec, encoded) = encode_chunk(&raw);
            let out = decode_chunk(codec, &encoded, raw.len(), CRS).unwrap();
            assert_eq!(out, raw);
            if codec != CODEC_RAW {
                assert!(encoded.len() < raw.len(), "strictly-smaller invariant");
            }
        }
    }

    #[test]
    fn rejects_bombs_and_mislabelled_sizes() {
        // A compressed tag whose encoded size >= canonical raw length.
        let raw = vec![0u8; 4096];
        let (_, _encoded) = encode_chunk(&raw); // RAW (compression wins? zeros compress well)
        // zeros DO compress; craft the violation directly instead:
        let z = qr_protocol::compress::compress(&raw, 1).unwrap();
        assert!(z.len() < raw.len());
        // Claim canonical raw == z.len() - 1 (smaller than encoded) → violation.
        assert!(matches!(
            decode_chunk(CODEC_ZSTD, &z, z.len() - 1, CRS),
            Err(ChunkError::NotStrictlySmaller { .. })
        ));
        // Claim a longer canonical length → exact-size mismatch after decode.
        assert!(matches!(
            decode_chunk(CODEC_ZSTD, &z, raw.len() + 1, CRS),
            Err(ChunkError::SizeMismatch { .. })
        ));
        // Decompression bomb: tiny zstd of huge zeros capped at expected len.
        let bomb = qr_protocol::compress::compress(&vec![0u8; 1 << 22], 1).unwrap();
        assert!(matches!(
            decode_chunk(CODEC_ZSTD, &bomb, 1024, CRS),
            Err(ChunkError::SizeMismatch { .. }) | Err(ChunkError::Decompress(_))
        ));
    }

    #[test]
    fn rejects_xz_dict_above_chunk_raw_size() {
        // §10.1: declared dictionary ≤ min(chunk_raw_size, 32 MiB). Build a
        // stream whose dict (8 MiB at preset 6) exceeds a 1 MiB chunking.
        let raw = vec![0x5Au8; 300_000];
        let x = qr_protocol::compress::compress_with(&raw, qr_protocol::compress::COMPRESSION_XZ)
            .unwrap();
        assert!(x.len() < raw.len());
        // The encoder clamps dict ≤ input length (here 256 KiB, the largest
        // legal size ≤ 300 KB), so a 1 MiB chunking cap passes — but a
        // fabricated 128 KiB cap (below the declared dict) must be rejected.
        assert!(decode_chunk(CODEC_XZ, &x, raw.len(), 1 << 20).is_ok());
        let small_cap = 128 << 10;
        assert!(matches!(
            decode_chunk(CODEC_XZ, &x, raw.len(), small_cap),
            Err(ChunkError::Decompress(_))
        ));
    }
}
