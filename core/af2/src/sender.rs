//! AF2 Sender and Automatic Playlist Generator (protocol 2, §9).
//!
//! Handles single-pass preprocessing, per-chunk compression, RaptorQ encoding
//! (OTI-only), and continuous emission via the §9.2 standard automatic playlist:
//!
//! ```text
//! Bootstrap:  ROOT × 4 → MANIFEST META × 4 → up to 32 Manifest Symbols
//! Each Chunk i:
//!   ROOT × 1 → CHUNK i META × 2 → i's source symbols → fresh repair symbols (0.25 K)
//!   Interleaving: repeat current META every ~17 frames; repeat ROOT every ~31 frames;
//!                 interleave 1 Manifest Symbol every ~8 Chunk Symbols until manifest done
//! Next Epoch: advance to next epoch, using fresh repair ESIs across all objects.
//! ```
//!
//! Infinite generator: loops indefinitely until user stops playback.

use crate::chunk::encode_chunk;
use crate::frame::{Af2Frame, FrameType, MAX_T, MIN_T};
use crate::id::{hash, object_id, transfer_id, EntryIdInput, ROLE_CHUNK, ROLE_MANIFEST};
use crate::manifest::build_manifest;
use crate::meta::{ObjectMetaRecord, CODEC_RAW, FEC_ID_RAPTORQ};
use crate::receiver::object_meta_from_oti;
use crate::root::RootRecord;
use raptorq::{EncodingPacket, Encoder};

#[derive(Debug, thiserror::Error)]
pub enum SenderError {
    #[error("manifest error: {0}")]
    Manifest(#[from] crate::manifest::ManifestError),
    #[error("root error: {0}")]
    Root(#[from] crate::root::RootError),
    #[error("meta error: {0}")]
    Meta(#[from] crate::meta::MetaError),
    #[error("frame error: {0}")]
    Frame(#[from] crate::frame::FrameError),
    #[error("oti error: {0}")]
    Oti(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("empty content: AF2 wire v2 cannot encode a zero-byte canonical stream (receiver OTI gate rejects F=0)")]
    EmptyContent,
}

pub struct SenderConfig {
    pub symbol_size: usize,
    pub chunk_raw_size: u32,
    pub redundancy_pct: u8,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            symbol_size: 1024,
            chunk_raw_size: 8 << 20,
            redundancy_pct: 25,
        }
    }
}

struct ObjectEncoder {
    object_id: [u8; 16],
    meta_frame_bytes: Vec<u8>,
    raptorq_encoder: Encoder,
    /// Source packets cached once at construction (all blocks, emission
    /// order — byte-identical to `get_encoded_packets(0)`, which regenerates
    /// the whole O(K) block pass on every call). Per-frame emission is O(1)
    /// instead of O(K); mirrors raptorq-core's CachedBlock.
    source_packets: Vec<EncodingPacket>,
    source_symbol_count: u32,
    next_repair_esi: u32,
}

pub struct Af2Sender {
    config: SenderConfig,
    root_record: RootRecord,
    root_frame_bytes: Vec<u8>,
    manifest_encoder: ObjectEncoder,
    chunk_encoders: Vec<ObjectEncoder>,
    // Playlist emission state
    state: PlaylistState,
    global_frame_count: u64,
    since_meta_counter: usize,
    since_root_counter: usize,
    since_manifest_counter: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistState {
    BootstrapRoot(u8),
    BootstrapManifestMeta(u8),
    BootstrapManifestSymbols(u32),
    ChunkLoop {
        chunk_index: usize,
        meta_count: u8,
        symbol_index: u32,
        symbols_target: u32,
    },
}

impl Af2Sender {
    /// Create an AF2 sender from pre-built manifest and canonical stream content.
    pub fn new(
        items: Vec<(u8, String, Vec<u8>)>,
        config: SenderConfig,
    ) -> Result<Self, SenderError> {
        let t = config.symbol_size;
        // Fail fast on bad config: `to_bytes` would otherwise reject every
        // produced frame and the error would surface as an empty QR stream.
        if !(MIN_T..=MAX_T).contains(&t) || t % 8 != 0 {
            return Err(SenderError::Config(format!(
                "symbol_size {t} must be in {MIN_T}..={MAX_T} and 8-aligned"
            )));
        }
        if config.redundancy_pct > 100 {
            return Err(SenderError::Config(format!(
                "redundancy_pct {} must be 0..=100",
                config.redundancy_pct
            )));
        }
        let mut entry_refs = Vec::new();
        for (kind, path, content) in &items {
            entry_refs.push((*kind, path.as_str(), content.as_slice()));
        }
        let manifest = build_manifest(entry_refs, config.chunk_raw_size)?;
        if manifest.total_raw_size == 0 {
            // Spec gives empty content one empty chunk, but raptorq cannot
            // encode a zero-length object and the receiver OTI gate rejects
            // F=0 — reject up front with a clear error instead of panicking
            // inside the encoder.
            return Err(SenderError::EmptyContent);
        }
        let manifest_bytes = manifest.encode()?;
        let manifest_hash = hash(&manifest_bytes);
        let tid = transfer_id(&manifest_hash, config.chunk_raw_size);

        // 1. Build Manifest Object Encoder
        let manifest_enc = Encoder::with_defaults(&manifest_bytes, t as u16);
        let manifest_oti = manifest_enc.get_config().serialize();
        let manifest_meta_obj = object_meta_from_oti(&manifest_oti, 16 << 20)
            .map_err(|e| SenderError::Oti(format!("{e}")))?;
        let manifest_encoded_hash = hash(&manifest_bytes);
        let manifest_oid = object_id(
            &tid,
            ROLE_MANIFEST,
            0,
            CODEC_RAW,
            FEC_ID_RAPTORQ,
            &manifest_meta_obj.oti_bytes,
            &manifest_encoded_hash,
        );
        let manifest_meta = ObjectMetaRecord {
            role: ROLE_MANIFEST,
            transfer_id: tid,
            object_index: 0,
            codec_id: CODEC_RAW,
            fec_id: FEC_ID_RAPTORQ,
            oti: manifest_meta_obj.oti_bytes,
            raw_hash: manifest_hash,
            encoded_hash: manifest_encoded_hash,
            extensions: vec![],
        };
        let manifest_meta_frame = Af2Frame {
            frame_type: FrameType::ObjectMeta,
            object_id: manifest_oid,
            sbn: 0,
            esi: 0,
            body: manifest_meta.encode()?,
            t,
        }
        .to_bytes()?;

        let manifest_source_count = manifest_meta_obj
            .blocks
            .iter()
            .map(|b| b.num_source_symbols)
            .sum();
        let manifest_source_packets = manifest_enc.get_encoded_packets(0);
        debug_assert_eq!(manifest_source_packets.len() as u32, manifest_source_count);

        let manifest_obj_encoder = ObjectEncoder {
            object_id: manifest_oid,
            meta_frame_bytes: manifest_meta_frame,
            raptorq_encoder: manifest_enc,
            source_packets: manifest_source_packets,
            source_symbol_count: manifest_source_count,
            next_repair_esi: manifest_source_count,
        };

        // 2. Build Root Record & Frame
        let entries_inputs = manifest
            .entries
            .iter()
            .map(|e| EntryIdInput {
                kind: e.kind,
                path: &e.path,
                size: e.content_size,
                entry_hash: e.content_hash,
            })
            .collect::<Vec<_>>();
        let cid = crate::id::content_id(&entries_inputs);

        let root_record = RootRecord {
            content_id: cid,
            manifest_object_id: manifest_oid,
            manifest_hash,
            total_raw_size: manifest.total_raw_size,
            entry_count: manifest.entries.len() as u32,
            chunk_count: manifest.chunk_count,
            chunk_raw_size: manifest.chunk_raw_size,
            extensions: vec![],
        };
        let root_frame = Af2Frame {
            frame_type: FrameType::Root,
            object_id: root_record.transfer(),
            sbn: 0,
            esi: 0,
            body: root_record.encode()?,
            t,
        }
        .to_bytes()?;

        // 3. Build Canonical Content Stream & Chunk Encoders
        let mut stream = Vec::new();
        for e in &manifest.entries {
            if e.kind != crate::id::KIND_DIRECTORY {
                if let Some((_, _, content)) = items.iter().find(|(_, p, _)| p == &e.path) {
                    stream.extend_from_slice(content);
                }
            }
        }

        let mut chunk_encoders = Vec::new();
        for i in 0..manifest.chunk_count {
            let start = (i as usize) * config.chunk_raw_size as usize;
            let end = (start + config.chunk_raw_size as usize).min(stream.len());
            let raw = if start < stream.len() {
                &stream[start..end]
            } else {
                &[]
            };
            let (codec, encoded) = encode_chunk(raw);
            let chunk_enc = Encoder::with_defaults(&encoded, t as u16);
            let chunk_oti = chunk_enc.get_config().serialize();
            let chunk_meta_obj = object_meta_from_oti(&chunk_oti, 32 << 20)
                .map_err(|e| SenderError::Oti(format!("{e}")))?;
            let encoded_hash = hash(&encoded);
            let chunk_oid = object_id(
                &tid,
                ROLE_CHUNK,
                i,
                codec,
                FEC_ID_RAPTORQ,
                &chunk_meta_obj.oti_bytes,
                &encoded_hash,
            );
            let c_meta = ObjectMetaRecord {
                role: ROLE_CHUNK,
                transfer_id: tid,
                object_index: i,
                codec_id: codec,
                fec_id: FEC_ID_RAPTORQ,
                oti: chunk_meta_obj.oti_bytes,
                raw_hash: hash(raw),
                encoded_hash,
                extensions: vec![],
            };
            let chunk_meta_frame = Af2Frame {
                frame_type: FrameType::ObjectMeta,
                object_id: chunk_oid,
                sbn: 0,
                esi: 0,
                body: c_meta.encode()?,
                t,
            }
            .to_bytes()?;

            let chunk_source_count = chunk_meta_obj
                .blocks
                .iter()
                .map(|b| b.num_source_symbols)
                .sum();
            let chunk_source_packets = chunk_enc.get_encoded_packets(0);
            debug_assert_eq!(chunk_source_packets.len() as u32, chunk_source_count);

            chunk_encoders.push(ObjectEncoder {
                object_id: chunk_oid,
                meta_frame_bytes: chunk_meta_frame,
                raptorq_encoder: chunk_enc,
                source_packets: chunk_source_packets,
                source_symbol_count: chunk_source_count,
                next_repair_esi: chunk_source_count,
            });
        }

        Ok(Self {
            config,
            root_record,
            root_frame_bytes: root_frame,
            manifest_encoder: manifest_obj_encoder,
            chunk_encoders,
            state: PlaylistState::BootstrapRoot(4),
            global_frame_count: 0,
            since_meta_counter: 0,
            since_root_counter: 0,
            since_manifest_counter: 0,
        })
    }

    pub fn transfer_id(&self) -> [u8; 16] {
        self.root_record.transfer()
    }

    pub fn content_id(&self) -> [u8; 32] {
        self.root_record.content_id
    }

    /// Produce the next wire frame according to the standard automatic playlist.
    ///
    /// Errors (bad ESI range, frame serialization) propagate to the caller —
    /// they must never surface as an empty frame, which would render as a
    /// meaningless QR the receiver can never lock onto.
    pub fn next_frame(&mut self) -> Result<Vec<u8>, SenderError> {
        self.global_frame_count += 1;
        self.since_meta_counter += 1;
        self.since_root_counter += 1;
        self.since_manifest_counter += 1;

        match self.state {
            PlaylistState::BootstrapRoot(rem) => {
                if rem <= 1 {
                    self.state = PlaylistState::BootstrapManifestMeta(4);
                } else {
                    self.state = PlaylistState::BootstrapRoot(rem - 1);
                }
                self.since_root_counter = 0;
                Ok(self.root_frame_bytes.clone())
            }
            PlaylistState::BootstrapManifestMeta(rem) => {
                if rem <= 1 {
                    let target = self.manifest_encoder.source_symbol_count.min(32);
                    self.state = PlaylistState::BootstrapManifestSymbols(target);
                } else {
                    self.state = PlaylistState::BootstrapManifestMeta(rem - 1);
                }
                self.since_meta_counter = 0;
                Ok(self.manifest_encoder.meta_frame_bytes.clone())
            }
            PlaylistState::BootstrapManifestSymbols(rem) => {
                let target = self.manifest_encoder.source_symbol_count.min(32);
                let symbol_idx = target.saturating_sub(rem);
                let frame = self.get_manifest_symbol_frame(symbol_idx)?;
                if rem <= 1 {
                    if !self.chunk_encoders.is_empty() {
                        let total_target = self.chunk_target_symbols(0);
                        self.state = PlaylistState::ChunkLoop {
                            chunk_index: 0,
                            meta_count: 2,
                            symbol_index: 0,
                            symbols_target: total_target,
                        };
                    } else {
                        // Empty transfer (manifest only): cycle back
                        self.state = PlaylistState::BootstrapRoot(4);
                    }
                } else {
                    self.state = PlaylistState::BootstrapManifestSymbols(rem - 1);
                }
                Ok(frame)
            }
            PlaylistState::ChunkLoop {
                chunk_index,
                meta_count,
                symbol_index,
                symbols_target,
            } => {
                // Interleaving priorities:
                // 1. ROOT repetition every ~31 frames
                if self.since_root_counter >= 31 {
                    self.since_root_counter = 0;
                    return Ok(self.root_frame_bytes.clone());
                }

                // 2. Chunk META repetition every ~17 frames
                if self.since_meta_counter >= 17 && meta_count == 0 {
                    self.since_meta_counter = 0;
                    return Ok(self.chunk_encoders[chunk_index].meta_frame_bytes.clone());
                }

                // 3. Manifest Symbol interleave every ~8 frames
                if self.since_manifest_counter >= 8 {
                    self.since_manifest_counter = 0;
                    let m_idx = (self.global_frame_count / 8) as u32 % self.manifest_encoder.source_symbol_count.max(1);
                    return self.get_manifest_symbol_frame(m_idx);
                }

                // Normal Chunk Playlist
                if meta_count > 0 {
                    self.since_meta_counter = 0;
                    self.state = PlaylistState::ChunkLoop {
                        chunk_index,
                        meta_count: meta_count - 1,
                        symbol_index,
                        symbols_target,
                    };
                    return Ok(self.chunk_encoders[chunk_index].meta_frame_bytes.clone());
                }

                let frame = self.get_chunk_symbol_frame(chunk_index, symbol_index)?;
                let next_idx = symbol_index + 1;
                if next_idx >= symbols_target {
                    // Next chunk or next Epoch
                    let next_chunk = chunk_index + 1;
                    if next_chunk < self.chunk_encoders.len() {
                        let next_target = self.chunk_target_symbols(next_chunk);
                        self.state = PlaylistState::ChunkLoop {
                            chunk_index: next_chunk,
                            meta_count: 2,
                            symbol_index: 0,
                            symbols_target: next_target,
                        };
                    } else {
                        // Epoch finished: start next Epoch from Chunk 0 with fresh repair symbols
                        let next_target = self.chunk_target_symbols(0);
                        self.state = PlaylistState::ChunkLoop {
                            chunk_index: 0,
                            meta_count: 2,
                            symbol_index: 0,
                            symbols_target: next_target,
                        };
                    }
                } else {
                    self.state = PlaylistState::ChunkLoop {
                        chunk_index,
                        meta_count: 0,
                        symbol_index: next_idx,
                        symbols_target,
                    };
                }
                Ok(frame)
            }
        }
    }

    fn chunk_target_symbols(&self, chunk_index: usize) -> u32 {
        let k = self.chunk_encoders[chunk_index].source_symbol_count;
        let redundancy = (k as u64 * self.config.redundancy_pct as u64 / 100) as u32;
        k + redundancy.max(1)
    }

    fn get_manifest_symbol_frame(&self, symbol_index: u32) -> Result<Vec<u8>, SenderError> {
        let t = self.config.symbol_size;
        let packets = &self.manifest_encoder.source_packets;
        let pkt = &packets[symbol_index as usize % packets.len()];
        Ok(Af2Frame {
            frame_type: FrameType::Symbol,
            object_id: self.manifest_encoder.object_id,
            sbn: pkt.payload_id().source_block_number(),
            esi: pkt.payload_id().encoding_symbol_id(),
            body: pkt.data().to_vec(),
            t,
        }
        .to_bytes()?)
    }

    fn get_chunk_symbol_frame(&mut self, chunk_index: usize, symbol_index: u32) -> Result<Vec<u8>, SenderError> {
        let t = self.config.symbol_size;
        let enc = &mut self.chunk_encoders[chunk_index];
        let k = enc.source_symbol_count;
        let (sbn, esi, body) = if symbol_index < k {
            // Source symbol — O(1) from the construction-time packet cache.
            let packets = &enc.source_packets;
            let pkt = &packets[symbol_index as usize % packets.len()];
            (
                pkt.payload_id().source_block_number(),
                pkt.payload_id().encoding_symbol_id(),
                pkt.data().to_vec(),
            )
        } else {
            // Fresh repair symbol (monotonic ESI)
            let current_repair_esi = enc.next_repair_esi;
            enc.next_repair_esi = enc.next_repair_esi.saturating_add(1);
            let block_encoders = enc.raptorq_encoder.get_block_encoders();
            let block_idx = (current_repair_esi as usize) % block_encoders.len().max(1);
            let repair_pkts = block_encoders[block_idx].repair_packets(current_repair_esi, 1);
            let pkt = &repair_pkts[0];
            (
                pkt.payload_id().source_block_number(),
                pkt.payload_id().encoding_symbol_id(),
                pkt.data().to_vec(),
            )
        };

        Ok(Af2Frame {
            frame_type: FrameType::Symbol,
            object_id: enc.object_id,
            sbn,
            esi,
            body,
            t,
        }
        .to_bytes()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{KIND_FILE, KIND_UTF8_TEXT};
    use crate::receiver::{Af2Receiver, IngestEvent};

    #[test]
    fn sender_receiver_end_to_end_playlist() {
        let items = vec![
            (KIND_UTF8_TEXT, "msg.txt".to_string(), b"Hello AF2 automatic stream!".to_vec()),
            (KIND_FILE, "binary.dat".to_string(), vec![0x42u8; 10000]),
        ];
        let mut sender = Af2Sender::new(items.clone(), SenderConfig::default()).unwrap();
        let mut receiver = Af2Receiver::new();

        let mut manifest_ready = false;
        let mut chunks_received = 0;

        // Run the automatic stream until recovery completes
        for _ in 0..1000 {
            let frame_bytes = sender.next_frame().unwrap();
            let event = receiver.ingest(&frame_bytes).unwrap();
            match event {
                IngestEvent::ManifestReady => {
                    manifest_ready = true;
                }
                IngestEvent::ChunkReady { index, raw } => {
                    chunks_received += 1;
                    assert_eq!(index, 0);
                    assert!(!raw.is_empty());
                    break;
                }
                _ => {}
            }
        }

        assert!(manifest_ready, "manifest must be ready");
        assert_eq!(chunks_received, 1, "all chunks must be received");
    }
}
