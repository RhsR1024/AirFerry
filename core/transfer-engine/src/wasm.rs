//! WebAssembly bindings for AirFerry Protocol 2 (WASM).
//!
//! Exposes:
//! - [`SenderBuilderWasm`]: builds multi-entry AF2 streaming senders.
//! - [`SenderSessionWasm`]: high-throughput QR frame generator with preallocated
//!   scratch buffer for zero-copy Canvas rendering.
//! - [`ReceiverSessionWasm`]: AF2 stream receiver state machine.
//! - [`Sha256Wasm`]: BLAKE3/SHA helpers for web workers.
//! - [`encode_qr`]: direct QR matrix encoder.

#![cfg(all(feature = "wasm", target_arch = "wasm32"))]

use af2::{Af2Receiver, Af2Sender, IngestEvent, SenderConfig};
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
    inner: Af2Receiver,
    last_chunk_ready: Option<(u32, Vec<u8>)>,
}

#[wasm_bindgen]
impl ReceiverSessionWasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> ReceiverSessionWasm {
        ReceiverSessionWasm {
            inner: Af2Receiver::new(),
            last_chunk_ready: None,
        }
    }

    /// Ingest a frame. Returns event code:
    /// 0=Dropped, 1=RootLocked, 2=RootMismatch, 3=Relocked, 4=MetaBound,
    /// 5=MetaRejected, 6=SymbolAccepted, 7=ManifestReady, 8=ChunkReady, 9=ChunkRejected.
    pub fn ingest(&mut self, frame_bytes: &[u8]) -> u32 {
        self.last_chunk_ready = None;
        match self.inner.ingest(frame_bytes) {
            Ok(IngestEvent::Dropped) => 0,
            Ok(IngestEvent::RootLocked) => 1,
            Ok(IngestEvent::RootMismatch { .. }) => 2,
            Ok(IngestEvent::Relocked) => 3,
            Ok(IngestEvent::MetaBound { .. }) => 4,
            Ok(IngestEvent::MetaRejected) => 5,
            Ok(IngestEvent::SymbolAccepted) => 6,
            Ok(IngestEvent::ManifestReady) => 7,
            Ok(IngestEvent::ChunkReady { index, raw }) => {
                self.last_chunk_ready = Some((index, raw));
                8
            }
            Ok(IngestEvent::ChunkRejected) => 9,
            Err(_) => 0,
        }
    }

    pub fn last_chunk_index(&self) -> u32 {
        self.last_chunk_ready.as_ref().map(|(i, _)| *i).unwrap_or(0)
    }

    pub fn last_chunk_bytes(&self) -> Vec<u8> {
        self.last_chunk_ready.as_ref().map(|(_, b)| b.clone()).unwrap_or_default()
    }

    /// Single-JSON receiver snapshot (`ReceiverSnapshotV2`).
    pub fn snapshot_json(&self) -> String {
        match self.inner.root() {
            Some(r) => {
                let mut entries_json = String::from("[");
                if let Some(m) = self.inner.manifest() {
                    for (i, e) in m.entries.iter().enumerate() {
                        if i > 0 {
                            entries_json.push(',');
                        }
                        entries_json.push_str(&format!(
                            r#"{{"kind":{},"path":"{}","offset":{},"size":{}}}"#,
                            e.kind,
                            escape_json_str(&e.path),
                            e.content_offset,
                            e.content_size
                        ));
                    }
                }
                entries_json.push(']');
                format!(
                    concat!(
                        r#"{{"schema_version":2,"meta_confirmed":true,"transfer_id_hex":"{}","#,
                        r#""content_id_hex":"{}","total_raw_size":{},"entry_count":{},"#,
                        r#""chunk_count":{},"chunk_raw_size":{},"symbol_size":{},"entries":{}}}"#
                    ),
                    hex_lower(&r.transfer()),
                    hex_lower(&r.content_id),
                    r.total_raw_size,
                    r.entry_count,
                    r.chunk_count,
                    r.chunk_raw_size,
                    self.inner.symbol_size(),
                    entries_json,
                )
            }
            None => {
                r#"{"schema_version":2,"meta_confirmed":false,"transfer_id_hex":"","content_id_hex":"","total_raw_size":0,"entry_count":0,"chunk_count":0,"chunk_raw_size":0,"symbol_size":0,"entries":[]}"#.to_string()
            }
        }
    }
}

fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
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
