//! AF2 Sender and Automatic Playlist Generator (protocol 2, §9).
//!
//! Handles single-pass preprocessing, lazy per-chunk compression, RaptorQ encoding
//! (OTI-only), and continuous emission via the §9.2 standard automatic playlist:
//!
//! ```text
//! Bootstrap:  ROOT × 4 → MANIFEST META × 4 → up to 32 Manifest Symbols
//! Each Chunk i:
//!   ROOT × 1 → CHUNK i META × 2 → i's source symbols → fresh repair symbols (0.25 K)
//!   Interleaving: repeat current META every ~17 frames; repeat ROOT every ~31 frames;
//!                 interleave 1 fresh Manifest Symbol every ~8 Chunk Symbols
//! Next Epoch: advance to next epoch, using fresh repair ESIs across all objects.
//! ```
//!
//! Infinite generator: loops indefinitely until user stops playback.

use crate::chunk::encode_chunk;
use crate::frame::{Af2Frame, FrameType, MAX_ESI, MAX_T, MIN_T};
use crate::id::{hash, object_id, transfer_id, EntryIdInput, ROLE_CHUNK, ROLE_MANIFEST};
use crate::manifest::{build_manifest, Manifest};
use crate::meta::{ObjectMetaRecord, CODEC_RAW, CODEC_XZ, CODEC_ZSTD, FEC_ID_RAPTORQ};
use crate::receiver::object_meta_from_oti;
use crate::root::RootRecord;
use raptorq::{Encoder, EncodingPacket};
use std::borrow::Cow;

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
    #[error("chunk {0} not staged — streamed senders require stage_chunk before playback reaches a chunk")]
    ChunkNotStaged(u32),
    #[error("empty content: AF2 wire v2 cannot encode a zero-byte canonical stream (receiver OTI gate rejects F=0)")]
    EmptyContent,
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub symbol_size: usize,
    pub chunk_raw_size: u32,
    pub redundancy_pct: u8,
}

/// A host pre-encoded chunk for the prep-time balanced policy
/// ([`crate::chunk::encode_chunk_balanced`]). Keeps compression off the
/// play path entirely; the wire format is untouched (a chunk's codec_id and
/// bytes are whatever they are).
#[derive(Debug, Clone)]
pub enum PreencodedChunk {
    /// "Compression cannot win" — skip the play-time codec attempts and
    /// stream the raw slice as RAW. Carries no bytes, so RAW-heavy media
    /// transfers pay zero duplicated memory.
    RawMarker,
    /// Pre-encoded chunk bytes with their wire codec tag. MUST be strictly
    /// smaller than the chunk's raw slice (§10.1 dual-end invariant); the
    /// build rejects violations.
    Encoded(u8, Vec<u8>),
}

/// One `(item index, offset within item, length)` slice of the canonical
/// stream — the host assembles chunk bytes from these without re-implementing
/// the NFC-path ordering rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSegment {
    pub item: u32,
    pub start: u64,
    pub len: u64,
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
    transfer_id: [u8; 16],
    root_record: RootRecord,
    root_frame_bytes: Vec<u8>,
    /// Encoded Manifest bytes (§9.3 resend cache: the whole hash pass —
    /// entry hashes, chunk hash table, content_id — is contained in these
    /// bytes, so a cached manifest rebuilds a sender without re-hashing).
    manifest_bytes: Vec<u8>,
    manifest_encoder: ObjectEncoder,
    /// Canonical content stream (single copy, entries concatenated). Empty in
    /// streamed mode — chunks arrive at play time via [`Self::stage_chunk`].
    stream: Vec<u8>,
    /// Lazily initialized chunk encoders (only built when the chunk is first
    /// reached, freed again when the playlist moves on — peak memory stays
    /// O(one chunk's encoder + packets), not O(whole file)).
    chunk_encoders: Vec<Option<ObjectEncoder>>,
    /// Per-chunk next repair ESI, surviving encoder free/rebuild (§9.2:
    /// later epochs send only repair ESIs never used before). 0 = never
    /// built (repair starts at the chunk's source symbol count).
    chunk_repair_esi: Vec<u32>,
    /// Host pre-encoded chunks (balanced sender policy). `None` chunks fall
    /// back to lazy `encode_chunk` at play time, so partially-provisioned
    /// hosts (and all native/tests paths) keep working unchanged.
    preencoded_chunks: Vec<Option<PreencodedChunk>>,
    /// ROOT-bound chunk hash table from the manifest. In streamed mode this
    /// is the only per-chunk trust anchor: staged bytes are validated
    /// against it, and the chunk META's raw_hash is taken from it (no
    /// play-time decompression needed to recover the raw hash).
    chunk_hashes: Vec<[u8; 32]>,
    /// Streamed mode: the canonical stream never materializes here; each
    /// chunk must be staged via [`Self::stage_chunk`] before the playlist
    /// reaches it, and is dropped again when the window moves on.
    staged_mode: bool,
    staged_chunks: Vec<Option<(u8, Vec<u8>)>>,
    /// 1-based broadcast epoch. Epoch 1 sends each chunk's source symbols
    /// once; epoch ≥ 2 sends only fresh repair symbols.
    epoch: u32,
    // Playlist emission state
    state: PlaylistState,
    global_frame_count: u64,
    since_meta_counter: usize,
    since_root_counter: usize,
    since_manifest_counter: usize,
    /// Every 8th manifest interleave slot carries the MANIFEST META frame
    /// instead of a repair symbol, so a late joiner can still build the
    /// manifest decoder (bootstrap alone is not recurring).
    manifest_interleave_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistState {
    BootstrapRoot(u8),
    BootstrapManifestMeta(u8),
    BootstrapManifestSymbols(u32),
    ChunkLoop {
        chunk_index: usize,
        root_sent: bool,
        meta_count: u8,
        symbol_index: u32,
        symbols_target: u32,
    },
}

impl Af2Sender {
    /// Create an AF2 sender from pre-built items. Preprocessing (manifest build,
    /// entry hashes, content id, chunk table) runs in a single pass; per-chunk
    /// RaptorQ encoders are constructed lazily on first playback.
    pub fn new(
        items: Vec<(u8, String, Vec<u8>)>,
        config: SenderConfig,
    ) -> Result<Self, SenderError> {
        Self::new_with_preencoded(items, config, Vec::new())
    }

    /// [`Self::new`] plus host pre-encoded chunks (balanced sender policy):
    /// every provisioned chunk skips play-time compression; the rest behave
    /// exactly like [`Self::new`].
    pub fn new_with_preencoded(
        items: Vec<(u8, String, Vec<u8>)>,
        config: SenderConfig,
        preencoded: Vec<(u32, PreencodedChunk)>,
    ) -> Result<Self, SenderError> {
        let mut entry_refs = Vec::new();
        for (kind, path, content) in &items {
            entry_refs.push((*kind, path.as_str(), content.as_slice()));
        }
        let manifest = build_manifest(entry_refs, config.chunk_raw_size)?;
        Self::from_manifest_with_preencoded(manifest, items, config, preencoded)
    }

    /// Build a sender from a **pre-built, already-trusted Manifest** — the
    /// §9.3 resend-cache path. Every hash pass (per-entry BLAKE3, chunk hash
    /// table, content_id derivation) is skipped; everything else (manifest
    /// encode, ROOT/META frames, lazy chunk encoders, canonical stream
    /// assembly) is identical to [`Self::new`], so the emitted frame stream
    /// is byte-for-byte the same as a full rebuild from the same items.
    ///
    /// The cache is advisory (SPEC §10.2): validity is keyed by the caller's
    /// `(path, size, mtime)` fingerprint, and a stale manifest produces a
    /// transfer whose receivers fail §13 verification — never a wire crash.
    /// Hosts MUST fall back to [`Self::new`] when the cache is unavailable
    /// or untrusted.
    pub fn from_manifest(
        manifest: Manifest,
        items: Vec<(u8, String, Vec<u8>)>,
        config: SenderConfig,
    ) -> Result<Self, SenderError> {
        Self::from_manifest_with_preencoded(manifest, items, config, Vec::new())
    }

    /// [`Self::from_manifest`] plus host pre-encoded chunks. Validation is
    /// fail-closed on the §10.1 invariant: an `Encoded` chunk must carry a
    /// Zstd/Xz tag and be strictly smaller than its canonical raw slice —
    /// a host bug can never put an illegal wire object on the air.
    pub fn from_manifest_with_preencoded(
        manifest: Manifest,
        items: Vec<(u8, String, Vec<u8>)>,
        config: SenderConfig,
        preencoded: Vec<(u32, PreencodedChunk)>,
    ) -> Result<Self, SenderError> {
        Self::build_from_manifest(manifest, Some(items), config, preencoded)
    }

    /// Bounded-memory streamed construction: the manifest already carries the
    /// complete entry table and chunk hash table, so the sender never holds
    /// the canonical stream. Each chunk must be provided at play time via
    /// [`Self::stage_chunk`] (validated against the manifest's chunk hashes);
    /// [`Self::next_frame`] fails closed with [`SenderError::ChunkNotStaged`]
    /// when the playlist reaches an unstaged chunk. Re-staging a chunk in a
    /// later epoch MUST supply byte-identical encoded bytes — the chunk META's
    /// encoded_hash (and thus its object_id) is derived from them, and a
    /// differing object_id would make the receiver drop every symbol.
    ///
    /// Use [`crate::manifest::build_manifest_from_hashes`] (or the §9.3
    /// resend cache) to build `manifest` without ever holding content bytes.
    pub fn from_manifest_streamed(
        manifest: Manifest,
        config: SenderConfig,
    ) -> Result<Self, SenderError> {
        Self::build_from_manifest(manifest, None, config, Vec::new())
    }

    fn build_from_manifest(
        manifest: Manifest,
        items: Option<Vec<(u8, String, Vec<u8>)>>,
        config: SenderConfig,
        preencoded: Vec<(u32, PreencodedChunk)>,
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
        // The manifest is the authoritative chunking (its chunk hash table was
        // built with ITS chunk_raw_size); a caller-supplied mismatch would
        // desync stream slicing and the transfer_id derivation.
        if config.chunk_raw_size != manifest.chunk_raw_size {
            return Err(SenderError::Config(format!(
                "chunk_raw_size {} does not match manifest's {}",
                config.chunk_raw_size, manifest.chunk_raw_size
            )));
        }
        if manifest.total_raw_size == 0 {
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

        // 3. Assemble single Canonical Content Stream (non-directory entries only).
        // Manifest entry paths are NFC-normalized (build_manifest), while the
        // caller's items may still carry NFD names (macOS) — match through an
        // NFC-normalized index, never a raw path comparison (a silent miss here
        // would desync every chunk hash from the manifest table).
        //
        // Streamed mode (items == None) skips the stream entirely: chunks are
        // staged at play time and validated against the manifest chunk table.
        let staged_mode = items.is_none();
        let mut stream = Vec::new();
        if let Some(items) = items {
            use std::collections::HashMap;
            use unicode_normalization::UnicodeNormalization;
            let mut item_index: HashMap<String, usize> = HashMap::with_capacity(items.len());
            for (i, (_, path, _)) in items.iter().enumerate() {
                // Duplicate normalized keys correspond to transfers build_manifest
                // already rejected above, so overwrite is unreachable.
                item_index.insert(path.nfc().collect::<String>(), i);
            }
            stream.reserve(manifest.total_raw_size as usize);
            for e in &manifest.entries {
                if e.kind != crate::id::KIND_DIRECTORY {
                    if let Some(&i) = item_index.get(&e.path) {
                        stream.extend_from_slice(&items[i].2);
                    }
                }
            }
            // The manifest is authoritative for total_raw_size; a silently-skipped
            // item (e.g. a stale §9.3 cached manifest replayed against a mutated
            // selection) must fail the build loudly instead of producing tail
            // chunks sliced from `&[]` that only die at play time.
            if stream.len() as u64 != manifest.total_raw_size {
                return Err(SenderError::Config(format!(
                    "assembled stream length {} != manifest total_raw_size {} — item set inconsistent with manifest",
                    stream.len(),
                    manifest.total_raw_size
                )));
            }
        }

        let chunk_count = manifest.chunk_count as usize;
        let mut chunk_encoders = Vec::with_capacity(chunk_count);
        chunk_encoders.resize_with(chunk_count, || None);
        let chunk_repair_esi = vec![0u32; chunk_count];

        // Validate + index the host pre-encoded chunks against the assembled
        // canonical stream (the authoritative chunking lives in the manifest).
        let mut preencoded_chunks: Vec<Option<PreencodedChunk>> =
            (0..chunk_count).map(|_| None).collect();
        for (index, pc) in preencoded {
            let idx = index as usize;
            if idx >= chunk_count {
                return Err(SenderError::Config(format!(
                    "preencoded chunk index {index} out of range (chunk_count {chunk_count})"
                )));
            }
            if preencoded_chunks[idx].is_some() {
                return Err(SenderError::Config(format!(
                    "preencoded chunk {index} provided twice"
                )));
            }
            if let PreencodedChunk::Encoded(codec, bytes) = &pc {
                if *codec != CODEC_ZSTD && *codec != CODEC_XZ {
                    return Err(SenderError::Config(format!(
                        "preencoded chunk {index} codec {codec} must be Zstd/Xz (send RawMarker for RAW)"
                    )));
                }
                let start = idx as u64 * u64::from(manifest.chunk_raw_size);
                let raw_len = (manifest.total_raw_size.saturating_sub(start))
                    .min(u64::from(manifest.chunk_raw_size))
                    as usize;
                if bytes.len() >= raw_len {
                    return Err(SenderError::Config(format!(
                        "preencoded chunk {index} violates strictly-smaller ({} >= {raw_len})",
                        bytes.len()
                    )));
                }
            }
            preencoded_chunks[idx] = Some(pc);
        }

        Ok(Self {
            config,
            transfer_id: tid,
            root_record,
            root_frame_bytes: root_frame,
            manifest_bytes,
            manifest_encoder: manifest_obj_encoder,
            stream,
            chunk_encoders,
            chunk_repair_esi,
            preencoded_chunks,
            chunk_hashes: manifest.chunk_hashes,
            staged_mode,
            staged_chunks: vec![None; chunk_count],
            epoch: 1,
            state: PlaylistState::BootstrapRoot(4),
            global_frame_count: 0,
            since_meta_counter: 0,
            since_root_counter: 0,
            since_manifest_counter: 0,
            manifest_interleave_count: 0,
        })
    }

    pub fn transfer_id(&self) -> [u8; 16] {
        self.transfer_id
    }

    pub fn content_id(&self) -> [u8; 32] {
        self.root_record.content_id
    }

    /// Encoded Manifest bytes — the §9.3 resend-cache payload. Deterministic:
    /// the same items + chunk_raw_size always produce the same bytes, so a
    /// cached copy can be handed back to [`Self::from_manifest`] to rebuild
    /// this sender without re-running the hash pass.
    pub fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }

    /// Playlist position: `Some(chunk)` once playback is inside a chunk's
    /// window, `None` during bootstrap. Streamed hosts use this to prefetch
    /// the upcoming chunk (stage current + next).
    pub fn current_chunk_index(&self) -> Option<u32> {
        match self.state {
            PlaylistState::ChunkLoop { chunk_index, .. } => Some(chunk_index as u32),
            _ => None,
        }
    }

    /// 1-based broadcast epoch. Together with [`Self::current_chunk_index`]
    /// this uniquely identifies the active window — a single-chunk transfer
    /// keeps the same chunk index across every epoch wrap, so hosts keying
    /// per-window prefetch state on the index alone go stale.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Provide one chunk's ENCODED bytes to a streamed sender
    /// ([`Self::from_manifest_streamed`]). Fail-closed against the manifest
    /// chunk table: RAW bytes must be exactly the canonical slice length and
    /// hash to the table entry; Zstd/Xz bytes must be strictly smaller AND
    /// bounded-decompress to bytes hashing to the table entry. The staged
    /// bytes are consumed when the playlist reaches the chunk and dropped
    /// when the window moves on — later epochs must re-stage byte-identical
    /// bytes (deterministic re-encode) to keep the object_id stable.
    pub fn stage_chunk(
        &mut self,
        index: u32,
        codec_id: u8,
        bytes: Vec<u8>,
    ) -> Result<(), SenderError> {
        if !self.staged_mode {
            return Err(SenderError::Config(
                "stage_chunk is only valid for streamed senders (from_manifest_streamed)".into(),
            ));
        }
        let idx = index as usize;
        if idx >= self.chunk_encoders.len() {
            return Err(SenderError::Config(format!(
                "stage_chunk index {index} out of range (chunk_count {})",
                self.chunk_encoders.len()
            )));
        }
        if self.staged_chunks[idx].is_some() {
            // Already armed for this chunk's next need (the host prefetches
            // the next epoch's copy while the current window is still live —
            // see retire_chunk, which keeps the slot).
            return Ok(());
        }
        let start = u64::from(idx as u32) * u64::from(self.config.chunk_raw_size);
        let canonical_len = self
            .root_record
            .total_raw_size
            .saturating_sub(start)
            .min(u64::from(self.config.chunk_raw_size)) as usize;
        let raw_hash = match codec_id {
            CODEC_RAW => {
                if bytes.len() != canonical_len {
                    return Err(SenderError::Config(format!(
                        "staged RAW chunk {index} length {} != canonical {canonical_len}",
                        bytes.len()
                    )));
                }
                hash(&bytes)
            }
            CODEC_ZSTD | CODEC_XZ => {
                // §10.1 dual-end invariant.
                if bytes.len() >= canonical_len {
                    return Err(SenderError::Config(format!(
                        "staged chunk {index} violates strictly-smaller ({} >= {canonical_len})",
                        bytes.len()
                    )));
                }
                let raw = crate::chunk::decode_chunk(
                    codec_id,
                    &bytes,
                    canonical_len,
                    self.config.chunk_raw_size,
                )
                .map_err(|e| SenderError::Config(format!("staged chunk {index} decode: {e}")))?;
                hash(&raw)
            }
            other => {
                return Err(SenderError::Config(format!(
                    "staged chunk {index} codec {other} must be RAW/Zstd/Xz"
                )));
            }
        };
        if raw_hash != self.chunk_hashes[idx] {
            return Err(SenderError::Config(format!(
                "staged chunk {index} hash mismatch — bytes disagree with the manifest chunk table"
            )));
        }
        self.staged_chunks[idx] = Some((codec_id, bytes));
        Ok(())
    }

    /// Produce the next wire frame according to the standard automatic playlist.
    ///
    /// Transactional: a failed call (e.g. [`SenderError::ChunkNotStaged`] in
    /// streamed mode) leaves EVERY piece of emission state untouched — the
    /// interleave counters, the per-chunk repair ESI cursor and the playlist
    /// position. Hosts may therefore "stage-and-retry" on error and get the
    /// exact frame sequence an always-staged sender would have produced;
    /// without the rollback every failed attempt would silently consume one
    /// interleave beat and desync the schedule.
    pub fn next_frame(&mut self) -> Result<Vec<u8>, SenderError> {
        let counters = (
            self.global_frame_count,
            self.since_meta_counter,
            self.since_root_counter,
            self.since_manifest_counter,
            self.manifest_interleave_count,
        );
        let cursor_chunk = match self.state {
            PlaylistState::ChunkLoop { chunk_index, .. } => Some(chunk_index),
            _ => None,
        };
        let cursor_checkpoint = cursor_chunk.map(|i| self.chunk_repair_esi[i]);
        self.global_frame_count += 1;
        self.since_meta_counter += 1;
        self.since_root_counter += 1;
        self.since_manifest_counter += 1;
        let result = self.step();
        if result.is_err() {
            (
                self.global_frame_count,
                self.since_meta_counter,
                self.since_root_counter,
                self.since_manifest_counter,
                self.manifest_interleave_count,
            ) = counters;
            if let (Some(i), Some(cursor)) = (cursor_chunk, cursor_checkpoint) {
                self.chunk_repair_esi[i] = cursor;
                // The LIVE encoder's internal cursor is the authoritative
                // value while its window is active (the persisted array only
                // feeds encoder rebuilds). Without restoring it too, a failed
                // advance AFTER a symbol fetch permanently burned repair
                // ordinals — the retry resumed past ESIs that were never
                // displayed, silently shrinking the receiver's fountain pool.
                if let Some(enc) = &mut self.chunk_encoders[i] {
                    enc.next_repair_esi = cursor;
                }
            }
        }
        result
    }

    fn step(&mut self) -> Result<Vec<u8>, SenderError> {
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
                        self.ensure_chunk_encoder(0)?;
                        let total_target = self.chunk_target_symbols(0);
                        self.state = PlaylistState::ChunkLoop {
                            chunk_index: 0,
                            root_sent: false,
                            meta_count: 2,
                            symbol_index: 0,
                            symbols_target: total_target,
                        };
                    } else {
                        // Manifest-only transfer: cycle back
                        self.state = PlaylistState::BootstrapRoot(4);
                    }
                } else {
                    self.state = PlaylistState::BootstrapManifestSymbols(rem - 1);
                }
                Ok(frame)
            }
            PlaylistState::ChunkLoop {
                chunk_index,
                root_sent,
                meta_count,
                symbol_index,
                symbols_target,
            } => {
                // Interleaving priorities (§9.2):
                // 1. ROOT repetition every ~31 frames
                if self.since_root_counter >= 31 {
                    self.since_root_counter = 0;
                    return Ok(self.root_frame_bytes.clone());
                }

                // 2. Chunk META repetition every ~17 frames
                if self.since_meta_counter >= 17 && meta_count == 0 {
                    self.since_meta_counter = 0;
                    self.ensure_chunk_encoder(chunk_index)?;
                    let enc = self.chunk_encoders[chunk_index].as_ref().unwrap();
                    return Ok(enc.meta_frame_bytes.clone());
                }

                // 3. Manifest interleave every ~8 frames: every 8th slot
                // carries the MANIFEST META frame (late-joiner bootstrap —
                // the bootstrap phase never recurs), the rest carry fresh
                // repair symbols.
                if self.since_manifest_counter >= 8 {
                    self.since_manifest_counter = 0;
                    self.manifest_interleave_count = self.manifest_interleave_count.wrapping_add(1);
                    if self.manifest_interleave_count % 8 == 1 {
                        return Ok(self.manifest_encoder.meta_frame_bytes.clone());
                    }
                    if let Some(frame) = self.get_manifest_interleave_frame()? {
                        return Ok(frame);
                    }
                    // Manifest repair ESI exhausted (§9.1: stop at 2^24, never
                    // re-issue) — fall through to the chunk symbol flow.
                }

                // Normal Chunk Playlist: §9.2 "ROOT ×1 → CHUNK i META × 2 → symbols"
                if !root_sent {
                    self.since_root_counter = 0;
                    self.state = PlaylistState::ChunkLoop {
                        chunk_index,
                        root_sent: true,
                        meta_count,
                        symbol_index,
                        symbols_target,
                    };
                    return Ok(self.root_frame_bytes.clone());
                }

                if meta_count > 0 {
                    self.since_meta_counter = 0;
                    self.ensure_chunk_encoder(chunk_index)?;
                    let enc = self.chunk_encoders[chunk_index].as_ref().unwrap();
                    let meta_bytes = enc.meta_frame_bytes.clone();
                    self.state = PlaylistState::ChunkLoop {
                        chunk_index,
                        root_sent: true,
                        meta_count: meta_count - 1,
                        symbol_index,
                        symbols_target,
                    };
                    return Ok(meta_bytes);
                }

                let frame = match self.get_chunk_symbol_frame(chunk_index, symbol_index)? {
                    Some(f) => f,
                    None => {
                        // §9.1: this chunk's repair ESI space is exhausted —
                        // no fresh symbols left. Advance exactly as if its
                        // target had been reached; the returned frame is the
                        // next playlist step's leading ROOT (§9.2).
                        self.advance_past_chunk(chunk_index)?;
                        self.since_root_counter = 0;
                        if let PlaylistState::ChunkLoop { root_sent, .. } = &mut self.state {
                            *root_sent = true;
                        }
                        return Ok(self.root_frame_bytes.clone());
                    }
                };
                let next_idx = symbol_index + 1;
                if next_idx >= symbols_target {
                    self.advance_past_chunk(chunk_index)?;
                } else {
                    self.state = PlaylistState::ChunkLoop {
                        chunk_index,
                        root_sent: true,
                        meta_count: 0,
                        symbol_index: next_idx,
                        symbols_target,
                    };
                }
                Ok(frame)
            }
        }
    }

    /// Chunk `chunk_index` is finished (symbol target reached, or its repair
    /// ESI space exhausted per §9.1): move the playlist to the next chunk —
    /// or, on the last chunk, restart the epoch from Chunk 0. Later epochs
    /// skip the source-symbol pass and send only fresh repair ESIs (§9.2),
    /// resuming from the persisted per-chunk ESI.
    ///
    /// The next chunk's encoder is built BEFORE the current one is freed and
    /// the state moves: in streamed mode an unstaged next chunk fails here
    /// with [`SenderError::ChunkNotStaged`] while the playlist state stays
    /// intact, so the host can stage the chunk and resume exactly where it
    /// stopped.
    fn advance_past_chunk(&mut self, chunk_index: usize) -> Result<(), SenderError> {
        let next_chunk = chunk_index + 1;
        if next_chunk < self.chunk_encoders.len() {
            self.ensure_chunk_encoder(next_chunk)?;
            let k = self.chunk_encoders[next_chunk]
                .as_ref()
                .unwrap()
                .source_symbol_count;
            let start = if self.epoch == 1 { 0 } else { k };
            let next_target = self.chunk_target_symbols(next_chunk);
            self.retire_chunk(chunk_index);
            self.state = PlaylistState::ChunkLoop {
                chunk_index: next_chunk,
                root_sent: false,
                meta_count: 2,
                symbol_index: start,
                symbols_target: next_target,
            };
        } else {
            // Epoch finished: restart from Chunk 0 with fresh repair symbols.
            // chunk_target_symbols depends on the epoch, so pass next_epoch
            // explicitly instead of mutating self.epoch before the (fallible)
            // staging is known to succeed.
            let next_epoch = self.epoch + 1;
            self.ensure_chunk_encoder(0)?;
            let k = self.chunk_encoders[0].as_ref().unwrap().source_symbol_count;
            // Epoch 1 already sent every source symbol once; epoch ≥ 2 sends
            // only repair symbols the receiver has never seen (§9.2).
            let start = if next_epoch == 1 { 0 } else { k };
            let next_target = self.chunk_target_symbols_at(0, next_epoch);
            self.retire_chunk(chunk_index);
            self.epoch = next_epoch;
            self.state = PlaylistState::ChunkLoop {
                chunk_index: 0,
                root_sent: false,
                meta_count: 2,
                symbol_index: start,
                symbols_target: next_target,
            };
        }
        Ok(())
    }

    /// Free the finished chunk's encoder + cached packets — rebuilt
    /// deterministically (same inputs ⇒ same OTI / object_id) when the
    /// playlist returns to it, keeping peak memory at O(one chunk).
    ///
    /// The STAGED slot is deliberately kept: hosts prefetch the next epoch's
    /// copy of a chunk while its current window is still live (essential for
    /// single-chunk transfers, whose EVERY window boundary is an epoch wrap —
    /// without the kept slot each wrap would stall on ChunkNotStaged). Memory
    /// stays bounded: the slot is empty whenever the chunk's window is active
    /// (the encoder build consumes it), so at most one prefetched chunk is
    /// held beyond the live one.
    fn retire_chunk(&mut self, chunk_index: usize) {
        self.chunk_encoders[chunk_index] = None;
    }

    /// Lazily construct the RaptorQ encoder for chunk `index` if not built yet.
    fn ensure_chunk_encoder(&mut self, index: usize) -> Result<(), SenderError> {
        if self.chunk_encoders[index].is_some() {
            return Ok(());
        }
        let t = self.config.symbol_size;
        // Streamed mode: the staged bytes ARE the encoded chunk (already
        // validated against the manifest chunk table at stage time). The
        // canonical raw never materializes here; the META's raw_hash comes
        // straight from the table (no play-time decompression).
        let (codec, encoded, raw_hash): (u8, Cow<'_, [u8]>, [u8; 32]) = if self.staged_mode {
            let (codec, bytes) = self
                .staged_chunks[index]
                .take()
                .ok_or(SenderError::ChunkNotStaged(index as u32))?;
            (codec, Cow::Owned(bytes), self.chunk_hashes[index])
        } else {
            let start64 = u64::from(index as u32) * u64::from(self.config.chunk_raw_size);
            let end64 = (start64 + u64::from(self.config.chunk_raw_size)).min(self.stream.len() as u64);
            let raw = if start64 < self.stream.len() as u64 {
                &self.stream[start64 as usize..end64 as usize]
            } else {
                &[]
            };
            let (codec, encoded): (u8, Cow<'_, [u8]>) = match &self.preencoded_chunks[index] {
                // Balanced policy provisioned this chunk at prep time — no codec
                // runs on the play path (the rAF/QR loop must never block on
                // compression).
                Some(PreencodedChunk::RawMarker) => (CODEC_RAW, Cow::Borrowed(raw)),
                Some(PreencodedChunk::Encoded(c, bytes)) => (*c, Cow::Borrowed(bytes)),
                None => {
                    let (c, e) = encode_chunk(raw);
                    (c, Cow::Owned(e))
                }
            };
            (codec, encoded, hash(raw))
        };
        let chunk_enc = Encoder::with_defaults(encoded.as_ref(), t as u16);
        let chunk_oti = chunk_enc.get_config().serialize();
        let chunk_meta_obj = object_meta_from_oti(&chunk_oti, 32 << 20)
            .map_err(|e| SenderError::Oti(format!("{e}")))?;
        let encoded_hash = hash(encoded.as_ref());
        let chunk_oid = object_id(
            &self.transfer_id,
            ROLE_CHUNK,
            index as u32,
            codec,
            FEC_ID_RAPTORQ,
            &chunk_meta_obj.oti_bytes,
            &encoded_hash,
        );
        let c_meta = ObjectMetaRecord {
            role: ROLE_CHUNK,
            transfer_id: self.transfer_id,
            object_index: index as u32,
            codec_id: codec,
            fec_id: FEC_ID_RAPTORQ,
            oti: chunk_meta_obj.oti_bytes,
            raw_hash,
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

        self.chunk_encoders[index] = Some(ObjectEncoder {
            object_id: chunk_oid,
            meta_frame_bytes: chunk_meta_frame,
            raptorq_encoder: chunk_enc,
            source_packets: chunk_source_packets,
            source_symbol_count: chunk_source_count,
            // Resume the persisted never-repeated repair cursor; 0 marks a
            // chunk never encoded before (repair starts at its source count).
            next_repair_esi: match self.chunk_repair_esi[index] {
                0 => chunk_source_count,
                saved => saved,
            },
        });
        Ok(())
    }

    fn chunk_target_symbols(&self, chunk_index: usize) -> u32 {
        self.chunk_target_symbols_at(chunk_index, self.epoch)
    }

    fn chunk_target_symbols_at(&self, chunk_index: usize, epoch: u32) -> u32 {
        let k = self.chunk_encoders[chunk_index]
            .as_ref()
            .map(|e| e.source_symbol_count)
            .unwrap_or(1);
        let redundancy = (k as u64 * self.config.redundancy_pct as u64 / 100).max(1) as u32;
        if epoch == 1 {
            // Epoch 1: the source-symbol pass plus the configured redundancy.
            k + redundancy
        } else {
            // Epoch ≥ 2 sends repair-only (start = k). The receiver holds
            // exactly ONE chunk decoder and drops it (with every collected
            // symbol — zero cache, §11 resource policy) the moment the next
            // chunk's META arrives, so a chunk that missed more than the
            // redundancy budget inside its epoch-1 window starts every later
            // window FROM SCRATCH. A window carrying only `redundancy` fresh
            // symbols could therefore never reach K again — a permanently
            // starved chunk and an unfinishable transfer (observed as
            // received/total climbing past 100% forever). Every later epoch
            // must carry a full K-symbol budget of fresh repair ESI plus the
            // same slack as epoch 1, so one watched window suffices to decode.
            k.saturating_mul(2) + redundancy
        }
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

    /// Produce a fresh repair symbol for the Manifest (§9.1 monotonic ESI,
    /// never repeated). Returns `None` once the ESI space is exhausted
    /// (2^24 − 1): per §9.1 the sender stops rather than re-issuing.
    fn get_manifest_interleave_frame(&mut self) -> Result<Option<Vec<u8>>, SenderError> {
        let t = self.config.symbol_size;
        let current_esi = self.manifest_encoder.next_repair_esi;
        if current_esi >= MAX_ESI {
            return Ok(None);
        }
        self.manifest_encoder.next_repair_esi += 1;
        let block_encoders = self.manifest_encoder.raptorq_encoder.get_block_encoders();
        let block_idx = (current_esi as usize) % block_encoders.len().max(1);
        let repair_pkts = block_encoders[block_idx].repair_packets(current_esi, 1);
        let pkt = &repair_pkts[0];
        Ok(Some(
            Af2Frame {
                frame_type: FrameType::Symbol,
                object_id: self.manifest_encoder.object_id,
                sbn: pkt.payload_id().source_block_number(),
                esi: pkt.payload_id().encoding_symbol_id(),
                body: pkt.data().to_vec(),
                t,
            }
            .to_bytes()?,
        ))
    }

    /// Produce the chunk's `symbol_index`-th symbol frame. Source symbols are
    /// replayable; repair symbols use a monotonically advancing ESI that is
    /// persisted across encoder free/rebuild so later epochs never re-issue a
    /// repair ESI (§9.1/§9.2). Returns `None` when the repair ESI space is
    /// exhausted — the caller must treat the chunk as finished.
    fn get_chunk_symbol_frame(
        &mut self,
        chunk_index: usize,
        symbol_index: u32,
    ) -> Result<Option<Vec<u8>>, SenderError> {
        self.ensure_chunk_encoder(chunk_index)?;
        let t = self.config.symbol_size;
        let (object_id, sbn, esi, body, esi_cursor) = {
            let enc = self.chunk_encoders[chunk_index].as_mut().unwrap();
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
                // Fresh repair symbol (monotonic ESI, never repeated, stop at 2^24 §9.1)
                let current_repair_esi = enc.next_repair_esi;
                if current_repair_esi >= MAX_ESI {
                    return Ok(None);
                }
                enc.next_repair_esi += 1;
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
            (enc.object_id, sbn, esi, body, enc.next_repair_esi)
        };
        // Persist the cursor after the encoder borrow ends — the encoder may
        // be freed at any moment by the playlist and must rebuild with the
        // same never-repeated ESI sequence.
        self.chunk_repair_esi[chunk_index] = esi_cursor;

        Ok(Some(
            Af2Frame {
                frame_type: FrameType::Symbol,
                object_id,
                sbn,
                esi,
                body,
                t,
            }
            .to_bytes()?,
        ))
    }
}

/// Canonical-stream chunk layout WITHOUT reading or hashing any content:
/// NFC-normalize paths, drop directories, sort by path bytes — exactly
/// [`build_manifest`]'s ordering — then cut the cumulative stream into
/// `chunk_raw_size` chunks. Hosts use this during prep to assemble each
/// chunk's bytes for pre-encoding; `from_manifest_with_preencoded` validates
/// the resulting encodings against the same slices, so a layout mismatch can
/// never reach the wire.
pub fn plan_chunks(
    metas: &[(u8, String, u64)],
    chunk_raw_size: u32,
) -> Result<Vec<Vec<ChunkSegment>>, SenderError> {
    use unicode_normalization::UnicodeNormalization;
    if chunk_raw_size == 0 {
        return Err(SenderError::Config("chunk_raw_size must be > 0".into()));
    }
    let mut ordered: Vec<(String, u32, u64)> = metas
        .iter()
        .enumerate()
        .filter(|(_, (kind, _, _))| *kind != crate::id::KIND_DIRECTORY)
        .map(|(i, (_, path, size))| (path.nfc().collect::<String>(), i as u32, *size))
        .collect();
    ordered.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let total: u64 = ordered.iter().map(|(_, _, s)| s).sum();
    if total == 0 {
        return Err(SenderError::EmptyContent);
    }
    let chunk = u64::from(chunk_raw_size);
    let mut out: Vec<Vec<ChunkSegment>> = Vec::new();
    let mut cur: Vec<ChunkSegment> = Vec::new();
    let mut cur_len: u64 = 0;
    for (_, item, size) in ordered {
        let mut pos: u64 = 0;
        while pos < size {
            let take = (chunk - cur_len).min(size - pos);
            cur.push(ChunkSegment {
                item,
                start: pos,
                len: take,
            });
            cur_len += take;
            pos += take;
            if cur_len == chunk {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{KIND_FILE, KIND_UTF8_TEXT};
    use crate::receiver::{Af2Receiver, IngestEvent};

    #[test]
    fn multi_chunk_transfer_completes_after_epoch1_window_loss() {
        // Regression: the reported "63573/46016, never completes" multi-item
        // bundle stall. Root cause: the receiver keeps ONE chunk decoder and
        // drops it (zero symbol cache) when the next chunk's META arrives;
        // pre-fix, epoch ≥ 2 windows carried only `redundancy` fresh repair
        // symbols, so any chunk that missed more than the redundancy budget in
        // its epoch-1 window could never gather K symbols again in any single
        // later window — received_symbols kept climbing past total_symbols
        // forever while the transfer never completed.
        // Content must be INCOMPRESSIBLE (pseudorandom): compressible filler
        // collapses each chunk's encoded size to a handful of symbols, which
        // any epoch can trivially re-collect — real media files do not.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next_bytes = |n: usize| {
            let mut v = Vec::with_capacity(n);
            while v.len() < n {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                v.extend_from_slice(&seed.to_le_bytes());
            }
            v.truncate(n);
            v
        };
        let items = vec![
            (KIND_FILE, "a.bin".to_string(), next_bytes(1_200_000)),
            (KIND_FILE, "b.bin".to_string(), next_bytes(1_200_000)),
            (KIND_FILE, "c.bin".to_string(), next_bytes(1_200_000)),
        ];
        let config = SenderConfig {
            symbol_size: 2400,
            chunk_raw_size: 1 << 20,
            redundancy_pct: 5,
        };
        let mut sender = Af2Sender::new(items, config).unwrap();
        let mut receiver = Af2Receiver::new();
        let mut chunk_count = 0usize;
        let mut ready_chunks = std::collections::HashSet::new();
        // Burst-loss window: drop EVERY symbol frame from the moment chunk 2's
        // META binds until a chunk OUTSIDE {2, 3} is announced — a single
        // camera-miss event spans two adjacent chunk windows. TWO starved
        // chunks is the real deadlock shape: with one missing chunk the
        // receiver's decoder survives into the next epoch (done chunks' METAs
        // early-return and never replace it) and slowly re-collects, but two
        // incomplete chunks destroy each other's decoder at every window
        // boundary — each restarts from zero symbols forever.
        let mut drop_window = false;
        let mut burst_armed = false;
        for _ in 0..12_000 {
            let frame = sender.next_frame().unwrap();
            if drop_window {
                let parsed = Af2Frame::from_bytes(&frame).unwrap();
                if parsed.frame_type == FrameType::ObjectMeta {
                    if let Ok(rec) = ObjectMetaRecord::parse(&parsed.body) {
                        if rec.role == ROLE_CHUNK
                            && rec.object_index != 2
                            && rec.object_index != 3
                        {
                            drop_window = false;
                        }
                    }
                } else if parsed.frame_type == FrameType::Symbol {
                    continue;
                }
            }
            match receiver.ingest(&frame).unwrap() {
                IngestEvent::ManifestReady => {
                    chunk_count = receiver.manifest().unwrap().chunk_count as usize;
                }
                IngestEvent::MetaBound { role, object_index } if role == ROLE_CHUNK => {
                    if object_index == 2 && !burst_armed {
                        burst_armed = true;
                        drop_window = true;
                    }
                }
                IngestEvent::ChunkReady { index, .. } => {
                    ready_chunks.insert(index as usize);
                }
                _ => {}
            }
            if chunk_count > 0 && ready_chunks.len() == chunk_count {
                break;
            }
        }
        assert!(chunk_count > 3, "scenario must span at least 4 chunks");
        assert!(
            ready_chunks.contains(&2) && ready_chunks.contains(&3),
            "the burst-loss chunks must eventually complete (ready: {:?}/{chunk_count})",
            {
                let mut v: Vec<_> = ready_chunks.into_iter().collect();
                v.sort_unstable();
                v
            }
        );
    }

    #[test]
    fn streamed_sender_matches_buffered_frames() {
        // The streamed (bounded-memory) construction must be wire-identical
        // to the buffered one: same manifest, same chunk metas, same symbol
        // frames — including across an epoch boundary where every chunk is
        // retired and re-staged from scratch.
        let mut seed = 0x0DDB_EA07_5C0F_1A33u64;
        let mut next_bytes = |n: usize| {
            let mut v = Vec::with_capacity(n);
            while v.len() < n {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                v.extend_from_slice(&seed.to_le_bytes());
            }
            v.truncate(n);
            v
        };
        let items = vec![
            (KIND_FILE, "a.bin".to_string(), next_bytes(300_000)),
            (KIND_UTF8_TEXT, "b.txt".to_string(), b"streamed text entry".to_vec()),
        ];
        let config = SenderConfig {
            symbol_size: 512,
            chunk_raw_size: 1 << 20,
            redundancy_pct: 5,
        };
        let mut buffered = Af2Sender::new(items.clone(), config.clone()).unwrap();
        let (manifest, _) = crate::manifest::Manifest::parse(buffered.manifest_bytes()).unwrap();
        let mut streamed = Af2Sender::from_manifest_streamed(manifest, config).unwrap();

        // Canonical stream for RAW staging, assembled exactly like the
        // manifest orders it (NFC path bytes, non-directory entries only).
        let mut sorted = items;
        sorted.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
        let mut stream = Vec::new();
        for (kind, _, content) in &sorted {
            if *kind != crate::id::KIND_DIRECTORY {
                stream.extend_from_slice(content);
            }
        }
        let crs = 1usize << 20;
        let stage = |s: &mut Af2Sender, index: u32| {
            let start = index as usize * crs;
            let end = ((index as usize + 1) * crs).min(stream.len());
            s.stage_chunk(index, CODEC_RAW, stream[start..end].to_vec())
                .unwrap();
        };
        let next = |s: &mut Af2Sender| -> Vec<u8> {
            loop {
                match s.next_frame() {
                    Ok(f) => return f,
                    Err(SenderError::ChunkNotStaged(i)) => stage(s, i),
                    Err(e) => panic!("unexpected sender error: {e}"),
                }
            }
        };
        for frame_no in 0..4_000 {
            assert_eq!(
                buffered.next_frame().unwrap(),
                next(&mut streamed),
                "streamed frame {frame_no} differs from buffered"
            );
        }
    }

    #[test]
    fn streamed_sender_fails_closed_on_unstaged_and_bad_hashes() {
        let items = vec![(KIND_FILE, "a.bin".to_string(), vec![0x5Au8; 4096])];
        let config = SenderConfig {
            symbol_size: 512,
            ..SenderConfig::default()
        };
        let buffered = Af2Sender::new(items, config.clone()).unwrap();
        let (manifest, _) = crate::manifest::Manifest::parse(buffered.manifest_bytes()).unwrap();
        let mut streamed = Af2Sender::from_manifest_streamed(manifest, config).unwrap();

        // Bootstrap (ROOT/MANIFEST frames) flows without any chunk staged…
        let mut saw_chunk_zero = false;
        for _ in 0..80 {
            match streamed.next_frame() {
                Err(SenderError::ChunkNotStaged(0)) => {
                    saw_chunk_zero = true;
                    break;
                }
                Ok(_) => {}
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert!(saw_chunk_zero, "must fail closed with ChunkNotStaged(0)");

        // …and staging WRONG bytes is rejected against the manifest table.
        let bad = vec![0u8; 4096];
        assert!(matches!(
            streamed.stage_chunk(0, CODEC_RAW, bad),
            Err(SenderError::Config(_))
        ));
        // Correct RAW bytes are accepted and playback proceeds — across an
        // epoch boundary (the tiny chunk's window is ~15 frames), which
        // retires the encoder and requires the documented stage-and-retry.
        streamed
            .stage_chunk(0, CODEC_RAW, vec![0x5Au8; 4096])
            .unwrap();
        for frame_no in 0..40 {
            loop {
                match streamed.next_frame() {
                    Ok(_) => break,
                    Err(SenderError::ChunkNotStaged(0)) => {
                        streamed
                            .stage_chunk(0, CODEC_RAW, vec![0x5Au8; 4096])
                            .unwrap()
                    }
                    Err(e) => panic!("frame {frame_no}: unexpected error {e}"),
                }
            }
        }
    }

    #[test]
    fn streamed_sender_proactive_staging_never_stalls() {
        // Mirrors the web ChunkStager exactly: stage the next chunk with
        // wraparound every tick, seed chunk 0 before bootstrap ends. A missed
        // prefetch surfaces as ChunkNotStaged — on the wire that is a
        // periodic playback stall (frozen QR frame, receiver rate drops to
        // zero until the stage completes). Zero stalls must hold for the
        // single-chunk case, whose EVERY window boundary is an epoch wrap,
        // and for multi-chunk transfers.
        let cases: &[usize] = &[1, 3]; // chunk counts (via content sizing)
        for &case in cases {
            // 3 × 900 KB at 1 MiB chunks ⇒ 3 chunks; 1 × 900 KB ⇒ 1 chunk.
            // Deliberately repetitive bytes (zstd collapses them) keep each
            // window to a handful of symbols, so the 6000-frame budget walks
            // through MANY window/epoch boundaries — exactly the transitions
            // a missed prefetch would stall on.
            let per = 900_000;
            let mut items = Vec::new();
            for i in 0..case {
                items.push((
                    KIND_FILE,
                    format!("f{i}.bin"),
                    (0..per).map(|j| ((i * 37 + j * 31) & 0xff) as u8).collect::<Vec<u8>>(),
                ));
            }
            let config = SenderConfig {
                symbol_size: 2400,
                chunk_raw_size: 1 << 20,
                redundancy_pct: 5,
            };
            let mut buffered = Af2Sender::new(items.clone(), config.clone()).unwrap();
            let (manifest, _) =
                crate::manifest::Manifest::parse(buffered.manifest_bytes()).unwrap();
            let chunk_count = manifest.chunk_count as usize;
            assert_eq!(chunk_count, case, "case setup must produce the wanted chunk count");
            let mut streamed = Af2Sender::from_manifest_streamed(manifest, config).unwrap();

            let mut sorted = items;
            sorted.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
            let mut stream = Vec::new();
            for (kind, _, content) in &sorted {
                if *kind != crate::id::KIND_DIRECTORY {
                    stream.extend_from_slice(content);
                }
            }
            let crs = 1usize << 20;
            let stage = |s: &mut Af2Sender, index: u32| {
                if index as usize >= chunk_count {
                    return; // mirrors the JS stager's out-of-range guard
                }
                let start = index as usize * crs;
                let end = ((index as usize + 1) * crs).min(stream.len());
                let _ = s.stage_chunk(index, CODEC_RAW, stream[start..end].to_vec());
            };

            let mut stalls = 0usize;
            let mut frames = 0usize;
            for _ in 0..6_000 {
                // ChunkStager.tick(): bootstrap seeds 0/1; each window tick
                // arms the WRAPPED next chunk (== current for single-chunk).
                match streamed.current_chunk_index() {
                    Some(cur) => stage(&mut streamed, (cur + 1) % chunk_count as u32),
                    None => {
                        stage(&mut streamed, 0);
                        stage(&mut streamed, 1);
                    }
                }
                match streamed.next_frame() {
                    Ok(_) => frames += 1,
                    Err(SenderError::ChunkNotStaged(_)) => stalls += 1,
                    Err(e) => panic!("case {case}: unexpected error {e}"),
                }
                let _ = buffered.next_frame().unwrap();
            }
            assert!(frames > 5_000, "case {case}: playback must progress");
            assert_eq!(
                stalls, 0,
                "case {case}: proactive staging must cover every window/epoch boundary"
            );
        }
    }

    #[test]
    fn streamed_sender_multi_frame_batches_match_buffered_across_epochs() {
        // Reproduces, at the core level, the exact host playback shape:
        // `next_qr_scratch(4)` pulls up to 4 frames per screen tick, keeps a
        // mid-batch ChunkNotStaged as a PARTIAL batch (frames already pulled
        // stay rendered; the marker defers to the next tick's first frame),
        // and the ChunkStager arms the WRAPPED next chunk every tick. Two
        // audit bugs are locked out by the sequence-equality + zero-stall
        // assertions:
        //   1. swallowing a partially-generated batch would consume fountain
        //      state (symbol_index / repair ESI) without displaying it — the
        //      streamed sequence would then skip frames vs the buffered one;
        //   2. a prefetch that misses the epoch wrap (or the single-chunk
        //      case, where every boundary IS a wrap) surfaces as a
        //      first-frame ChunkNotStaged — a visible playback stall.
        // Two host scenarios per shape:
        //  - proactive: the real ChunkStager arms the WRAPPED next chunk every
        //    tick — zero not-staged events are tolerated (each would be a
        //    visible playback freeze);
        //  - reactive: prefetch never lands in time (slow disk) and staging
        //    happens strictly on the not-staged marker — every window
        //    boundary walks the PARTIAL-BATCH path, and the sequence must
        //    still stay byte-identical (this is the scenario where the old
        //    "swallow the partial batch" bug dropped fountain frames).
        for chunk_count in [1usize, 2] {
            for proactive in [true, false] {
                let per = 900_000;
                let mut items = Vec::new();
                for i in 0..chunk_count {
                    items.push((
                        KIND_FILE,
                        format!("f{i}.bin"),
                        (0..per).map(|j| ((i * 37 + j * 31) & 0xff) as u8).collect::<Vec<u8>>(),
                    ));
                }
                let config = SenderConfig {
                    symbol_size: 2400,
                    chunk_raw_size: 1 << 20,
                    redundancy_pct: 5,
                };
                let mut buffered = Af2Sender::new(items.clone(), config.clone()).unwrap();
                let (manifest, _) =
                    crate::manifest::Manifest::parse(buffered.manifest_bytes()).unwrap();
                assert_eq!(manifest.chunk_count as usize, chunk_count, "case setup");
                let mut streamed = Af2Sender::from_manifest_streamed(manifest, config).unwrap();

                let mut sorted = items;
                sorted.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
                let mut stream = Vec::new();
                for (kind, _, content) in &sorted {
                    if *kind != crate::id::KIND_DIRECTORY {
                        stream.extend_from_slice(content);
                    }
                }
                let crs = 1usize << 20;
                // Stage FAITHFULLY: run the same lazy codec decision the
                // buffered sender's rebuild will make (encode_chunk), so the
                // rebuilt chunk META — codec_id, encoded_hash, OTI, K —
                // matches byte-for-byte.
                let stage = |s: &mut Af2Sender, index: u32| {
                    if index as usize >= chunk_count {
                        return;
                    }
                    let start = index as usize * crs;
                    let end = ((index as usize + 1) * crs).min(stream.len());
                    let (codec, encoded) = encode_chunk(&stream[start..end]);
                    let _ = s.stage_chunk(index, codec, encoded);
                };

                let mut buffered_seq: Vec<Vec<u8>> = Vec::new();
                let mut streamed_seq: Vec<Vec<u8>> = Vec::new();
                let mut verified = 0usize;
                let mut not_staged_events = 0usize;
                // 600 ticks × ≤4 frames ≈ 2400 frames; the compressible
                // content makes windows a handful of symbols, so this walks
                // dozens of epochs — far past the "3 consecutive epochs" bar.
                for _tick in 0..600 {
                    if proactive {
                        match streamed.current_chunk_index() {
                            Some(cur) => stage(&mut streamed, (cur + 1) % chunk_count as u32),
                            None => {
                                stage(&mut streamed, 0);
                                stage(&mut streamed, 1);
                            }
                        }
                    }
                    // next_qr_scratch(4) equivalent with the partial-batch
                    // rule: a mid-batch not-staged keeps the already-pulled
                    // frames and defers the marker to the next tick.
                    let mut produced = 0usize;
                    while produced < 4 {
                        match streamed.next_frame() {
                            Ok(f) => {
                                streamed_seq.push(f);
                                produced += 1;
                            }
                            Err(SenderError::ChunkNotStaged(i)) => {
                                not_staged_events += 1;
                                if produced == 0 {
                                    stage(&mut streamed, i);
                                    continue;
                                }
                                break; // partial batch: keep frames, defer marker
                            }
                            Err(e) => panic!("case {chunk_count}: unexpected error {e}"),
                        }
                    }
                    for _ in 0..4 {
                        buffered_seq.push(buffered.next_frame().unwrap());
                    }
                    // Incremental prefix check: streamed may lag by a partial
                    // batch but must never skip, duplicate or alter a frame.
                    assert!(
                        streamed_seq.len() <= buffered_seq.len(),
                        "case {chunk_count}/{proactive}: streamed ran ahead of the reference"
                    );
                    for (i, f) in streamed_seq[verified..].iter().enumerate() {
                        assert_eq!(
                            f, &buffered_seq[verified + i],
                            "case {chunk_count}/{proactive}: frame {} diverges from the buffered reference",
                            verified + i
                        );
                    }
                    verified = streamed_seq.len();
                }
                // Reactive staging pays a partial-batch + retry tick at every
                // window boundary, so its throughput floor is lower — the
                // sequence equality above is the real invariant.
                let floor = if proactive { 2_000 } else { 1_200 };
                assert!(
                    verified > floor,
                    "case {chunk_count}/{proactive}: playback must progress (verified {verified})"
                );
                if proactive {
                    assert_eq!(
                        not_staged_events, 0,
                        "case {chunk_count}: proactive arming must cover every window/epoch boundary"
                    );
                } else {
                    assert!(
                        not_staged_events > 0,
                        "case {chunk_count}: reactive scenario must actually exercise the partial-batch path"
                    );
                }
            }
        }
    }

    #[test]
    fn streamed_sender_end_to_end() {
        let items = vec![
            (KIND_FILE, "a.bin".to_string(), vec![0x77u8; 250_000]),
            (KIND_FILE, "b.bin".to_string(), b"second entry payload".to_vec()),
        ];
        let config = SenderConfig {
            symbol_size: 512,
            redundancy_pct: 5,
            ..SenderConfig::default()
        };
        let buffered = Af2Sender::new(items.clone(), config.clone()).unwrap();
        let (manifest, _) = crate::manifest::Manifest::parse(buffered.manifest_bytes()).unwrap();
        let chunk_count = manifest.chunk_count as usize;
        let mut streamed = Af2Sender::from_manifest_streamed(manifest, config).unwrap();

        let mut sorted = items;
        sorted.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
        let mut stream = Vec::new();
        for (kind, _, content) in &sorted {
            if *kind != crate::id::KIND_DIRECTORY {
                stream.extend_from_slice(content);
            }
        }
        let crs = 8 * 1024 * 1024usize;
        let stage = |s: &mut Af2Sender, index: u32| {
            let start = index as usize * crs;
            let end = ((index as usize + 1) * crs).min(stream.len());
            s.stage_chunk(index, CODEC_RAW, stream[start..end].to_vec())
                .unwrap();
        };

        let mut receiver = Af2Receiver::new();
        let mut ready = std::collections::HashSet::new();
        for _ in 0..8_000 {
            let frame = loop {
                match streamed.next_frame() {
                    Ok(f) => break f,
                    Err(SenderError::ChunkNotStaged(i)) => stage(&mut streamed, i),
                    Err(e) => panic!("unexpected sender error: {e}"),
                }
            };
            if let IngestEvent::ChunkReady { index, .. } = receiver.ingest(&frame).unwrap() {
                ready.insert(index);
                if ready.len() == chunk_count {
                    break;
                }
            }
        }
        assert_eq!(
            ready.len(),
            chunk_count,
            "streamed transfer must complete end-to-end"
        );
    }

    #[test]
    fn sender_receiver_end_to_end_playlist() {
        let items = vec![
            (
                KIND_UTF8_TEXT,
                "msg.txt".to_string(),
                b"Hello AF2 automatic stream!".to_vec(),
            ),
            (KIND_FILE, "binary.dat".to_string(), vec![0x42u8; 10000]),
        ];
        let mut sender = Af2Sender::new(items, SenderConfig::default()).unwrap();
        let mut receiver = Af2Receiver::new();

        let mut manifest_ready = false;
        let mut chunks_received = 0;

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

    #[test]
    fn from_manifest_rebuilds_byte_identical_stream() {
        // §9.3 resend cache: a sender rebuilt from its own cached manifest
        // bytes (skipping the whole hash pass) must emit a byte-for-byte
        // identical frame stream, transfer id and content id.
        let items = vec![
            (
                KIND_UTF8_TEXT,
                "msg.txt".to_string(),
                b"cached rebuild payload".to_vec(),
            ),
            (KIND_FILE, "data.bin".to_string(), vec![0x42u8; 7000]),
        ];
        let config = SenderConfig {
            symbol_size: 512,
            chunk_raw_size: 1 << 20,
            redundancy_pct: 25,
        };
        let mut full = Af2Sender::new(items.clone(), config.clone()).unwrap();
        let manifest_bytes = full.manifest_bytes().to_vec();
        // Rebuild from the cached manifest bytes only (the host re-reads the
        // content items, but no BLAKE3 pass runs).
        let (manifest, _) = crate::manifest::Manifest::parse(&manifest_bytes).unwrap();
        let mut cached = Af2Sender::from_manifest(manifest, items, config).unwrap();
        assert_eq!(cached.manifest_bytes(), manifest_bytes.as_slice());
        assert_eq!(cached.transfer_id(), full.transfer_id());
        assert_eq!(cached.content_id(), full.content_id());
        for _ in 0..300 {
            assert_eq!(
                full.next_frame().unwrap(),
                cached.next_frame().unwrap(),
                "cached rebuild must emit identical frames"
            );
        }
    }

    #[test]
    fn from_manifest_rejects_chunk_raw_size_mismatch() {
        let items = vec![(KIND_FILE, "a.bin".to_string(), vec![0x11u8; 4096])];
        let manifest = crate::manifest::build_manifest(
            vec![(KIND_FILE, "a.bin", &items[0].2[..])],
            1 << 20,
        )
        .unwrap();
        match Af2Sender::from_manifest(
            manifest,
            items,
            SenderConfig {
                chunk_raw_size: 2 << 20,
                ..SenderConfig::default()
            },
        ) {
            Ok(_) => panic!("chunk_raw_size mismatch must be rejected"),
            Err(e) => assert!(
                e.to_string().contains("does not match manifest"),
                "unexpected error: {e}"
            ),
        }
    }

    #[test]
    fn chunk_start_emits_root_then_meta() {
        // Verify §9.2 playlist: ChunkLoop starts with ROOT ×1 → CHUNK META ×2
        let items = vec![(
            KIND_UTF8_TEXT,
            "m.txt".to_string(),
            b"test playlist root intro".to_vec(),
        )];
        let mut sender = Af2Sender::new(items, SenderConfig::default()).unwrap();
        let mut frames = Vec::new();
        // Skip bootstrap (4 ROOT + 4 META + up to 32 Manifest symbols)
        for _ in 0..60 {
            let f = Af2Frame::from_bytes(&sender.next_frame().unwrap()).unwrap();
            frames.push(f.frame_type);
        }
        // There must be at least one Root before Chunk symbols
        assert!(frames.contains(&FrameType::Root));
        assert!(frames.contains(&FrameType::ObjectMeta));
    }

    #[test]
    fn nfd_filenames_transfer_correctly() {
        // macOS hands out NFD names ("e" + U+0301); build_manifest normalizes
        // to NFC. The stream assembly must match items through NFC, or every
        // chunk hash silently desyncs from the manifest table.
        let nfd = "cafe\u{0301}.txt".to_string();
        let content = b"nfd path content".to_vec();
        let mut sender = Af2Sender::new(
            vec![(KIND_FILE, nfd, content.clone())],
            SenderConfig::default(),
        )
        .unwrap();
        let manifest_path = String::from("caf\u{00e9}.txt"); // NFC
        let mut receiver = Af2Receiver::new();
        for _ in 0..500 {
            let f = sender.next_frame().unwrap();
            if let IngestEvent::ChunkReady { raw, .. } = receiver.ingest(&f).unwrap() {
                assert_eq!(raw, content);
                let m = receiver.manifest().expect("manifest decoded");
                assert_eq!(m.entries[0].path, manifest_path);
                assert!(receiver.verify_chunk(0, &raw));
                return;
            }
        }
        panic!("NFD-named transfer must complete");
    }

    #[test]
    fn late_joiner_eventually_gets_manifest() {
        // The bootstrap phase never recurs; the ~8-frame manifest interleave
        // must periodically re-emit the MANIFEST META so a receiver joining
        // mid-epoch can still build the manifest decoder and materialize
        // entry names.
        let mut sender = Af2Sender::new(
            vec![(KIND_FILE, "late.bin".to_string(), vec![0x11u8; 8192])],
            SenderConfig {
                symbol_size: 512,
                ..SenderConfig::default()
            },
        )
        .unwrap();
        for _ in 0..400 {
            let _ = sender.next_frame().unwrap(); // join late
        }
        let mut receiver = Af2Receiver::new();
        let mut manifest_ready = false;
        for _ in 0..3000 {
            let f = sender.next_frame().unwrap();
            if let IngestEvent::ManifestReady = receiver.ingest(&f).unwrap() {
                manifest_ready = true;
                break;
            }
        }
        assert!(
            manifest_ready,
            "late joiner must receive a recurring MANIFEST META + symbols"
        );
    }

    #[test]
    fn chunk_object_ids_stay_stable_across_epochs() {
        // Freeing a chunk encoder on transition and rebuilding it later must
        // reproduce the identical object_id (deterministic re-encode), or
        // receivers would treat the replay as a foreign instance.
        let mut sender = Af2Sender::new(
            vec![(KIND_FILE, "stable.bin".to_string(), vec![0x33u8; 4096])],
            SenderConfig {
                symbol_size: 512,
                redundancy_pct: 25,
                ..SenderConfig::default()
            },
        )
        .unwrap();
        let mut seen_chunk_oids: std::collections::HashSet<[u8; 16]> =
            std::collections::HashSet::new();
        let mut meta_count = 0;
        for _ in 0..3000 {
            let f = sender.next_frame().unwrap();
            let parsed = Af2Frame::from_bytes(&f).unwrap();
            if parsed.frame_type == FrameType::ObjectMeta {
                meta_count += 1;
                // Skip the manifest META (first four after bootstrap ROOTs)
                // by checking the body role byte (AFO2 offset 5: 1=MANIFEST).
                if parsed.body.get(5).copied() != Some(2) {
                    continue;
                }
                seen_chunk_oids.insert(parsed.object_id);
                if meta_count > 40 {
                    break;
                }
            }
        }
        assert!(
            seen_chunk_oids.len() == 1,
            "one chunk ⇒ exactly one chunk object_id across epochs, got {}",
            seen_chunk_oids.len()
        );
    }

    #[test]
    fn chunk_symbol_esis_never_repeat_across_epochs() {
        // §9.1/§9.2: source symbols are sent exactly once (epoch 1) and every
        // repair ESI is never re-issued — including across encoder
        // free/rebuild at epoch boundaries and chunk transitions.
        let mut sender = Af2Sender::new(
            vec![
                (KIND_FILE, "a.bin".to_string(), vec![0x41u8; 6144]),
                (KIND_FILE, "b.bin".to_string(), vec![0x42u8; 4096]),
            ],
            SenderConfig {
                symbol_size: 512,
                redundancy_pct: 25,
                ..SenderConfig::default()
            },
        )
        .unwrap();
        let mut seen: std::collections::HashSet<([u8; 16], u8, u32)> =
            std::collections::HashSet::new();
        let mut duplicates = 0usize;
        // Enough frames for several epochs (chunks are small).
        for _ in 0..4000 {
            let f = sender.next_frame().unwrap();
            let parsed = Af2Frame::from_bytes(&f).unwrap();
            if parsed.frame_type == FrameType::Symbol && !seen.insert((
                parsed.object_id,
                parsed.sbn,
                parsed.esi,
            )) {
                duplicates += 1;
            }
        }
        assert_eq!(duplicates, 0, "no symbol frame may ever repeat");
    }

    #[test]
    fn manifest_repair_esi_exhaustion_stops() {
        let mut sender = Af2Sender::new(
            vec![(KIND_FILE, "x.bin".to_string(), vec![0x55u8; 2048])],
            SenderConfig {
                symbol_size: 512,
                ..SenderConfig::default()
            },
        )
        .unwrap();
        sender.manifest_encoder.next_repair_esi = MAX_ESI;
        assert!(
            sender.get_manifest_interleave_frame().unwrap().is_none(),
            "§9.1: exhausted repair ESI space must stop, not wrap"
        );
    }

    #[test]
    fn chunk_repair_esi_exhaustion_advances_playlist() {
        // Exhausting chunk 0's repair ESI cursor mid-playlist: the sender must
        // stop emitting chunk-0 symbols (forever — the cursor persists across
        // rebuilds) and advance to the next chunk instead of re-issuing.
        let mut sender = Af2Sender::new(
            vec![
                (KIND_FILE, "a.bin".to_string(), vec![0x41u8; 700 << 10]),
                (KIND_FILE, "b.bin".to_string(), vec![0x42u8; 700 << 10]),
            ],
            SenderConfig {
                symbol_size: 512,
                chunk_raw_size: 1 << 20,
                redundancy_pct: 25,
            },
        )
        .unwrap();
        // Advance to chunk 0's phase (its encoder is built at chunk start and
        // spans the ROOT/META preamble plus the symbol pass).
        for _ in 0..64 {
            if sender.chunk_encoders[0].is_some() {
                break;
            }
            let _ = sender.next_frame().unwrap();
        }
        assert!(
            sender.chunk_encoders[0].is_some(),
            "chunk 0 encoder must be live in its symbol phase"
        );
        let chunk0_k = sender.chunk_encoders[0].as_ref().unwrap().source_symbol_count;
        if let Some(enc) = sender.chunk_encoders[0].as_mut() {
            enc.next_repair_esi = MAX_ESI;
        }
        sender.chunk_repair_esi[0] = MAX_ESI;
        let chunk0_oid = sender.chunk_encoders[0].as_ref().unwrap().object_id;
        let chunk1_oid = {
            sender.ensure_chunk_encoder(1).unwrap();
            sender.chunk_encoders[1].as_ref().unwrap().object_id
        };
        let mut chunk0_repairs = 0usize;
        let mut chunk1_symbols = 0usize;
        for _ in 0..600 {
            let f = sender.next_frame().unwrap();
            let parsed = Af2Frame::from_bytes(&f).unwrap();
            if parsed.frame_type == FrameType::Symbol {
                if parsed.object_id == chunk0_oid {
                    // Source symbols (esi < k) may still finish their epoch-1
                    // pass; no repair symbol may ever be issued after the
                    // cursor is exhausted.
                    if parsed.esi >= chunk0_k {
                        chunk0_repairs += 1;
                    }
                } else if parsed.object_id == chunk1_oid {
                    chunk1_symbols += 1;
                }
            }
        }
        assert_eq!(
            chunk0_repairs, 0,
            "exhausted chunk must never issue repair symbols (also across epochs)"
        );
        assert!(
            chunk1_symbols > 0,
            "playlist must advance to the next chunk after exhaustion"
        );
    }

    /// Host-side prep flow: plan_chunks → assemble → encode_chunk_balanced →
    /// new_with_preencoded. The layout must match the sender's own stream
    /// slicing and the receiver must complete the full transfer.
    #[test]
    fn preencoded_roundtrip_completes() {
        let text: Vec<u8> = {
            let paragraph = b"preencoded roundtrip payload; repetition compresses. ";
            let mut v = Vec::new();
            while v.len() < 900_000 {
                v.extend_from_slice(paragraph);
            }
            v
        };
        let items = vec![
            (KIND_FILE, "b.bin".to_string(), vec![0x33u8; 700_000]),
            (KIND_UTF8_TEXT, "a.txt".to_string(), text),
        ];
        let chunk_raw_size: u32 = 1 << 20;
        let config = SenderConfig {
            symbol_size: 512,
            chunk_raw_size,
            redundancy_pct: 25,
        };
        let metas: Vec<(u8, String, u64)> = items
            .iter()
            .map(|(k, p, c)| (*k, p.clone(), c.len() as u64))
            .collect();
        let plan = plan_chunks(&metas, chunk_raw_size).unwrap();
        assert_eq!(plan.len(), 2, "1.6 MiB stream → 2 chunks");
        let mut preencoded = Vec::new();
        for (index, segs) in plan.iter().enumerate() {
            let mut raw = Vec::new();
            for seg in segs {
                let (.., content) = &items[seg.item as usize];
                raw.extend_from_slice(&content[seg.start as usize..(seg.start + seg.len) as usize]);
            }
            let (codec, encoded) =
                crate::chunk::encode_chunk_balanced(&raw, 0, true);
            let pc = if codec == crate::meta::CODEC_RAW {
                PreencodedChunk::RawMarker
            } else {
                assert!(encoded.len() < raw.len(), "strictly-smaller invariant");
                PreencodedChunk::Encoded(codec, encoded)
            };
            preencoded.push((index as u32, pc));
        }
        let mut sender = Af2Sender::new_with_preencoded(items, config, preencoded).unwrap();
        let mut receiver = Af2Receiver::new();
        let mut manifest_ready = false;
        let mut ready = 0usize;
        for _ in 0..4000 {
            let frame = sender.next_frame().unwrap();
            match receiver.ingest(&frame).unwrap() {
                IngestEvent::ManifestReady => manifest_ready = true,
                IngestEvent::ChunkReady { .. } => ready += 1,
                _ => {}
            }
            if manifest_ready && ready == 2 {
                break;
            }
        }
        assert!(manifest_ready);
        assert_eq!(ready, 2, "both preencoded chunks must complete");
    }

    #[test]
    fn preencoded_rejects_invariant_violations() {
        let items = vec![(KIND_FILE, "a.bin".to_string(), vec![0x11u8; 4096])];
        let config = SenderConfig {
            symbol_size: 512,
            chunk_raw_size: 1 << 20,
            redundancy_pct: 25,
        };
        // Not strictly smaller.
        let raw_clone = items[0].2.clone();
        let err = Af2Sender::new_with_preencoded(
            items.clone(),
            config.clone(),
            vec![(0, PreencodedChunk::Encoded(crate::meta::CODEC_ZSTD, raw_clone))],
        );
        assert!(err.err().unwrap().to_string().contains("strictly-smaller"));
        // RAW bytes must use the marker, not carried bytes.
        let err = Af2Sender::new_with_preencoded(
            items.clone(),
            config.clone(),
            vec![(0, PreencodedChunk::Encoded(crate::meta::CODEC_RAW, vec![1, 2, 3]))],
        );
        assert!(err.err().unwrap().to_string().contains("RawMarker"));
        // Out-of-range index.
        let err = Af2Sender::new_with_preencoded(
            items.clone(),
            config,
            vec![(7, PreencodedChunk::RawMarker)],
        );
        assert!(err.err().unwrap().to_string().contains("out of range"));
        // Duplicate index.
        let config = SenderConfig {
            symbol_size: 512,
            chunk_raw_size: 1 << 20,
            redundancy_pct: 25,
        };
        let err = Af2Sender::new_with_preencoded(
            items,
            config,
            vec![(0, PreencodedChunk::RawMarker), (0, PreencodedChunk::RawMarker)],
        );
        assert!(err.err().unwrap().to_string().contains("twice"));
    }

    #[test]
    fn plan_chunks_mirrors_canonical_stream() {
        // NFD "é" must plan identically to NFC (build_manifest normalizes),
        // directories carry no stream bytes, and the assembled chunks equal
        // the NFC-path-ordered concatenation.
        let items = [
            (crate::id::KIND_DIRECTORY, "dir/".to_string(), Vec::new()),
            (
                KIND_UTF8_TEXT,
                "me\u{301}xico.txt".to_string(), // NFD; NFC sorts before b.bin
                b"nfd path normalizes".to_vec(),
            ),
            (KIND_FILE, "b.bin".to_string(), vec![0xAB; 30]),
        ];
        let metas: Vec<(u8, String, u64)> = items
            .iter()
            .map(|(k, p, c)| (*k, p.clone(), c.len() as u64))
            .collect();
        let chunk_raw_size = 1 << 20; // single chunk
        let plan = plan_chunks(&metas, chunk_raw_size).unwrap();
        assert_eq!(plan.len(), 1);
        let mut assembled = Vec::new();
        for seg in &plan[0] {
            let content = &items[seg.item as usize].2;
            assembled.extend_from_slice(&content[seg.start as usize..(seg.start + seg.len) as usize]);
        }
        // NFC-normalized "méxico.txt" ('m' = 0x6d) sorts AFTER "b.bin"
        // ('b' = 0x62) in path-byte order, so b.bin's bytes come first.
        let expected = [vec![0xAB; 30].as_slice(), b"nfd path normalizes".as_slice()].concat();
        assert_eq!(assembled, expected);
        // Empty content (all-directories) is unrepresentable, matching build.
        assert!(matches!(
            plan_chunks(
                &[(crate::id::KIND_DIRECTORY, "d/".to_string(), 0)],
                chunk_raw_size
            ),
            Err(SenderError::EmptyContent)
        ));
    }
}
