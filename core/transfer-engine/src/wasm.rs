//! WebAssembly bindings for AirFerry Protocol 2 (WASM).
//!
//! Exposes:
//! - [`SenderBuilderWasm`]: builds multi-entry AF2 streaming senders.
//! - [`SenderSessionWasm`]: high-throughput QR frame generator with preallocated
//!   scratch buffer for zero-copy Canvas rendering.
//! - [`ReceiverSessionWasm`]: AF2 stream receiver session (wraps [`crate::receiver::ReceiverSession`]).
//! - [`Sha256Wasm`]: SHA-256 helper for web workers.
//! - [`Blake3Wasm`]: BLAKE3-256 helper for single-pass hashing in web workers.
//! - [`encode_qr`]: direct QR matrix encoder.

#![cfg(all(feature = "wasm", target_arch = "wasm32"))]

use crate::receiver::ReceiverSession;
use af2::{Af2Sender, SenderConfig};
use wasm_bindgen::prelude::*;

const MAX_UI_QR_COUNT: usize = 4;
const MAX_SCRATCH_TILES: usize = 4;
const MAX_QR_SIDE_MODULES: usize = 177;
const QR_SCRATCH_BYTES: usize =
    4 + (MAX_SCRATCH_TILES * (4 + MAX_QR_SIDE_MODULES * MAX_QR_SIDE_MODULES));

#[wasm_bindgen]
pub struct Sha256Wasm {
    hasher: sha2::Sha256,
}

#[wasm_bindgen]
impl Sha256Wasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        use sha2::Digest;
        Self {
            hasher: sha2::Sha256::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        use sha2::Digest;
        self.hasher.update(bytes);
    }

    pub fn digest(&self) -> Vec<u8> {
        use sha2::Digest;
        self.hasher.clone().finalize().to_vec()
    }
}

#[wasm_bindgen]
pub struct Blake3Wasm {
    bytes: Vec<u8>,
}

#[wasm_bindgen]
impl Blake3Wasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub fn digest(&self) -> Vec<u8> {
        af2::id::hash(&self.bytes).to_vec()
    }
}

#[wasm_bindgen]
pub struct SenderBuilderWasm {
    items: Vec<(u8, String, Vec<u8>)>,
}

#[wasm_bindgen]
impl SenderBuilderWasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn add_entry(&mut self, kind: u8, path: &str, content: &[u8]) {
        self.items.push((kind, path.to_string(), content.to_vec()));
    }

    pub fn build(
        self,
        symbol_size: u32,
        chunk_raw_size: u32,
        redundancy_pct: u8,
    ) -> Result<SenderSessionWasm, JsValue> {
        let config = SenderConfig {
            symbol_size: symbol_size as usize,
            chunk_raw_size,
            redundancy_pct,
        };
        let inner = Af2Sender::new(self.items, config)
            .map_err(|e| JsValue::from_str(&format!("AF2 Sender build failed: {e}")))?;
        Ok(SenderSessionWasm {
            inner,
            qr_scratch: vec![0; QR_SCRATCH_BYTES],
            frames_emitted: 0,
            bytes_emitted: 0,
            start_time_ms: js_sys::Date::now(),
        })
    }
}

#[wasm_bindgen]
pub struct SenderSessionWasm {
    inner: Af2Sender,
    qr_scratch: Vec<u8>,
    frames_emitted: u64,
    bytes_emitted: u64,
    start_time_ms: f64,
}

#[wasm_bindgen]
impl SenderSessionWasm {
    pub fn transfer_id_hex(&self) -> String {
        hex_lower(&self.inner.transfer_id())
    }

    pub fn content_id_hex(&self) -> String {
        hex_lower(&self.inner.content_id())
    }

    pub fn stats_json(&self) -> String {
        let now = js_sys::Date::now();
        let elapsed_ms = (now - self.start_time_ms).max(1.0);
        let fps = (self.frames_emitted as f64) / (elapsed_ms / 1000.0);
        let throughput_bps = (self.bytes_emitted as f64) / (elapsed_ms / 1000.0);
        format!(
            r#"{{"frames":{},"fps":{:.1},"throughput_bps":{:.0},"bytes":{},"elapsed_ms":{:.0}}}"#,
            self.frames_emitted, fps, throughput_bps, self.bytes_emitted, elapsed_ms
        )
    }

    pub fn next_qr_scratch(&mut self, count: u32) -> Result<u32, JsValue> {
        let n = (count as usize).clamp(1, MAX_UI_QR_COUNT);
        let mut pos = 4usize;
        let mut produced = 0u32;
        for _ in 0..n {
            let frame_bytes = self
                .inner
                .next_frame()
                .map_err(|e| JsValue::from_str(&format!("AF2 frame generation failed: {e}")))?;
            self.frames_emitted += 1;
            self.bytes_emitted += frame_bytes.len() as u64;
            let matrix = qr_protocol::qr_render::encode(&frame_bytes)
                .map_err(|e| JsValue::from_str(&format!("qr encode failed: {e:?}")))?;
            let need = 4 + matrix.modules.len();
            if pos + need > self.qr_scratch.len() {
                return Err(JsValue::from_str("internal QR scratch buffer overflow"));
            }
            self.qr_scratch[pos..pos + 4].copy_from_slice(&(matrix.size as u32).to_le_bytes());
            pos += 4;
            for (dst, &dark) in self.qr_scratch[pos..pos + matrix.modules.len()]
                .iter_mut()
                .zip(matrix.modules.iter())
            {
                *dst = dark as u8;
            }
            pos += matrix.modules.len();
            produced += 1;
        }
        self.qr_scratch[..4].copy_from_slice(&produced.to_le_bytes());
        Ok(pos as u32)
    }

    /// View over the internal scratch buffer. The view is invalidated by the
    /// next `next_qr_scratch` call (same buffer is overwritten in place) —
    /// consume it immediately, never cache it across frames.
    pub fn qr_scratch_view(&self) -> js_sys::Uint8Array {
        unsafe { js_sys::Uint8Array::view(&self.qr_scratch) }
    }
}

#[wasm_bindgen]
pub struct ReceiverSessionWasm {
    inner: ReceiverSession,
}

#[wasm_bindgen]
impl ReceiverSessionWasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> ReceiverSessionWasm {
        ReceiverSessionWasm {
            inner: ReceiverSession::new(),
        }
    }

    /// Ingest a frame. Returns the unified packed `u64` ingest status word as
    /// a JavaScript `BigInt` (SPEC §16, identical to JNI and C-ABI layout).
    pub fn ingest(&mut self, frame_bytes: &[u8]) -> u64 {
        self.inner.ingest(frame_bytes)
    }

    /// True once all chunks of the transfer have been verified and staged.
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// Index of the chunk completed by the most recent ChunkReady frame (or 0).
    pub fn last_chunk_index(&self) -> u32 {
        self.inner.last_completed_chunk_index().unwrap_or(0)
    }

    /// Bytes of a completed chunk currently in memory (or empty if evicted).
    pub fn assemble_chunk(&mut self, index: u32) -> Vec<u8> {
        self.inner.assemble_chunk(index).unwrap_or_default()
    }

    /// Release chunk memory once persisted to host storage (OPFS / IndexedDB).
    pub fn forget_chunk(&mut self, index: u32) -> bool {
        self.inner.forget_chunk(index)
    }

    /// Verify a staged raw chunk against the ROOT-bound Manifest table (§11).
    pub fn verify_chunk(&self, index: u32, raw: &[u8]) -> bool {
        self.inner.verify_chunk(index, raw)
    }

    /// Run the final §13 ⑧⑨ integrity chain over the reassembled canonical stream.
    pub fn verify_final_stream(&self, stream: &[u8]) -> bool {
        self.inner.verify_final_stream(stream)
    }

    /// Restore session state from stored ROOT frame bytes + completed chunk indices.
    pub fn resume(&mut self, root_frame_bytes: &[u8], completed: &[u32]) -> bool {
        self.inner.resume(root_frame_bytes, completed)
    }

    /// Single-JSON receiver snapshot (`schema_version: 2`).
    pub fn snapshot_json(&self) -> String {
        self.inner.snapshot_json()
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[wasm_bindgen]
pub fn encode_qr(frame_bytes: &[u8], out_side: &mut [u32]) -> Result<Vec<u8>, JsValue> {
    let matrix = qr_protocol::qr_render::encode(frame_bytes)
        .map_err(|e| JsValue::from_str(&format!("qr encode failed: {e:?}")))?;
    if !out_side.is_empty() {
        out_side[0] = matrix.size as u32;
    }
    Ok(matrix.modules.into_iter().map(|b| b as u8).collect())
}
