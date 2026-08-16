//! Receiver session wrapper around the AF2 state machine.
//!
//! Provides the shared API consumed by the JNI (Android) and C-ABI (Windows)
//! native bindings.

use af2::{Af2Receiver, IngestEvent};
use std::collections::HashMap;

/// A receiver session driven by AF2.
pub struct ReceiverSession {
    inner: Af2Receiver,
    frames_seen: u64,
    received_symbols: u32,
    session_mismatch_streak: u32,
    last_chunk: Option<(u32, Vec<u8>)>,
    completed_chunks: HashMap<u32, Vec<u8>>,
}

fn escape_json(s: &str) -> String {
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

impl Default for ReceiverSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ReceiverSession {
    pub fn new() -> Self {
        Self {
            inner: Af2Receiver::new(),
            frames_seen: 0,
            received_symbols: 0,
            session_mismatch_streak: 0,
            last_chunk: None,
            completed_chunks: HashMap::new(),
        }
    }

    pub fn new_pending(_unused_sid: u128) -> Self {
        Self::new()
    }

    /// Ingest a frame. Returns packed status word via [`crate::ingest_status::pack`].
    pub fn ingest(&mut self, frame_bytes: &[u8]) -> u64 {
        self.frames_seen += 1;
        self.last_chunk = None;
        match self.inner.ingest(frame_bytes) {
            Ok(IngestEvent::RootLocked) => {
                self.session_mismatch_streak = 0;
                crate::ingest_status::pack(self.is_complete(), true, 0, self.received_symbols)
            }
            Ok(IngestEvent::RootMismatch { streak }) => {
                self.session_mismatch_streak = streak;
                crate::ingest_status::pack(self.is_complete(), false, streak, self.received_symbols)
            }
            Ok(IngestEvent::Relocked) => {
                self.session_mismatch_streak = 0;
                self.received_symbols = 0;
                self.completed_chunks.clear();
                crate::ingest_status::pack(false, true, 0, 0)
            }
            Ok(IngestEvent::MetaBound { .. }) => {
                crate::ingest_status::pack(self.is_complete(), true, 0, self.received_symbols)
            }
            Ok(IngestEvent::SymbolAccepted) => {
                self.received_symbols = self.received_symbols.saturating_add(1);
                crate::ingest_status::pack(self.is_complete(), true, 0, self.received_symbols)
            }
            Ok(IngestEvent::ManifestReady) => {
                self.received_symbols = self.received_symbols.saturating_add(1);
                crate::ingest_status::pack(self.is_complete(), true, 0, self.received_symbols)
            }
            Ok(IngestEvent::ChunkReady { index, raw }) => {
                self.received_symbols = self.received_symbols.saturating_add(1);
                self.completed_chunks.insert(index, raw.clone());
                self.last_chunk = Some((index, raw));
                crate::ingest_status::pack(self.is_complete(), true, 0, self.received_symbols)
            }
            Ok(IngestEvent::MetaRejected | IngestEvent::ChunkRejected | IngestEvent::Dropped) => {
                crate::ingest_status::pack(
                    self.is_complete(),
                    false,
                    self.session_mismatch_streak,
                    self.received_symbols,
                )
            }
            Err(_) => crate::ingest_status::INGEST_ERROR,
        }
    }

    pub fn is_complete(&self) -> bool {
        if let Some(r) = self.inner.root() {
            self.completed_chunks.len() as u32 >= r.chunk_count && r.chunk_count > 0
        } else {
            false
        }
    }

    pub fn snapshot_json(&self) -> String {
        match self.inner.root() {
            Some(r) => {
                let tid_hex: String = r.transfer().iter().map(|b| format!("{b:02x}")).collect();
                let cid_hex: String = r.content_id.iter().map(|b| format!("{b:02x}")).collect();
                let mut entries_json = String::from("[");
                if let Some(m) = self.inner.manifest() {
                    for (i, e) in m.entries.iter().enumerate() {
                        if i > 0 {
                            entries_json.push(',');
                        }
                        entries_json.push_str(&format!(
                            r#"{{"kind":{},"path":"{}","offset":{},"size":{}}}"#,
                            e.kind,
                            escape_json(&e.path),
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
                    tid_hex,
                    cid_hex,
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

    pub fn progress(&self) -> crate::Progress {
        let root = self.inner.root();
        // Symbol totals are estimates from the observed wire T: the exact
        // per-chunk K is only known while a chunk META is live, and chunk
        // compression shrinks the encoded size, so raw-size/T is an upper
        // bound that keeps decoded_fraction honest.
        let t = {
            let s = self.inner.symbol_size();
            (if s == 0 { 1024 } else { s }) as u32
        };
        let est_symbols = |chunks: u32| -> u32 {
            root.map(|r| {
                u32::try_from(
                    (u64::from(chunks) * u64::from(r.chunk_raw_size)).div_ceil(u64::from(t)),
                )
                .unwrap_or(u32::MAX)
            })
            .unwrap_or(0)
        };
        let total_symbols = root
            .map(|r| {
                u32::try_from(r.total_raw_size.div_ceil(u64::from(t))).unwrap_or(u32::MAX)
            })
            .unwrap_or(0);
        let decoded_symbols = est_symbols(self.completed_chunks.len() as u32).min(total_symbols);
        crate::Progress {
            decoded_symbols,
            total_symbols,
            symbol_size: t,
            received_symbols: self.received_symbols,
            frames_seen: self.frames_seen,
            frames_duplicate: 0,
            frames_corrupt: 0,
            decoded_blocks: self.completed_chunks.len() as u32,
            total_blocks: root.map(|r| r.chunk_count).unwrap_or(0),
            meta_confirmed: root.is_some(),
            session_mismatch_streak: self.session_mismatch_streak,
        }
    }

    pub fn assemble_chunk(&mut self, index: u32) -> Option<Vec<u8>> {
        self.completed_chunks.get(&index).cloned()
    }

    /// Reassemble all chunks in order into the full canonical stream.
    pub fn assemble_all(&self) -> Option<Vec<u8>> {
        let root = self.inner.root()?;
        if (self.completed_chunks.len() as u32) < root.chunk_count {
            return None;
        }
        let mut out = Vec::with_capacity(root.total_raw_size as usize);
        for i in 0..root.chunk_count {
            let chunk = self.completed_chunks.get(&i)?;
            out.extend_from_slice(chunk);
        }
        Some(out)
    }
}
